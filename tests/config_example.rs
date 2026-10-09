//! Guards the shipped example configuration (tasks.md T1.2 / T7.2).
//!
//! `config.example.toml` is what operators copy to `/etc/snore-monitor/config.toml`
//! and what T8.6 calibrates against. Parsing it here with the real loader means an
//! example that no longer satisfies the validator fails `cargo test` instead of
//! failing on a Raspberry Pi at night.

use snore_monitor::config::{BYTES_PER_SAMPLE, Config, DETECTION_RATE_HZ, SUPPORTED_CAPTURE_RATES};
use std::path::PathBuf;

fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.example.toml")
}

#[test]
fn the_example_configuration_parses_and_validates() {
    let path = example_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let config = Config::from_toml_str(&text)
        .unwrap_or_else(|error| panic!("{} no longer validates: {error}", path.display()));

    // Every documented default in TECH_SPEC §11 must actually be what the
    // example sets, so reading the example is a reliable way to learn them.
    assert_eq!(config.server.bind_address, "127.0.0.1");
    assert_eq!(config.server.port, 8080);
    assert_eq!(config.server.http_threads, 2);
    assert_eq!(config.server.timezone, "");

    assert_eq!(config.audio.alsa_device, "");
    assert_eq!(config.audio.preferred_rate_hz, 16_000);
    assert_eq!(config.audio.channels, 1);
    assert_eq!(config.audio.format.as_str(), "S16_LE");
    assert_eq!(config.audio.mono_channel_index, 0);
    assert_eq!(config.audio.segment_duration_seconds, 3_600);
    assert_eq!(config.audio.gap_tolerance_ms, 20);
    assert_eq!(config.audio.shutdown_timeout_seconds, 10);

    assert_eq!(config.detector.detection_rate_hz, DETECTION_RATE_HZ);
    assert_eq!(config.detector.frame_ms, 10);
    assert_eq!(config.detector.band_low_hz, 80.0);
    assert_eq!(config.detector.band_high_hz, 1_500.0);
    assert_eq!(config.detector.absolute_floor_dbfs, -50.0);
    assert_eq!(config.detector.noise_margin_db, 8.0);
    assert_eq!(config.detector.band_margin_db, 6.0);
    assert_eq!(config.detector.band_ratio_min, 0.35);
    assert_eq!(config.detector.attack_window_frames, 5);
    assert_eq!(config.detector.attack_min_hits, 3);
    assert_eq!(config.detector.hangover_frames, 50);
    assert_eq!(config.detector.min_event_ms, 200);
    assert_eq!(config.detector.merge_gap_ms, 800);
    assert_eq!(config.detector.max_event_seconds, 15);

    assert_eq!(config.storage.recording_max_bytes, 21_474_836_480);
    assert_eq!(
        config.storage.recording_cleanup_target_bytes,
        20_401_094_656
    );
    assert_eq!(config.storage.recording_reserve_bytes, 536_870_912);
    assert_eq!(config.storage.retention_check_interval_seconds, 60);
}

/// The example is what deployments copy, so its two paths must be absolute and
/// its segment must fit classic RIFF; both are checked explicitly as well as by
/// the validator, so a failure names the example rather than a bare field.
#[test]
fn the_example_configuration_is_safe_for_the_documented_deployment() {
    let path = example_path();
    let text = std::fs::read_to_string(&path).unwrap();
    let config = Config::from_toml_str(&text).unwrap();
    assert!(config.storage.recording_dir.is_absolute());
    assert!(config.storage.database_path.is_absolute());
    assert!(SUPPORTED_CAPTURE_RATES.contains(&config.audio.preferred_rate_hz));
    assert!(config.audio.mono_channel_index < config.audio.channels);
    assert!(config.detector.attack_min_hits <= config.detector.attack_window_frames);
    assert!(
        u64::from(config.detector.max_event_seconds) * 1000
            > u64::from(config.detector.min_event_ms)
    );
    assert!(config.audio.max_segment_bytes() < 4 * 1024 * 1024 * 1024);
    assert_eq!(BYTES_PER_SAMPLE, 2);
    assert!(config.audio.block_align() > 0);
}
