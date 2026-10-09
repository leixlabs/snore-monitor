//! Configuration loading and validation (TECH_SPEC §11, task T1.2).
//!
//! Validation never falls back to a different value: a bad field is reported by
//! name and startup fails. The only documented fallbacks live in the capture
//! device negotiation (TECH_SPEC §4.1), not here.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Default configuration path (TECH_SPEC §11).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/snore-monitor/config.toml";
/// Environment variable that overrides the configuration path.
pub const CONFIG_PATH_ENV: &str = "SNORE_MONITOR_CONFIG";

/// Capture rates the detection branch knows how to convert (TECH_SPEC §5.1).
pub const SUPPORTED_CAPTURE_RATES: [u32; 5] = [16_000, 32_000, 44_100, 48_000, 96_000];
/// The MVP detection rate. Any other configured value is a validation error.
pub const DETECTION_RATE_HZ: u32 = 16_000;
/// Classic RIFF/WAVE size ceiling; segments must stay below it (TECH_SPEC §4.2).
pub const RIFF_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Bytes per stored sample for the only supported capture format.
pub const BYTES_PER_SAMPLE: u64 = 2;

/// Field-level validation failures.
#[derive(Debug, thiserror::Error)]
#[error("invalid configuration:\n  {}", .0.join("\n  "))]
pub struct ConfigError(pub Vec<String>);

/// Why a configuration file could not be turned into a usable [`Config`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("cannot read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse configuration file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(transparent)]
    Invalid(#[from] ConfigError),
}

/// Sample format of the recorded stream. MVP supports `S16_LE` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SampleFormat {
    #[serde(rename = "S16_LE")]
    S16Le,
}

impl SampleFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            SampleFormat::S16Le => "S16_LE",
        }
    }

    pub fn bytes_per_sample(self) -> u64 {
        match self {
            SampleFormat::S16Le => BYTES_PER_SAMPLE,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub detector: DetectorConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// T0.1 decision: the shipped default binds to loopback. Deployments that
    /// need LAN access set this explicitly (see `config.example.toml`).
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_http_threads")]
    pub http_threads: usize,
    /// IANA timezone used to turn `?date=YYYY-MM-DD` into a UTC range.
    /// Empty means "use the host's local timezone" (T0.1 decision).
    #[serde(default)]
    pub timezone: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    /// ALSA card name. Empty means "pick the only USB capture device"; with
    /// more than one candidate present, startup fails instead of guessing.
    #[serde(default)]
    pub alsa_device: String,
    #[serde(default = "default_preferred_rate_hz")]
    pub preferred_rate_hz: u32,
    #[serde(default = "default_channels")]
    pub channels: u16,
    #[serde(default = "default_format")]
    pub format: SampleFormat,
    #[serde(default = "default_period_ms")]
    pub period_ms: u32,
    #[serde(default = "default_buffer_ms")]
    pub buffer_ms: u32,
    #[serde(default = "default_segment_duration_seconds")]
    pub segment_duration_seconds: u64,
    #[serde(default)]
    pub mono_channel_index: u16,
    #[serde(default = "default_capture_ring_ms")]
    pub capture_ring_ms: u32,
    #[serde(default = "default_recording_queue_ms")]
    pub recording_queue_ms: u32,
    #[serde(default = "default_detection_queue_ms")]
    pub detection_queue_ms: u32,
    #[serde(default = "default_gap_tolerance_ms")]
    pub gap_tolerance_ms: u32,
    #[serde(default = "default_gap_rotate_seconds")]
    pub gap_rotate_seconds: u64,
    #[serde(default = "default_header_flush_interval_seconds")]
    pub header_flush_interval_seconds: u64,
    #[serde(default = "default_shutdown_timeout_seconds")]
    pub shutdown_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectorConfig {
    #[serde(default = "default_detection_rate_hz")]
    pub detection_rate_hz: u32,
    #[serde(default = "default_frame_ms")]
    pub frame_ms: u32,
    #[serde(default = "default_band_low_hz")]
    pub band_low_hz: f32,
    #[serde(default = "default_band_high_hz")]
    pub band_high_hz: f32,
    #[serde(default = "default_absolute_floor_dbfs")]
    pub absolute_floor_dbfs: f32,
    #[serde(default = "default_noise_margin_db")]
    pub noise_margin_db: f32,
    #[serde(default = "default_band_margin_db")]
    pub band_margin_db: f32,
    #[serde(default = "default_band_ratio_min")]
    pub band_ratio_min: f32,
    #[serde(default = "default_noise_floor_window_seconds")]
    pub noise_floor_window_seconds: u32,
    #[serde(default = "default_noise_floor_percentile")]
    pub noise_floor_percentile: u32,
    #[serde(default = "default_noise_floor_min_seconds")]
    pub noise_floor_min_seconds: u32,
    #[serde(default = "default_noise_floor_reset_gap_seconds")]
    pub noise_floor_reset_gap_seconds: u64,
    #[serde(default = "default_noise_freeze_max_seconds")]
    pub noise_freeze_max_seconds: u32,
    #[serde(default = "default_filter_warmup_ms")]
    pub filter_warmup_ms: u32,
    #[serde(default = "default_attack_window_frames")]
    pub attack_window_frames: u32,
    #[serde(default = "default_attack_min_hits")]
    pub attack_min_hits: u32,
    #[serde(default = "default_hangover_frames")]
    pub hangover_frames: u32,
    #[serde(default = "default_min_event_ms")]
    pub min_event_ms: u32,
    #[serde(default = "default_merge_gap_ms")]
    pub merge_gap_ms: u32,
    #[serde(default = "default_max_event_seconds")]
    pub max_event_seconds: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub recording_dir: PathBuf,
    pub database_path: PathBuf,
    #[serde(default = "default_recording_max_bytes")]
    pub recording_max_bytes: u64,
    #[serde(default = "default_recording_cleanup_target_bytes")]
    pub recording_cleanup_target_bytes: u64,
    #[serde(default = "default_recording_reserve_bytes")]
    pub recording_reserve_bytes: u64,
    /// T0.1 decision: retention also runs on a timer, not only after a segment
    /// completes and at startup.
    #[serde(default = "default_retention_check_interval_seconds")]
    pub retention_check_interval_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind_address: default_bind_address(),
            port: default_port(),
            http_threads: default_http_threads(),
            timezone: String::new(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            alsa_device: String::new(),
            preferred_rate_hz: default_preferred_rate_hz(),
            channels: default_channels(),
            format: default_format(),
            period_ms: default_period_ms(),
            buffer_ms: default_buffer_ms(),
            segment_duration_seconds: default_segment_duration_seconds(),
            mono_channel_index: 0,
            capture_ring_ms: default_capture_ring_ms(),
            recording_queue_ms: default_recording_queue_ms(),
            detection_queue_ms: default_detection_queue_ms(),
            gap_tolerance_ms: default_gap_tolerance_ms(),
            gap_rotate_seconds: default_gap_rotate_seconds(),
            header_flush_interval_seconds: default_header_flush_interval_seconds(),
            shutdown_timeout_seconds: default_shutdown_timeout_seconds(),
        }
    }
}

impl Default for DetectorConfig {
    fn default() -> Self {
        DetectorConfig {
            detection_rate_hz: default_detection_rate_hz(),
            frame_ms: default_frame_ms(),
            band_low_hz: default_band_low_hz(),
            band_high_hz: default_band_high_hz(),
            absolute_floor_dbfs: default_absolute_floor_dbfs(),
            noise_margin_db: default_noise_margin_db(),
            band_margin_db: default_band_margin_db(),
            band_ratio_min: default_band_ratio_min(),
            noise_floor_window_seconds: default_noise_floor_window_seconds(),
            noise_floor_percentile: default_noise_floor_percentile(),
            noise_floor_min_seconds: default_noise_floor_min_seconds(),
            noise_floor_reset_gap_seconds: default_noise_floor_reset_gap_seconds(),
            noise_freeze_max_seconds: default_noise_freeze_max_seconds(),
            filter_warmup_ms: default_filter_warmup_ms(),
            attack_window_frames: default_attack_window_frames(),
            attack_min_hits: default_attack_min_hits(),
            hangover_frames: default_hangover_frames(),
            min_event_ms: default_min_event_ms(),
            merge_gap_ms: default_merge_gap_ms(),
            max_event_seconds: default_max_event_seconds(),
        }
    }
}

fn default_bind_address() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_http_threads() -> usize {
    2
}
fn default_preferred_rate_hz() -> u32 {
    16_000
}
fn default_channels() -> u16 {
    1
}
fn default_format() -> SampleFormat {
    SampleFormat::S16Le
}
fn default_period_ms() -> u32 {
    10
}
fn default_buffer_ms() -> u32 {
    200
}
fn default_segment_duration_seconds() -> u64 {
    3600
}
fn default_capture_ring_ms() -> u32 {
    500
}
fn default_recording_queue_ms() -> u32 {
    2000
}
fn default_detection_queue_ms() -> u32 {
    1000
}
fn default_gap_tolerance_ms() -> u32 {
    20
}
fn default_gap_rotate_seconds() -> u64 {
    5
}
fn default_header_flush_interval_seconds() -> u64 {
    30
}
fn default_shutdown_timeout_seconds() -> u64 {
    10
}
fn default_detection_rate_hz() -> u32 {
    DETECTION_RATE_HZ
}
fn default_frame_ms() -> u32 {
    10
}
fn default_band_low_hz() -> f32 {
    80.0
}
fn default_band_high_hz() -> f32 {
    1500.0
}
fn default_absolute_floor_dbfs() -> f32 {
    -50.0
}
fn default_noise_margin_db() -> f32 {
    8.0
}
fn default_band_margin_db() -> f32 {
    6.0
}
fn default_band_ratio_min() -> f32 {
    0.35
}
fn default_noise_floor_window_seconds() -> u32 {
    30
}
fn default_noise_floor_percentile() -> u32 {
    20
}
fn default_noise_floor_min_seconds() -> u32 {
    10
}
fn default_noise_floor_reset_gap_seconds() -> u64 {
    60
}
fn default_noise_freeze_max_seconds() -> u32 {
    30
}
fn default_filter_warmup_ms() -> u32 {
    100
}
fn default_attack_window_frames() -> u32 {
    5
}
fn default_attack_min_hits() -> u32 {
    3
}
fn default_hangover_frames() -> u32 {
    50
}
fn default_min_event_ms() -> u32 {
    200
}
fn default_merge_gap_ms() -> u32 {
    800
}
fn default_max_event_seconds() -> u32 {
    15
}
fn default_recording_max_bytes() -> u64 {
    20 * 1024 * 1024 * 1024
}
fn default_recording_cleanup_target_bytes() -> u64 {
    19 * 1024 * 1024 * 1024
}
fn default_recording_reserve_bytes() -> u64 {
    512 * 1024 * 1024
}
fn default_retention_check_interval_seconds() -> u64 {
    60
}

impl Config {
    /// Loads, parses and validates the configuration file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigLoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Config::from_toml_str(&text).map_err(|e| match e {
            ConfigLoadError::Parse { source, .. } => ConfigLoadError::Parse {
                path: path.to_path_buf(),
                source,
            },
            other => other,
        })
    }

    /// Parses and validates a TOML document that is already in memory.
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigLoadError> {
        let config: Config = toml::from_str(text).map_err(|source| ConfigLoadError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Path from `SNORE_MONITOR_CONFIG`, falling back to the packaged default.
    pub fn resolve_path() -> PathBuf {
        match std::env::var(CONFIG_PATH_ENV) {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
            _ => PathBuf::from(DEFAULT_CONFIG_PATH),
        }
    }

    /// Loads the configuration named by the environment, or the default path.
    pub fn load_from_env() -> Result<Config, ConfigLoadError> {
        Config::load(&Config::resolve_path())
    }

    /// Runs every check listed in TECH_SPEC §11 and returns all failures.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        self.server.validate(&mut errors);
        self.audio.validate(&mut errors);
        self.detector.validate(&mut errors);
        self.storage.validate(&mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError(errors))
        }
    }
}

impl ServerConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        if !(1..=8).contains(&self.http_threads) {
            errors.push(format!(
                "server.http_threads: must be within 1..=8, got {}",
                self.http_threads
            ));
        }
        if self.port == 0 {
            errors.push("server.port: must not be 0".to_string());
        }
        if self.bind_address.trim().is_empty() {
            errors.push("server.bind_address: must not be empty".to_string());
        } else if self.bind_address.parse::<std::net::IpAddr>().is_err() {
            errors.push(format!(
                "server.bind_address: must be an IP address, got {:?}",
                self.bind_address
            ));
        }
        if !self.timezone.is_empty() && self.timezone.parse::<chrono_tz::Tz>().is_err() {
            errors.push(format!(
                "server.timezone: must be an IANA timezone name or empty for the host timezone, got {:?}",
                self.timezone
            ));
        }
    }

    /// Resolved timezone, or `None` for "use the host's local time".
    pub fn timezone(&self) -> Option<chrono_tz::Tz> {
        if self.timezone.is_empty() {
            None
        } else {
            self.timezone.parse::<chrono_tz::Tz>().ok()
        }
    }
}

impl AudioConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        if !(1..=2).contains(&self.channels) {
            errors.push(format!(
                "audio.channels: MVP supports 1 or 2 channels, got {}",
                self.channels
            ));
        }
        if self.mono_channel_index >= self.channels {
            errors.push(format!(
                "audio.mono_channel_index: must be < audio.channels ({}), got {}",
                self.channels, self.mono_channel_index
            ));
        }
        if !SUPPORTED_CAPTURE_RATES.contains(&self.preferred_rate_hz) {
            errors.push(format!(
                "audio.preferred_rate_hz: must be one of {:?} (TECH_SPEC §5.1), got {}",
                SUPPORTED_CAPTURE_RATES, self.preferred_rate_hz
            ));
        }
        if self.period_ms == 0 {
            errors.push("audio.period_ms: must be > 0".to_string());
        }
        if self.buffer_ms == 0 {
            errors.push("audio.buffer_ms: must be > 0".to_string());
        }
        if self.gap_tolerance_ms == 0 {
            errors.push(
                "audio.gap_tolerance_ms: must be > 0, otherwise every block boundary registers a gap"
                    .to_string(),
            );
        }
        let two_periods = self.period_ms.saturating_mul(2);
        for (name, value) in [
            ("audio.capture_ring_ms", self.capture_ring_ms),
            ("audio.recording_queue_ms", self.recording_queue_ms),
            ("audio.detection_queue_ms", self.detection_queue_ms),
        ] {
            if value < two_periods {
                errors.push(format!(
                    "{name}: must be at least 2 periods ({two_periods} ms), got {value}"
                ));
            }
        }
        if self.segment_duration_seconds == 0 {
            errors.push("audio.segment_duration_seconds: must be > 0".to_string());
        }
        let segment_bytes = self.max_segment_bytes();
        if segment_bytes >= RIFF_MAX_BYTES {
            errors.push(format!(
                "audio.segment_duration_seconds: {} s at {} Hz x {} ch x 2 B is {} bytes, which does not fit classic RIFF (< {} bytes)",
                self.segment_duration_seconds,
                self.preferred_rate_hz,
                self.channels,
                segment_bytes,
                RIFF_MAX_BYTES
            ));
        }
        if self.header_flush_interval_seconds == 0 {
            errors.push("audio.header_flush_interval_seconds: must be > 0".to_string());
        }
        if self.shutdown_timeout_seconds == 0 {
            errors.push("audio.shutdown_timeout_seconds: must be > 0".to_string());
        }
    }

    /// Byte size of one full segment of raw PCM, before the WAV header.
    pub fn max_segment_bytes(&self) -> u64 {
        self.preferred_rate_hz as u64
            * self.channels as u64
            * self.format.bytes_per_sample()
            * self.segment_duration_seconds
    }

    /// Frame count that triggers a `duration` rotation (TECH_SPEC §4.3).
    pub fn segment_frames(&self) -> u64 {
        self.preferred_rate_hz as u64 * self.segment_duration_seconds
    }

    /// Frames held by the SPSC capture ring.
    pub fn capture_ring_frames(&self) -> usize {
        (self.preferred_rate_hz as u64 * self.capture_ring_ms as u64 / 1000) as usize
    }

    /// Bytes per frame of interleaved PCM as stored on disk.
    pub fn block_align(&self) -> u64 {
        self.channels as u64 * self.format.bytes_per_sample()
    }
}

impl DetectorConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        if self.detection_rate_hz != DETECTION_RATE_HZ {
            errors.push(format!(
                "detector.detection_rate_hz: MVP fixes this at {DETECTION_RATE_HZ}, got {}",
                self.detection_rate_hz
            ));
        }
        if self.frame_ms == 0 || !(self.detection_rate_hz * self.frame_ms).is_multiple_of(1000) {
            errors.push(format!(
                "detector.frame_ms: {} ms at {} Hz is not a whole number of samples",
                self.frame_ms, self.detection_rate_hz
            ));
        }
        if self.band_low_hz <= 0.0 || !self.band_low_hz.is_finite() {
            errors.push(format!(
                "detector.band_low_hz: must be a positive frequency, got {}",
                self.band_low_hz
            ));
        }
        let nyquist = self.detection_rate_hz as f32 / 2.0;
        if self.band_high_hz >= nyquist || !self.band_high_hz.is_finite() {
            errors.push(format!(
                "detector.band_high_hz: must stay below the {nyquist} Hz Nyquist limit, got {}",
                self.band_high_hz
            ));
        }
        if self.band_low_hz >= self.band_high_hz {
            errors.push(format!(
                "detector.band_low_hz: must be < detector.band_high_hz ({}), got {}",
                self.band_high_hz, self.band_low_hz
            ));
        }
        if !self.absolute_floor_dbfs.is_finite() || self.absolute_floor_dbfs > -1.0 {
            errors.push(format!(
                "detector.absolute_floor_dbfs: must be a finite dBFS value well below full scale, got {}",
                self.absolute_floor_dbfs
            ));
        }
        for (name, value) in [
            ("detector.noise_margin_db", self.noise_margin_db),
            ("detector.band_margin_db", self.band_margin_db),
        ] {
            if !value.is_finite() || value <= 0.0 {
                errors.push(format!("{name}: must be a positive number of dB, got {value}"));
            }
        }
        if !self.band_ratio_min.is_finite() || !(0.0..=1.0).contains(&self.band_ratio_min) {
            errors.push(format!(
                "detector.band_ratio_min: must be within 0.0..=1.0, got {}",
                self.band_ratio_min
            ));
        }
        if self.noise_floor_percentile < 1 || self.noise_floor_percentile > 50 {
            errors.push(format!(
                "detector.noise_floor_percentile: must be within 1..=50, got {}",
                self.noise_floor_percentile
            ));
        }
        if self.noise_floor_min_seconds > self.noise_floor_window_seconds {
            errors.push(format!(
                "detector.noise_floor_min_seconds: must be <= detector.noise_floor_window_seconds ({}), got {}",
                self.noise_floor_window_seconds, self.noise_floor_min_seconds
            ));
        }
        if self.noise_floor_window_seconds == 0 {
            errors.push("detector.noise_floor_window_seconds: must be > 0".to_string());
        }
        if self.attack_window_frames == 0 {
            errors.push("detector.attack_window_frames: must be > 0".to_string());
        }
        if self.attack_min_hits == 0 || self.attack_min_hits > self.attack_window_frames {
            errors.push(format!(
                "detector.attack_min_hits: must be within 1..=detector.attack_window_frames ({}), got {}",
                self.attack_window_frames, self.attack_min_hits
            ));
        }
        if self.hangover_frames == 0 {
            errors.push("detector.hangover_frames: must be >= 1".to_string());
        }
        if self.min_event_ms == 0 {
            errors.push("detector.min_event_ms: must be > 0".to_string());
        }
        if u64::from(self.max_event_seconds) * 1000 <= u64::from(self.min_event_ms) {
            errors.push(format!(
                "detector.max_event_seconds: {} s must be longer than detector.min_event_ms ({} ms)",
                self.max_event_seconds, self.min_event_ms
            ));
        }
        if self.filter_warmup_ms > 0 && self.frame_ms == 0 {
            errors.push("detector.filter_warmup_ms: cannot be applied without a frame length".to_string());
        }
    }

    /// Samples per detection frame.
    pub fn frame_samples(&self) -> usize {
        (self.detection_rate_hz * self.frame_ms / 1000) as usize
    }

    /// Frames skipped after a reset while the IIR chain settles (TECH_SPEC §5.3).
    pub fn warmup_frames(&self) -> u64 {
        u64::from(self.filter_warmup_ms).div_ceil(u64::from(self.frame_ms.max(1)))
    }

    /// Size of the noise-floor ring window in frames.
    pub fn noise_floor_window_frames(&self) -> usize {
        (self.noise_floor_window_seconds as usize) * 1000 / self.frame_ms.max(1) as usize
    }

    /// Minimum number of frames before the noise floor is considered known.
    pub fn noise_floor_min_frames(&self) -> usize {
        (self.noise_floor_min_seconds as usize) * 1000 / self.frame_ms.max(1) as usize
    }
}

impl StorageConfig {
    fn validate(&self, errors: &mut Vec<String>) {
        for (name, path) in [
            ("storage.recording_dir", &self.recording_dir),
            ("storage.database_path", &self.database_path),
        ] {
            if !path.is_absolute() {
                errors.push(format!(
                    "{name}: must be an absolute path, got {}",
                    path.display()
                ));
            }
        }
        if self.recording_max_bytes == 0 {
            errors.push("storage.recording_max_bytes: must be > 0".to_string());
        }
        if self.recording_cleanup_target_bytes >= self.recording_max_bytes {
            errors.push(format!(
                "storage.recording_cleanup_target_bytes: must be < storage.recording_max_bytes ({}), got {}",
                self.recording_max_bytes, self.recording_cleanup_target_bytes
            ));
        }
        if self.retention_check_interval_seconds == 0 {
            errors.push("storage.retention_check_interval_seconds: must be > 0".to_string());
        }
    }

    /// Total bytes a single recording may occupy, used by the 4 GiB guard.
    pub fn reserve_bytes(&self) -> u64 {
        self.recording_reserve_bytes
    }
}
