//! Recording capacity accounting and safe oldest-complete deletion (TECH_SPEC §6,
//! task T4.3).
//!
//! Only WAV files belonging to DB rows in `complete` state are candidates.
//! `.partial` files and in-progress segments are never deleted. Every candidate
//! path is normalized and checked to remain below the configured recordings root
//! before unlinking; event rows and user review labels are intentionally retained.

use crate::error::{Error, Result};
use crate::storage::{Database, SegmentRow, SegmentStatus};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    pub max_bytes: u64,
    pub cleanup_target_bytes: u64,
    pub reserve_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    pub measured_bytes: u64,
    pub removed_bytes: u64,
    pub deleted_ids: Vec<String>,
    pub failed_ids: Vec<String>,
    pub storage_pressure: bool,
    pub low_free_space: bool,
    pub should_stop_recording: bool,
}

#[derive(Debug, Clone)]
pub struct RetentionManager {
    root: PathBuf,
    config: RetentionConfig,
}

impl RetentionManager {
    pub fn new(root: impl Into<PathBuf>, config: RetentionConfig) -> Result<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(Error::Storage(format!("recording root must be absolute: {}", root.display())));
        }
        if config.cleanup_target_bytes >= config.max_bytes {
            return Err(Error::Storage("recording_cleanup_target_bytes must be below recording_max_bytes".into()));
        }
        Ok(RetentionManager { root, config })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Checks configured quota and optionally actual filesystem free space. The
    /// caller runs this at startup, after segment close and on the configured timer.
    pub fn enforce(&self, db: &Database, free_bytes: Option<u64>) -> Result<RetentionReport> {
        let mut measured = self.measure_recording_files()?;
        let mut removed = 0u64;
        let mut deleted_ids = Vec::new();
        let mut failed_ids = Vec::new();

        if measured > self.config.max_bytes {
            for segment in db.list_segments_for_retention()? {
                if measured <= self.config.cleanup_target_bytes {
                    break;
                }
                match self.delete_complete_segment(db, &segment) {
                    Ok(size) => {
                        removed = removed.saturating_add(size);
                        measured = measured.saturating_sub(size);
                        deleted_ids.push(segment.id);
                    }
                    Err(error) => {
                        tracing::warn!(segment_id = %segment.id, error = %error, "retention could not delete candidate; trying next complete segment");
                        failed_ids.push(segment.id);
                    }
                }
            }
        }

        let low_free_space = free_bytes.is_some_and(|free| free < self.config.reserve_bytes);
        let storage_pressure = measured > self.config.max_bytes;
        // No new audio should be written if reserve is breached and deletion did
        // not restore safe free space. Quota pressure alone remains visible and
        // prompts the caller to prevent further growth at the next rotation.
        let should_stop_recording = low_free_space && removed == 0;
        Ok(RetentionReport {
            measured_bytes: measured,
            removed_bytes: removed,
            deleted_ids,
            failed_ids,
            storage_pressure,
            low_free_space,
            should_stop_recording,
        })
    }

    /// Counts WAV and `.partial` files under the recordings tree; excludes DB and
    /// logs per TECH_SPEC §6. Symlinks are ignored instead of followed.
    pub fn measure_recording_files(&self) -> Result<u64> {
        if !self.root.exists() {
            return Ok(0);
        }
        let mut total = 0u64;
        let mut pending = vec![self.root.clone()];
        while let Some(dir) = pending.pop() {
            let entries = fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?;
            for entry in entries {
                let entry = entry.map_err(Error::from)?;
                let path = entry.path();
                let file_type = entry.file_type().map_err(|e| Error::io(&path, e))?;
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    pending.push(path);
                } else if file_type.is_file() && is_recording_file(&path) {
                    let len = entry.metadata().map_err(|e| Error::io(&path, e))?.len();
                    total = total.saturating_add(len);
                }
            }
        }
        Ok(total)
    }

    fn delete_complete_segment(&self, db: &Database, segment: &SegmentRow) -> Result<u64> {
        if segment.status != SegmentStatus::Complete.as_str() {
            return Err(Error::Storage(format!("refusing to delete non-complete segment {}", segment.id)));
        }
        let relative = Path::new(&segment.relative_path);
        validate_db_relative_path(relative)?;
        let canonical_root = self.root.canonicalize().map_err(|e| Error::io(&self.root, e))?;
        let candidate = self.root.join(relative);
        let canonical_file = candidate.canonicalize().map_err(|e| Error::io(&candidate, e))?;
        if !canonical_file.starts_with(&canonical_root) {
            return Err(Error::Storage(format!("recording path escapes configured root: {}", canonical_file.display())));
        }
        let database_path_matches = db
            .segment(&segment.id)?
            .is_some_and(|current| current.relative_path == segment.relative_path);
        if !database_path_matches {
            // DB state/path may have changed since the oldest-candidate query.
            return Err(Error::Storage(format!("segment {} no longer has matching DB identity", segment.id)));
        }
        let metadata = fs::metadata(&canonical_file).map_err(|e| Error::io(&canonical_file, e))?;
        if !metadata.is_file() || !is_recording_file(&canonical_file) {
            return Err(Error::Storage(format!("retention candidate is not a WAV file: {}", canonical_file.display())));
        }
        let size = metadata.len();
        fs::remove_file(&canonical_file).map_err(|e| Error::io(&canonical_file, e))?;
        db.mark_segment_status(&segment.id, SegmentStatus::Deleted)?;
        Ok(size)
    }
}

fn is_recording_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
    name.ends_with(".wav") || name.ends_with(".wav.partial")
}

/// Rejects absolute paths, parent traversal, dot segments, and empty DB paths.
pub fn validate_db_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(Error::Storage("recording DB path must be nonempty and relative".into()));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::Storage(format!("unsafe recording DB path: {}", path.display())));
        }
    }
    if !path.file_name().is_some_and(|name| name.to_string_lossy().ends_with(".wav")) {
        return Err(Error::Storage(format!("recording DB path must end in .wav: {}", path.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Database, SegmentStarted, SegmentCompleted, SegmentStatus};
    use crate::timeline::TimeQuality;
    use chrono::{TimeZone, Utc};
    use std::io::Write;
    use tempfile::tempdir;

    fn config() -> RetentionConfig {
        RetentionConfig { max_bytes: 1_000, cleanup_target_bytes: 500, reserve_bytes: 100 }
    }

    fn add_segment(db: &Database, id: &str, rel: &str, when: i64, size: u64) {
        let started = Utc.timestamp_opt(when, 0).single().unwrap();
        db.insert_segment_started(&SegmentStarted {
            id: id.into(), relative_path: PathBuf::from(rel), started_at_utc: started,
            capture_rate_hz: 16_000, channels: 1, sample_format: "S16_LE".into(),
            channel_mapping: "mono:0".into(), time_quality: TimeQuality::Synced, boot_id: "boot".into(),
        }).unwrap();
        db.complete_segment(&SegmentCompleted {
            id: id.into(), ended_at_utc: started + chrono::Duration::hours(1), frame_count: 0,
            size_bytes: size, end_reason: "duration".into(), clock_drift_ppm: None,
            xrun_count: 0, gap_count: 0, lost_frames_total: 0, status: SegmentStatus::Complete,
        }).unwrap();
    }

    fn make_file(root: &Path, rel: &str, bytes: usize) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(&vec![0; bytes]).unwrap();
    }

    #[test]
    fn deletes_oldest_complete_only_and_keeps_event_rows() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(&root).unwrap();
        let db = Database::in_memory().unwrap();
        add_segment(&db, "old", "2025/01/01/old.wav", 100, 300);
        add_segment(&db, "new", "2025/01/01/new.wav", 200, 300);
        add_segment(&db, "active", "2025/01/01/live.wav", 300, 500);
        db.connection().execute("UPDATE recording_segments SET status='recording' WHERE id='active'", []).unwrap();
        make_file(&root, "2025/01/01/old.wav", 300);
        make_file(&root, "2025/01/01/new.wav", 300);
        make_file(&root, "2025/01/01/live.wav.partial", 500);
        let mut cfg = config();
        cfg.max_bytes = 900;
        cfg.cleanup_target_bytes = 500;
        let manager = RetentionManager::new(&root, cfg).unwrap();
        let report = manager.enforce(&db, Some(5_000)).unwrap();
        assert_eq!(report.measured_bytes, 500);
        assert!(report.failed_ids.is_empty());
        assert_eq!(report.deleted_ids, vec!["old", "new"]);
        assert!(!root.join("2025/01/01/old.wav").exists());
        assert!(!root.join("2025/01/01/new.wav").exists());
        assert!(root.join("2025/01/01/live.wav.partial").exists());
        assert_eq!(db.segment("old").unwrap().unwrap().status, "deleted");
    }

    #[test]
    fn path_traversal_is_rejected() {
        assert!(validate_db_relative_path(Path::new("../outside.wav")).is_err());
        assert!(validate_db_relative_path(Path::new("/outside.wav")).is_err());
        assert!(validate_db_relative_path(Path::new("safe/file.wav")).is_ok());
    }

    #[test]
    fn partials_count_toward_quota_but_are_never_deleted() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(&root).unwrap();
        make_file(&root, "new.wav.partial", 1_200);
        let db = Database::in_memory().unwrap();
        let manager = RetentionManager::new(&root, config()).unwrap();
        let report = manager.enforce(&db, Some(5_000)).unwrap();
        assert_eq!(report.measured_bytes, 1_200);
        assert!(report.storage_pressure);
        assert!(root.join("new.wav.partial").exists());
    }

    #[test]
    fn low_free_space_with_no_deletion_requires_stop() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("recordings");
        fs::create_dir_all(&root).unwrap();
        let db = Database::in_memory().unwrap();
        let manager = RetentionManager::new(&root, config()).unwrap();
        let report = manager.enforce(&db, Some(10)).unwrap();
        assert!(report.low_free_space);
        assert!(report.should_stop_recording);
    }
}
