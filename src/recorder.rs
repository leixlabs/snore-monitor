//! Raw PCM WAV writer, recovery helpers and WAV metadata (TECH_SPEC §4.2,
//! tasks T3.5 and T3.6).
//!
//! The writer creates a `.partial`, appends little-endian S16 PCM, periodically
//! patches the RIFF/data lengths and syncs, then on close syncs and atomically
//! renames to `.wav`. It never filters or resamples the recording branch.

use crate::config::{AudioConfig, RIFF_MAX_BYTES};
use crate::error::{Error, Result};
use chrono::{DateTime, Datelike, Utc};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const WAV_HEADER_BYTES: u64 = 44;
const WRITE_BATCH_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct WavFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

impl WavFormat {
    pub fn new(sample_rate_hz: u32, channels: u16) -> Result<Self> {
        if sample_rate_hz == 0 || !(1..=2).contains(&channels) {
            return Err(Error::Storage(format!(
                "unsupported WAV format: rate={sample_rate_hz}, channels={channels}; MVP requires 1-2 channel S16 PCM"
            )));
        }
        Ok(WavFormat {
            sample_rate_hz,
            channels,
            bits_per_sample: 16,
        })
    }

    pub fn block_align(&self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }

    pub fn byte_rate(&self) -> u32 {
        self.sample_rate_hz * u32::from(self.block_align())
    }
}

#[derive(Debug, Clone)]
pub struct SegmentFile {
    pub id: String,
    pub relative_path: PathBuf,
    pub started_at_utc: DateTime<Utc>,
    pub format: WavFormat,
}

#[derive(Debug)]
pub struct WavRecorder {
    partial_path: PathBuf,
    final_path: PathBuf,
    file: File,
    format: WavFormat,
    data_bytes: u64,
    max_segment_bytes: u64,
    flush_interval: Duration,
    last_flush: Instant,
    closed: bool,
}

impl WavRecorder {
    /// Opens a new `.partial` file beneath the recordings root.
    pub fn create(
        recording_root: &Path,
        relative_path: &Path,
        format: WavFormat,
        max_segment_bytes: u64,
        header_flush_interval: Duration,
    ) -> Result<Self> {
        validate_relative_path(relative_path)?;
        if max_segment_bytes + WAV_HEADER_BYTES >= RIFF_MAX_BYTES {
            return Err(Error::Storage("configured segment limit does not fit classic RIFF".into()));
        }
        let final_path = recording_root.join(relative_path);
        let parent = final_path.parent().ok_or_else(|| Error::Storage("WAV path has no parent directory".into()))?;
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        let mut partial_path = final_path.clone();
        let file_name = final_path
            .file_name()
            .ok_or_else(|| Error::Storage("WAV path has no file name".into()))?
            .to_string_lossy();
        partial_path.set_file_name(format!("{file_name}.partial"));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&partial_path)
            .map_err(|e| Error::io(&partial_path, e))?;
        write_header(&mut file, &format, 0).map_err(|e| Error::io(&partial_path, e))?;
        file.flush().map_err(|e| Error::io(&partial_path, e))?;
        Ok(WavRecorder {
            partial_path,
            final_path,
            file,
            format,
            data_bytes: 0,
            max_segment_bytes,
            flush_interval: header_flush_interval,
            last_flush: Instant::now(),
            closed: false,
        })
    }

    pub fn data_bytes(&self) -> u64 {
        self.data_bytes
    }

    pub fn frame_count(&self) -> u64 {
        self.data_bytes / u64::from(self.format.block_align())
    }

    pub fn partial_path(&self) -> &Path {
        &self.partial_path
    }

    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// Appends interleaved S16 frames in bounded batches. Returns the accepted
    /// frame count; `Err` means the caller must rotate before writing more data.
    pub fn append_s16(&mut self, interleaved: &[i16]) -> Result<u64> {
        if self.closed {
            return Err(Error::Storage("cannot append to a closed WAV segment".into()));
        }
        if !interleaved.len().is_multiple_of(self.format.channels as usize) {
            return Err(Error::Storage(format!(
                "{} samples are not divisible by {} channels",
                interleaved.len(), self.format.channels
            )));
        }
        let frame_count = interleaved.len() as u64 / u64::from(self.format.channels);
        let bytes = frame_count * u64::from(self.format.block_align());
        if self.data_bytes.saturating_add(bytes) > self.max_segment_bytes
            || self.data_bytes.saturating_add(bytes) + WAV_HEADER_BYTES >= RIFF_MAX_BYTES
        {
            return Err(Error::Storage("segment size limit reached; rotate before writing this block".into()));
        }

        let samples_per_batch = (WRITE_BATCH_BYTES / std::mem::size_of::<i16>()).max(1);
        let mut scratch = Vec::with_capacity(WRITE_BATCH_BYTES);
        for chunk in interleaved.chunks(samples_per_batch) {
            scratch.clear();
            for sample in chunk {
                scratch.extend_from_slice(&sample.to_le_bytes());
            }
            self.file.write_all(&scratch).map_err(|e| Error::io(&self.partial_path, e))?;
        }
        self.data_bytes += bytes;
        if self.last_flush.elapsed() >= self.flush_interval {
            self.flush_header_and_sync()?;
        }
        Ok(frame_count)
    }

    /// Patches both RIFF lengths and calls `sync_data` (fdatasync on Unix).
    pub fn flush_header_and_sync(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        write_header(&mut self.file, &self.format, self.data_bytes)
            .map_err(|e| Error::io(&self.partial_path, e))?;
        self.file.sync_data().map_err(|e| Error::io(&self.partial_path, e))?;
        self.last_flush = Instant::now();
        Ok(())
    }

    /// Finishes a segment: header -> fdatasync -> atomic rename. DB completion is
    /// performed by the caller only after this returns successfully.
    pub fn close(mut self) -> Result<(PathBuf, u64, u64)> {
        self.flush_header_and_sync()?;
        self.file.flush().map_err(|e| Error::io(&self.partial_path, e))?;
        self.file.flush().map_err(|e| Error::io(&self.partial_path, e))?;
        fs::rename(&self.partial_path, &self.final_path).map_err(|e| Error::io(&self.final_path, e))?;
        if let Some(parent) = self.final_path.parent() {
            sync_directory(parent)?;
        }
        self.closed = true;
        Ok((self.final_path.clone(), self.frame_count(), self.data_bytes + WAV_HEADER_BYTES))
    }

    /// Leaves `.partial` in place for the startup recovery scan.
    pub fn abandon(self) -> PathBuf {
        self.partial_path.clone()
    }
}

impl Drop for WavRecorder {
    fn drop(&mut self) {
        // Do not rename or complete from Drop. A crash/timeout leaves `.partial`
        // for the conservative startup scan, and therefore is never mislabeled.
        if !self.closed {
            let _ = self.file.flush();
        }
    }
}

/// Constructs `recordings/YYYY/MM/DD/<UTC-start>_<id>.wav` and the matching DB
/// relative path.
pub fn segment_relative_path(started_at: DateTime<Utc>, id: &str) -> PathBuf {
    PathBuf::from(format!(
        "recordings/{:04}/{:02}/{:02}/{}_{}.wav",
        started_at.year(),
        started_at.month(),
        started_at.day(),
        started_at.format("%Y%m%dT%H%M%S%.3fZ"),
        id
    ))
}

/// Runs all path components through a strict relative-path check. The DB path is
/// the only source of a playback path; request strings never get joined here.
pub fn validate_relative_path(path: &Path) -> Result<()> {
    if path.is_absolute() || path.as_os_str().is_empty() {
        return Err(Error::Storage(format!("recording path must be nonempty and relative: {}", path.display())));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(Error::Storage(format!("recording path contains a forbidden component: {}", path.display()))),
        }
    }
    if path.extension().is_none_or(|ext| ext != "wav") {
        return Err(Error::Storage("recording path must end in .wav".into()));
    }
    Ok(())
}

/// Writes a canonical 44-byte little-endian PCM WAV header.
pub fn write_header(file: &mut File, format: &WavFormat, data_bytes: u64) -> std::io::Result<()> {
    let data_bytes = u32::try_from(data_bytes).map_err(|_| std::io::Error::other("classic WAV data exceeds u32"))?;
    let riff_size = data_bytes.checked_add(36).ok_or_else(|| std::io::Error::other("RIFF size overflow"))?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?; // WAVE_FORMAT_PCM
    file.write_all(&format.channels.to_le_bytes())?;
    file.write_all(&format.sample_rate_hz.to_le_bytes())?;
    file.write_all(&format.byte_rate().to_le_bytes())?;
    file.write_all(&format.block_align().to_le_bytes())?;
    file.write_all(&format.bits_per_sample.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

/// Reads and validates a PCM WAV header, returning its format and declared data
/// length. The data may be shorter than declared when called during recovery.
pub fn read_header(path: &Path) -> Result<(WavFormat, u64)> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut header = [0u8; 44];
    file.read_exact(&mut header).map_err(|e| Error::io(path, e))?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" || &header[12..16] != b"fmt " || &header[36..40] != b"data" {
        return Err(Error::Storage(format!("{} has an invalid PCM WAV header", path.display())));
    }
    let format_tag = u16::from_le_bytes([header[20], header[21]]);
    let channels = u16::from_le_bytes([header[22], header[23]]);
    let rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let bits = u16::from_le_bytes([header[34], header[35]]);
    if format_tag != 1 || bits != 16 {
        return Err(Error::Storage(format!("{} is not WAVE_FORMAT_PCM S16_LE", path.display())));
    }
    let declared_bytes = u32::from_le_bytes([header[40], header[41], header[42], header[43]]) as u64;
    Ok((WavFormat::new(rate, channels)?, declared_bytes))
}

/// Repairs a `.partial` by truncating to a whole block-aligned frame, patches the
/// header, syncs, then atomically renames it to `.wav`. Caller marks it
/// `interrupted` in the database; it must not claim the final tail was complete.
pub fn recover_partial(partial: &Path) -> Result<(PathBuf, u64, u64, WavFormat)> {
    let (format, _declared) = read_header(partial)?;
    let actual = fs::metadata(partial).map_err(|e| Error::io(partial, e))?.len();
    let block_align = u64::from(format.block_align());
    let whole_data = actual.saturating_sub(WAV_HEADER_BYTES) / block_align * block_align;
    let final_path = partial.with_extension(""); // `name.wav.partial` -> `name.wav`
    let mut file = OpenOptions::new().read(true).write(true).open(partial).map_err(|e| Error::io(partial, e))?;
    file.set_len(WAV_HEADER_BYTES + whole_data).map_err(|e| Error::io(partial, e))?;
    write_header(&mut file, &format, whole_data).map_err(|e| Error::io(partial, e))?;
    file.sync_data().map_err(|e| Error::io(partial, e))?;
    drop(file);
    fs::rename(partial, &final_path).map_err(|e| Error::io(&final_path, e))?;
    if let Some(parent) = final_path.parent() {
        sync_directory(parent)?;
    }
    Ok((final_path, whole_data / block_align, whole_data + WAV_HEADER_BYTES, format))
}

fn sync_directory(path: &Path) -> Result<()> {
    let dir = File::open(path).map_err(|e| Error::io(path, e))?;
    dir.sync_all().map_err(|e| Error::io(path, e))?;
    Ok(())
}

/// Computes the 4 GiB-bounded payload budget for the selected capture format.
pub fn payload_limit(config: &AudioConfig) -> u64 {
    config.max_segment_bytes().min(RIFF_MAX_BYTES - WAV_HEADER_BYTES - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    #[test]
    fn wav_header_and_pcm_bytes_are_correct() {
        let dir = tempdir().unwrap();
        let rel = Path::new("recordings/2025/01/02/test.wav");
        let format = WavFormat::new(48_000, 2).unwrap();
        let mut recorder = WavRecorder::create(dir.path(), rel, format, 1_000_000, Duration::from_secs(30)).unwrap();
        recorder.append_s16(&[1, -2, i16::MIN, i16::MAX]).unwrap();
        let (path, frames, size) = recorder.close().unwrap();
        assert_eq!(frames, 2);
        assert_eq!(size, 52);
        let (read_format, data_bytes) = read_header(&path).unwrap();
        assert_eq!(read_format.sample_rate_hz, 48_000);
        assert_eq!(read_format.channels, 2);
        assert_eq!(data_bytes, 8);
        let mut file = File::open(path).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes[44..], &[1, 0, 254, 255, 0, 128, 255, 127]);
    }

    #[test]
    fn rotate_before_riff_limit_using_a_small_simulated_limit() {
        let dir = tempdir().unwrap();
        let format = WavFormat::new(16_000, 1).unwrap();
        let mut recorder = WavRecorder::create(dir.path(), Path::new("a.wav"), format, 8, Duration::from_secs(30)).unwrap();
        recorder.append_s16(&[1, 2, 3, 4]).unwrap();
        assert!(recorder.append_s16(&[5]).is_err());
        assert_eq!(recorder.frame_count(), 4);
    }

    #[test]
    fn recovery_truncates_partial_frame_and_marks_file_as_interrupted_candidate() {
        let dir = tempdir().unwrap();
        let format = WavFormat::new(16_000, 2).unwrap();
        let rel = Path::new("segment.wav");
        let mut recorder = WavRecorder::create(dir.path(), rel, format, 1_000, Duration::from_secs(30)).unwrap();
        recorder.append_s16(&[1, 2, 3, 4, 5, 6]).unwrap();
        let partial = recorder.abandon();
        {
            let mut file = OpenOptions::new().append(true).open(&partial).unwrap();
            file.write_all(&[7, 0, 8]).unwrap(); // trailing half-frame
        }
        let (recovered, frames, size, _) = recover_partial(&partial).unwrap();
        assert_eq!(frames, 3);
        assert_eq!(size, 56);
        assert_eq!(recovered.extension().unwrap(), "wav");
        assert_eq!(read_header(&recovered).unwrap().1, 12);
    }

    #[test]
    fn malformed_and_traversal_paths_are_rejected() {
        assert!(validate_relative_path(Path::new("../escape.wav")).is_err());
        assert!(validate_relative_path(Path::new("/tmp/absolute.wav")).is_err());
        assert!(validate_relative_path(Path::new("valid.wav")).is_ok());
    }

    #[test]
    fn generated_paths_are_utc_calendar_paths() {
        let start = DateTime::parse_from_rfc3339("2025-01-02T03:04:05.678Z").unwrap().to_utc();
        let path = segment_relative_path(start, "seg-id");
        assert_eq!(path, PathBuf::from("recordings/2025/01/02/20250102T030405.678Z_seg-id.wav"));
    }
}
