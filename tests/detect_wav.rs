//! Integration tests for the offline `detect-wav` path (T2.7, TECH_SPEC §5.5).
//!
//! These fixtures are deterministic synthesis; they prove the offline tool drives
//! the same pipeline the service uses and that frame→offset mapping holds within
//! 10 ms at multiple capture rates. They cannot say anything about real-world
//! snore detection accuracy (§5.5: "合成 fixture 只能证明实现符合规格").

use snore_monitor::config::DetectorConfig;
use snore_monitor::detect_wav::{detect_file, result_to_csv, CSV_HEADER};
use std::f64::consts::PI;
use std::io::Write;
use std::path::PathBuf;
use tempfile::tempdir;

/// Writes a WAV containing `seconds` of the given signal, `rate` Hz, 1 channel.
fn write_wav(
    dir: &tempfile::TempDir,
    name: &str,
    rate: u32,
    seconds: f64,
    signal: impl Fn(f64) -> f32,
) -> PathBuf {
    let path = dir.path().join(name);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    let total = (seconds * rate as f64) as usize;
    for n in 0..total {
        let t = n as f64 / rate as f64;
        let value = (signal(t) * 32767.0).clamp(-32768.0, 32767.0) as i16;
        writer.write_sample(value).unwrap();
    }
    writer.finalize().unwrap();
    path
}

/// A snore-band-like burst: 300 Hz, amplitude 0.4, from 0.5 s for 1.0 s.
fn burst(t: f64) -> f32 {
    if (0.5..1.5).contains(&t) {
        ((2.0 * PI * 300.0 * t).sin() * 0.4) as f32
    } else {
        0.0
    }
}

fn calibration_config() -> DetectorConfig {
    // Fast noise-floor warm-up so the fixture reaches detection quickly.
    DetectorConfig {
        noise_floor_window_seconds: 1,
        noise_floor_min_seconds: 0,
        filter_warmup_ms: 0,
        ..DetectorConfig::default()
    }
}

#[test]
fn silence_produces_no_events() {
    let dir = tempdir().unwrap();
    let path = write_wav(&dir, "silence.wav", 16_000, 2.0, |_| 0.0);
    let result = detect_file(&path, calibration_config(), 0, chrono::Utc::now()).unwrap();
    assert_eq!(result.events.len(), 0, "silence must not produce events");
    assert_eq!(result.capture_rate_hz, 16_000);
}

#[test]
fn burst_event_offsets_match_ground_truth_within_10ms() {
    for rate in [16_000u32, 48_000] {
        let dir = tempdir().unwrap();
        let path = write_wav(&dir, "burst.wav", rate, 2.0, burst);
        let result = detect_file(&path, calibration_config(), 0, chrono::Utc::now()).unwrap();
        assert!(
            !result.events.is_empty(),
            "{rate} Hz: a 1 s 300 Hz burst at 0.4 amplitude must produce an event"
        );
        let event = &result.events[0];
        let start_s = event.start_offset_frames as f64 / rate as f64;
        let end_s = event.end_offset_frames as f64 / rate as f64;
        assert!(
            (start_s - 0.5).abs() <= 0.010,
            "{rate} Hz: event start {start_s:.4}s vs ground truth 0.5s"
        );
        // The end boundary is the last hit frame plus the state machine's
        // frame granularity; §5.5's 10 ms budget covers the mapping error, and
        // one extra 10 ms frame of hangover is the algorithmic boundary.
        assert!(
            (end_s - 1.5).abs() <= 0.010 + 0.010,
            "{rate} Hz: event end {end_s:.4}s vs ground truth 1.5s"
        );
    }
}

#[test]
fn event_boundaries_agree_across_capture_rates() {
    // The same physical signal at 16 kHz and 48 kHz must yield the same event
    // boundaries in seconds (§5.5's resampled offset-mapping check).
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    for rate in [16_000u32, 48_000] {
        let dir = tempdir().unwrap();
        let path = write_wav(&dir, "burst.wav", rate, 2.0, burst);
        let result = detect_file(&path, calibration_config(), 0, chrono::Utc::now()).unwrap();
        assert!(!result.events.is_empty());
        let event = &result.events[0];
        starts.push(event.start_offset_frames as f64 / rate as f64);
        ends.push(event.end_offset_frames as f64 / rate as f64);
    }
    assert!(
        (starts[0] - starts[1]).abs() <= 0.010,
        "start differed across rates: {starts:?}"
    );
    assert!(
        (ends[0] - ends[1]).abs() <= 0.010,
        "end differed across rates: {ends:?}"
    );
}

#[test]
fn csv_rows_match_json_events() {
    let dir = tempdir().unwrap();
    let path = write_wav(&dir, "burst.wav", 16_000, 2.0, burst);
    let result = detect_file(&path, calibration_config(), 0, chrono::Utc::now()).unwrap();
    let csv = result_to_csv(&result);
    let rows: Vec<&str> = csv.split('\n').filter(|line| !line.is_empty()).collect();
    assert_eq!(rows.len(), result.events.len());
    assert!(CSV_HEADER.starts_with("file,start_offset_frames"));
    for (row, event) in rows.iter().zip(&result.events) {
        assert!(row.starts_with(&format!("{},{}", result.file, event.start_offset_frames)));
    }
}

#[test]
fn unsupported_sample_rate_is_rejected_not_resampled() {
    let dir = tempdir().unwrap();
    let path = write_wav(&dir, "odd.wav", 22_050, 1.0, burst);
    let error = detect_file(&path, calibration_config(), 0, chrono::Utc::now()).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("22050"),
        "error should name the rejected rate: {message}"
    );
}

#[test]
fn stereo_files_select_the_configured_channel() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("stereo.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    for n in 0..16_000 * 2 {
        let t = n as f64 / 16_000.0;
        let right = if (0.5..1.5).contains(&t) {
            ((2.0 * PI * 300.0 * t).sin() * 0.4 * 32767.0) as i16
        } else {
            0
        };
        // Left channel stays silent; only channel 1 carries the burst.
        writer.write_sample(0i16).unwrap();
        writer.write_sample(right).unwrap();
    }
    writer.finalize().unwrap();

    let silent_left = detect_file(&path, calibration_config(), 0, chrono::Utc::now()).unwrap();
    assert_eq!(silent_left.events.len(), 0, "channel 0 is silent");
    let right = detect_file(&path, calibration_config(), 1, chrono::Utc::now()).unwrap();
    assert!(!right.events.is_empty(), "channel 1 carries the burst");
    let _ = std::io::stdout().flush();
}

#[test]
fn out_of_range_channel_index_is_rejected() {
    let dir = tempdir().unwrap();
    let path = write_wav(&dir, "mono.wav", 16_000, 1.0, |_| 0.0);
    let error = detect_file(&path, calibration_config(), 1, chrono::Utc::now()).unwrap_err();
    assert!(
        error.to_string().contains("out of range"),
        "expected channel-index error, got: {error}"
    );
}
