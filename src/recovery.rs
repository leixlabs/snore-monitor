//! Startup recovery scan (TECH_SPEC §4.2, task T3.6).
//!
//! Runs before the capture pipeline starts, so it is the one place where a
//! `.partial` file and a segment row are known not to be receiving writes.
//! Three reconciliations happen here:
//!
//! 1. `.partial` files left by a crash or a shutdown timeout are truncated to a
//!    whole `block_align` frame, their header is patched to the real size, and
//!    they are renamed to `.wav` and recorded as `interrupted`.
//! 2. A `.partial` whose header cannot be validated is left exactly as it is:
//!    it is never published and an alert is raised, rather than guessing that
//!    its tail is complete.
//! 3. Files and rows are reconciled: a WAV with no DB row is an orphan, and a
//!    row whose file is gone is marked `missing` (T0.1).
//!
//! Capacity is checked by the caller afterwards (T3.6), once the scan has made
//! the on-disk inventory accurate.

use crate::error::{Error, Result};
use crate::recorder::{recover_partial, validate_relative_path};
use crate::storage::{Database, SegmentStarted, SegmentStatus};
use crate::timeline::TimeQuality;
use chrono::{DateTime, NaiveDateTime, Utc};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// A `.partial` that could not be validated. It stays on disk, unpublished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiablePartial {
    pub path: PathBuf,
    pub reason: String,
}

/// Summary of one startup scan, suitable for logging and for the status page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// `.partial` files repaired into `interrupted` segments.
    pub recovered: Vec<String>,
    /// `.partial` files left untouched because they could not be validated.
    pub unverifiable: Vec<UnverifiablePartial>,
    /// WAV files with no matching row. They are reported, never deleted here:
    /// deleting audio is retention's job, and only for rows it can identify.
    pub orphans: Vec<PathBuf>,
    /// Segment ids whose row promised audio that is no longer on disk.
    pub missing: Vec<String>,
    /// Rows still marked `recording` from a previous run. They cannot be
    /// resumed; they are either reclassified by step 1/3 or set `interrupted`.
    pub stale_recording: Vec<String>,
}

impl RecoveryReport {
    pub fn is_clean(&self) -> bool {
        self.unverifiable.is_empty() && self.orphans.is_empty() && self.missing.is_empty()
    }
}

/// Scans `recordings_root`, repairs partials and reconciles them with `db`.
///
/// The scan is deliberately conservative: audio is only ever published when its
/// header and length agree, and nothing is deleted.
pub fn scan(recordings_root: &Path, db: &Database) -> Result<RecoveryReport> {
    if !recordings_root.is_absolute() {
        return Err(Error::Storage(format!(
            "recordings root must be absolute: {}",
            recordings_root.display()
        )));
    }
    let mut report = RecoveryReport::default();
    repair_partials(recordings_root, db, &mut report)?;
    reconcile(recordings_root, db, &mut report)?;
    Ok(report)
}

/// Step 1/2: repair every `.partial` below the root.
fn repair_partials(
    recordings_root: &Path,
    db: &Database,
    report: &mut RecoveryReport,
) -> Result<()> {
    for partial in collect_with_extension(recordings_root, "partial")? {
        // `name.wav.partial` belongs to `name.wav`; a partial with any other
        // shape is not ours to guess about.
        if partial.extension().is_none_or(|ext| ext != "partial") {
            continue;
        }
        let Some(stem_is_wav) = partial
            .file_stem()
            .and_then(|s| Path::new(s).extension())
            .map(|e| e == "wav")
        else {
            continue;
        };
        if !stem_is_wav {
            report.unverifiable.push(UnverifiablePartial {
                path: partial.clone(),
                reason: "partial does not follow the <name>.wav.partial convention".into(),
            });
            continue;
        }
        match recover_partial(&partial) {
            Ok((final_path, frames, size_bytes, format)) => {
                let started_at = match segment_start_from_path(&partial) {
                    Some(started) => started,
                    None => {
                        // The file itself is now repaired and publishable, but we
                        // cannot name its start time, so we cannot describe it in
                        // the DB without inventing a timestamp.
                        let id = segment_id_from_path(&partial);
                        report.orphans.push(final_path);
                        tracing::warn!(file = %partial.display(), %id, "recovered WAV has no parseable start timestamp; left for manual review");
                        continue;
                    }
                };
                let id = segment_id_from_path(&partial);
                let relative = relative_to(recordings_root, &final_path)?;
                let ended_at = started_at
                    + chrono::Duration::from_std(frame_duration(frames, format.sample_rate_hz))
                        .unwrap_or_else(|_| chrono::Duration::zero());
                let segment = SegmentStarted {
                    id: id.clone(),
                    relative_path: relative,
                    started_at_utc: started_at,
                    capture_rate_hz: format.sample_rate_hz,
                    channels: format.channels,
                    sample_format: "S16_LE".into(),
                    channel_mapping: default_channel_mapping(format.channels),
                    time_quality: TimeQuality::Unsynced,
                    boot_id: String::new(),
                };
                db.insert_interrupted_recovery(&segment, frames, size_bytes, ended_at)?;
                report.recovered.push(id);
            }
            Err(error) => {
                report.unverifiable.push(UnverifiablePartial {
                    path: partial.clone(),
                    reason: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Step 3: reconcile rows and files, including rows a crash left `recording`.
fn reconcile(recordings_root: &Path, db: &Database, report: &mut RecoveryReport) -> Result<()> {
    // Keep both identities of each file: the absolute path (for reporting) and
    // the DB-relative path (for matching against rows).
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in collect_with_extension(recordings_root, "wav")? {
        let relative = relative_to(recordings_root, &path)?;
        files.push((path, relative));
    }
    let rows = db.all_segments()?;
    let mut claimed: HashSet<PathBuf> = HashSet::new();

    for row in rows {
        let relative = PathBuf::from(&row.relative_path);
        let on_disk = files.iter().any(|(_, stored)| *stored == relative);
        match row.status.as_str() {
            // A row still marked `recording` cannot be resumed: the writer that
            // owned it is gone. Its data is whatever reached the disk, so the
            // honest classification is `interrupted` (complete would claim the
            // tail was flushed, deleted would hide audio that still exists).
            "recording" => {
                report.stale_recording.push(row.id.clone());
                db.mark_segment_status(&row.id, SegmentStatus::Interrupted)?;
                if !on_disk {
                    report.missing.push(row.id.clone());
                } else {
                    claimed.insert(relative);
                }
            }
            "complete" | "interrupted" => {
                if on_disk {
                    claimed.insert(relative);
                } else {
                    // The row promises audio that is gone. `missing` is the one
                    // status the API and Portal both understand as "cannot play
                    // this, and it is not a retention deletion".
                    db.mark_segment_status(&row.id, SegmentStatus::Missing)?;
                    report.missing.push(row.id.clone());
                }
            }
            // `deleted` rows are expected to have no file; `missing` rows are
            // already in the state we would set them to.
            _ => {}
        }
    }

    for (path, stored) in files {
        if !claimed.contains(&stored) {
            report.orphans.push(path);
        }
    }
    Ok(())
}

/// Collects every file below `root` with the given extension.
fn collect_with_extension(root: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A per-day directory that is already gone is not an error worth
            // failing startup over; the scan is a best-effort reconciliation.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::io(&dir, error)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&dir, e))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| Error::io(&path, e))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == extension) {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Builds the DB-stored relative path for a file below the recordings root.
///
/// `recorder::segment_relative_path` writes `recordings/YYYY/MM/DD/<name>.wav`,
/// so the stored path is relative to the *parent* of the recordings root, and the
/// root's own directory name is the leading component. When the root is not named
/// `recordings`, the directory name is used as-is, which keeps the stored path a
/// faithful, traversable relative path either way.
fn relative_to(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path.strip_prefix(root).map_err(|_| {
        Error::Storage(format!(
            "{} is outside the recordings root {}",
            path.display(),
            root.display()
        ))
    })?;
    let root_name = root.file_name().ok_or_else(|| {
        Error::Storage(format!("recordings root has no name: {}", root.display()))
    })?;
    let stored = Path::new(root_name).join(relative);
    validate_relative_path(&stored)?;
    Ok(stored)
}

/// Parses the UTC start time embedded in `<YYYYMMDDTHHMMSS.mmmZ>_<id>.wav`.
fn segment_start_from_path(path: &Path) -> Option<DateTime<Utc>> {
    let name = path.file_stem()?.to_str()?;
    let (timestamp, _) = name.split_once('_')?;
    let naive = NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%S%.3fZ").ok()?;
    Some(naive.and_utc())
}

/// Recovers the segment id embedded in the file name. The file name is
/// `<UTC-start>_<id>.wav.partial`, so the id is everything after the first
/// underscore with the WAV extensions stripped. Falls back to the bare stem so a
/// repaired file still gets a stable identity.
fn segment_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| Path::new(s).file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    match stem.split_once('_') {
        Some((_, id)) if !id.is_empty() => id.to_string(),
        _ => stem,
    }
}

fn frame_duration(frames: u64, rate_hz: u32) -> std::time::Duration {
    if rate_hz == 0 {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_secs_f64(frames as f64 / f64::from(rate_hz))
}

fn default_channel_mapping(channels: u16) -> String {
    match channels {
        1 => "mono".into(),
        _ => "stereo".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::{WavFormat, read_header, write_header};
    use chrono::{NaiveDate, NaiveTime};
    use tempfile::tempdir;

    /// Writes a `.partial` holding `frames` whole mono S16 frames followed by a
    /// partial frame of `tail_bytes` bytes, with a header that declares the full
    /// (untruncated) length — exactly the state a crash leaves behind.
    fn write_partial(root: &Path, relative: &Path, frames: u64, tail_bytes: u64) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data_bytes = frames * 2 + tail_bytes;
        let bytes = vec![0u8; (44 + data_bytes) as usize];
        fs::write(&path, &bytes).unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        write_header(&mut file, &WavFormat::new(16_000, 1).unwrap(), data_bytes).unwrap();
        drop(file);
        path
    }

    #[test]
    fn truncated_partial_is_repaired_and_recorded_as_interrupted() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(&root).unwrap();
        let db = Database::in_memory().unwrap();
        // 2 whole mono S16 frames plus a trailing partial frame that must be cut.
        write_partial(
            &root,
            Path::new("2025/01/02/20250102T010203.000Z_seg1.wav.partial"),
            2,
            1,
        );

        let report = scan(&root, &db).unwrap();

        assert_eq!(report.recovered, vec!["seg1".to_string()]);
        assert!(
            !root
                .join("2025/01/02/20250102T010203.000Z_seg1.wav.partial")
                .exists()
        );
        let recovered = root.join("2025/01/02/20250102T010203.000Z_seg1.wav");
        assert!(recovered.exists());
        // The partial frame was cut, so the header now declares exactly 2 frames.
        let (format, declared) = read_header(&recovered).unwrap();
        assert_eq!(format.sample_rate_hz, 16_000);
        assert_eq!(declared, 4);
        let row = db.segment("seg1").unwrap().unwrap();
        assert_eq!(row.status, "interrupted");
        assert_eq!(row.frame_count, 2);
        // The stored path stays relative and keeps the recordings-root prefix.
        assert_eq!(
            row.relative_path,
            "recordings/2025/01/02/20250102T010203.000Z_seg1.wav"
        );
    }

    #[test]
    fn unreadable_partial_is_left_in_place_and_reported() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(root.join("2025/01/02")).unwrap();
        let db = Database::in_memory().unwrap();
        let partial = root.join("2025/01/02/20250102T010203.000Z_seg2.wav.partial");
        fs::write(
            &partial,
            b"not a wav header at all, but long enough to read 44 bytes!!",
        )
        .unwrap();

        let report = scan(&root, &db).unwrap();

        assert!(report.recovered.is_empty());
        assert_eq!(report.unverifiable.len(), 1);
        assert_eq!(report.unverifiable[0].path, partial);
        assert!(
            partial.exists(),
            "an unverifiable partial must never be renamed away"
        );
        assert!(
            db.segment("seg2").unwrap().is_none(),
            "nothing may be published for it"
        );
    }

    #[test]
    fn orphan_wav_and_missing_row_are_both_reported() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(root.join("2025/01/02")).unwrap();
        let db = Database::in_memory().unwrap();
        // A row whose file never existed.
        let started = DateTime::from_naive_utc_and_offset(
            NaiveDateTime::new(
                NaiveDate::from_ymd_opt(2025, 1, 2).unwrap(),
                NaiveTime::from_hms_opt(1, 2, 3).unwrap(),
            ),
            Utc,
        );
        let segment = SegmentStarted {
            id: "gone".into(),
            relative_path: PathBuf::from("recordings/2025/01/02/20250102T010203.000Z_gone.wav"),
            started_at_utc: started,
            capture_rate_hz: 16_000,
            channels: 1,
            sample_format: "S16_LE".into(),
            channel_mapping: "mono".into(),
            time_quality: TimeQuality::Synced,
            boot_id: "boot".into(),
        };
        db.insert_segment_started(&segment).unwrap();
        db.complete_segment(&crate::storage::SegmentCompleted {
            id: "gone".into(),
            ended_at_utc: started,
            frame_count: 10,
            size_bytes: 64,
            end_reason: "duration".into(),
            clock_drift_ppm: None,
            xrun_count: 0,
            gap_count: 0,
            lost_frames_total: 0,
            status: SegmentStatus::Complete,
        })
        .unwrap();
        // A file no row mentions.
        let orphan = root.join("2025/01/02/20250102T040506.000Z_orphan.wav");
        fs::write(&orphan, b"whatever").unwrap();

        let report = scan(&root, &db).unwrap();

        assert_eq!(report.missing, vec!["gone".to_string()]);
        assert_eq!(db.segment("gone").unwrap().unwrap().status, "missing");
        assert_eq!(report.orphans, vec![orphan]);
    }

    #[test]
    fn stale_recording_row_becomes_interrupted_and_is_not_deleted() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(root.join("2025/01/02")).unwrap();
        let db = Database::in_memory().unwrap();
        let started = DateTime::from_naive_utc_and_offset(
            NaiveDateTime::new(
                NaiveDate::from_ymd_opt(2025, 1, 2).unwrap(),
                NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            ),
            Utc,
        );
        let relative = PathBuf::from("recordings/2025/01/02/20250102T090000.000Z_live.wav");
        let segment = SegmentStarted {
            id: "live".into(),
            relative_path: relative.clone(),
            started_at_utc: started,
            capture_rate_hz: 48_000,
            channels: 2,
            sample_format: "S16_LE".into(),
            channel_mapping: "stereo".into(),
            time_quality: TimeQuality::Synced,
            boot_id: "boot".into(),
        };
        db.insert_segment_started(&segment).unwrap();
        // The stored path is root-relative (`recordings/...`), so the file on disk
        // sits at `<root>/2025/01/02/<name>.wav`.
        let on_disk = root.join(relative.strip_prefix("recordings").unwrap());
        fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        let mut file = fs::File::create(&on_disk).unwrap();
        write_header(&mut file, &WavFormat::new(48_000, 2).unwrap(), 0).unwrap();
        drop(file);

        let report = scan(&root, &db).unwrap();

        assert_eq!(report.stale_recording, vec!["live".to_string()]);
        assert_eq!(db.segment("live").unwrap().unwrap().status, "interrupted");
        assert!(report.orphans.is_empty(), "a live row still owns its file");
    }

    #[test]
    fn scan_is_clean_when_nothing_needs_repair() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(&root).unwrap();
        let db = Database::in_memory().unwrap();
        let report = scan(&root, &db).unwrap();
        assert!(report.is_clean());
    }
}
