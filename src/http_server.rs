//! HTTP API and static portal (TECH_SPEC §8, §9; tasks T5.1–T5.6).
//!
//! # Public interface
//!
//! ```ignore
//! pub struct HttpState { /* ... */ }
//!
//! impl HttpState {
//!     pub fn new(
//!         config: Config,
//!         database: Arc<Mutex<Database>>,
//!         metrics: Arc<Metrics>,
//!         runtime: Arc<RuntimeStatus>,
//!     ) -> Self;
//!     pub fn with_web_root(self, root: impl Into<PathBuf>) -> Self;
//! }
//!
//! pub fn router(state: Arc<HttpState>) -> axum::Router;
//! pub async fn serve(state: Arc<HttpState>) -> crate::error::Result<()>;
//! pub fn build_runtime(config: &ServerConfig) -> std::io::Result<tokio::runtime::Runtime>;
//! ```
//!
//! `main` wires capture/dispatch and HTTP together by constructing an
//! [`HttpState`] and either calling [`serve`] or handing [`router`] to its own
//! server. [`RuntimeStatus`] is the one piece of state the HTTP layer cannot
//! derive from [`Metrics`]: recording/microphone/time quality are known only to
//! the supervisor, so they are pushed in through that handle and default to
//! `unknown` (TECH_SPEC §12 forbids inventing an `ok`).
//!
//! # Design notes
//!
//! * `rusqlite::Connection` is `Send` but not `Sync`, so the shared database is
//!   an `Arc<Mutex<Database>>`. Every lock is taken and released inside a
//!   synchronous helper; no guard is ever held across an `.await`.
//! * Audio bytes are streamed from the file in [`AUDIO_CHUNK_BYTES`] chunks with
//!   `tokio::fs`, so a long playback request never loads a whole segment into
//!   memory and never touches an audio thread.
//! * The playback path comes only from the database `relative_path` joined to
//!   `storage.recording_dir`. Request data never contributes path components.
//! * The portal is read from `<storage.recording_dir>/../web`. That directory is
//!   resolved once at construction ([`HttpState::with_web_root`] overrides it)
//!   and every request path is confined to it after canonicalization.
//!
//! # Range requests
//!
//! A single `bytes=a-b`, `bytes=a-` or `bytes=-n` range is honoured. Anything
//! that is not a single well-formed range (multiple ranges, another unit, junk)
//! is ignored and answered with the full `200`, which RFC 9110 permits. A
//! well-formed range that selects no byte (start past the end, `-0`, any range
//! on an empty file) is answered with `416`.

use crate::config::{Config, ServerConfig};
use crate::error::{Error, Result};
use crate::metrics::Metrics;
use crate::recorder::validate_relative_path;
use crate::storage::{local_day_range, parse_date, Database, EventRow, ReviewStatus, SegmentRow};
use crate::timeline::{GapRecord, TimeQuality};
use axum::body::{Body, Bytes};
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// API prefix for every JSON endpoint (TECH_SPEC §8).
pub const API_PREFIX: &str = "/api/v1";
/// Fixed read size for streamed audio; a request never buffers more than this.
const AUDIO_CHUNK_BYTES: usize = 64 * 1024;
/// Widest `/days` window accepted, so one request cannot fan out without bound.
const MAX_DAYS_SPAN: i64 = 366;

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

/// The single error shape every endpoint uses: `{"error":{...}}`.
fn error_response(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    let body = ErrorResponse {
        error: ErrorDetail {
            code,
            message: message.into(),
        },
    };
    match serde_json::to_vec(&body) {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(_) => (status, [(header::CONTENT_TYPE, "text/plain")], code).into_response(),
    }
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to serialize API response");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "response serialization failed",
            )
        }
    }
}

fn internal_error(error: &Error) -> Response {
    tracing::error!(%error, "HTTP request failed");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", error.to_string())
}

// ---------------------------------------------------------------------------
// Supervisor-reported status
// ---------------------------------------------------------------------------

/// Recording health as reported by the supervisor (TECH_SPEC §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    Unknown,
    Ok,
    Degraded,
    Stopped,
}

impl RecordingState {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordingState::Unknown => "unknown",
            RecordingState::Ok => "ok",
            RecordingState::Degraded => "degraded",
            RecordingState::Stopped => "stopped",
        }
    }

    fn code(self) -> u8 {
        match self {
            RecordingState::Unknown => 0,
            RecordingState::Ok => 1,
            RecordingState::Degraded => 2,
            RecordingState::Stopped => 3,
        }
    }

    fn from_code(value: u8) -> Self {
        match value {
            1 => RecordingState::Ok,
            2 => RecordingState::Degraded,
            3 => RecordingState::Stopped,
            _ => RecordingState::Unknown,
        }
    }
}

/// Microphone health as reported by the supervisor (TECH_SPEC §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneState {
    Unknown,
    Ok,
    Disconnected,
}

impl MicrophoneState {
    pub fn as_str(self) -> &'static str {
        match self {
            MicrophoneState::Unknown => "unknown",
            MicrophoneState::Ok => "ok",
            MicrophoneState::Disconnected => "disconnected",
        }
    }

    fn code(self) -> u8 {
        match self {
            MicrophoneState::Unknown => 0,
            MicrophoneState::Ok => 1,
            MicrophoneState::Disconnected => 2,
        }
    }

    fn from_code(value: u8) -> Self {
        match value {
            1 => MicrophoneState::Ok,
            2 => MicrophoneState::Disconnected,
            _ => MicrophoneState::Unknown,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Reported {
    device: String,
    capture_rate_hz: u32,
    channels: u16,
    sample_format: String,
    time_quality: Option<TimeQuality>,
    time_synced: bool,
    clock_drift_ppm: Option<f64>,
}

/// Live values `/status` needs that no other module can supply on its own.
///
/// Everything starts at `unknown`/empty, and `HttpState::new` fills the capture
/// format in from the configuration so the endpoint reports the *configured*
/// stream until the supervisor reports the negotiated one. The supervisor
/// (the dispatcher/capture wiring in `main`) owns the setters.
#[derive(Debug)]
pub struct RuntimeStatus {
    running: AtomicBool,
    recording: AtomicU8,
    microphone: AtomicU8,
    reported: Mutex<Reported>,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        RuntimeStatus {
            running: AtomicBool::new(false),
            recording: AtomicU8::new(RecordingState::Unknown.code()),
            microphone: AtomicU8::new(MicrophoneState::Unknown.code()),
            reported: Mutex::new(Reported::default()),
        }
    }
}

impl RuntimeStatus {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once capture and dispatch are actually running. Gates every `ok`
    /// that would otherwise be invented for a pipeline that never started.
    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn set_recording(&self, state: RecordingState) {
        self.recording.store(state.code(), Ordering::Relaxed);
    }

    pub fn recording(&self) -> RecordingState {
        RecordingState::from_code(self.recording.load(Ordering::Relaxed))
    }

    pub fn set_microphone(&self, state: MicrophoneState) {
        self.microphone.store(state.code(), Ordering::Relaxed);
    }

    pub fn microphone(&self) -> MicrophoneState {
        MicrophoneState::from_code(self.microphone.load(Ordering::Relaxed))
    }

    pub fn set_device(&self, device: impl Into<String>) {
        if let Ok(mut reported) = self.reported.lock() {
            reported.device = device.into();
        }
    }

    /// Reports the format the device negotiation actually settled on.
    pub fn set_capture_format(&self, rate_hz: u32, channels: u16, sample_format: impl Into<String>) {
        if let Ok(mut reported) = self.reported.lock() {
            reported.capture_rate_hz = rate_hz;
            reported.channels = channels;
            reported.sample_format = sample_format.into();
        }
    }

    /// Reports the timeline state (`time_quality`, `time_synced`, drift).
    pub fn set_time(&self, quality: Option<TimeQuality>, synced: bool, drift_ppm: Option<f64>) {
        if let Ok(mut reported) = self.reported.lock() {
            reported.time_quality = quality;
            reported.time_synced = synced;
            reported.clock_drift_ppm = drift_ppm;
        }
    }

    fn seed_capture_format(&self, rate_hz: u32, channels: u16, sample_format: &str) {
        if let Ok(mut reported) = self.reported.lock() {
            if reported.capture_rate_hz == 0 {
                reported.capture_rate_hz = rate_hz;
            }
            if reported.channels == 0 {
                reported.channels = channels;
            }
            if reported.sample_format.is_empty() {
                reported.sample_format = sample_format.to_string();
            }
        }
    }

    fn reported(&self) -> Reported {
        self.reported
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Everything the handlers need. See the module docs for the constructor.
pub struct HttpState {
    config: Config,
    database: Arc<Mutex<Database>>,
    metrics: Arc<Metrics>,
    runtime: Arc<RuntimeStatus>,
    web_root: PathBuf,
}

impl HttpState {
    /// Builds the shared state. `runtime` is the handle the supervisor updates;
    /// the configured capture format is used as its initial value.
    pub fn new(
        config: Config,
        database: Arc<Mutex<Database>>,
        metrics: Arc<Metrics>,
        runtime: Arc<RuntimeStatus>,
    ) -> Self {
        runtime.seed_capture_format(
            config.audio.preferred_rate_hz,
            config.audio.channels,
            config.audio.format.as_str(),
        );
        let web_root = config
            .storage
            .recording_dir
            .parent()
            .map(|parent| parent.join("web"))
            .unwrap_or_else(|| PathBuf::from("web"));
        HttpState {
            config,
            database,
            metrics,
            runtime,
            web_root,
        }
    }

    /// Overrides the directory the portal is served from (defaults to
    /// `<storage.recording_dir>/../web`).
    pub fn with_web_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.web_root = root.into();
        self
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn runtime(&self) -> &Arc<RuntimeStatus> {
        &self.runtime
    }

    pub fn web_root(&self) -> &Path {
        &self.web_root
    }

    /// Runs `f` with the database lock held. The closure is synchronous, which
    /// is what keeps the guard from crossing an `.await`.
    fn with_database<T>(&self, f: impl FnOnce(&Database) -> Result<T>) -> Result<T> {
        let guard = self
            .database
            .lock()
            .map_err(|_| Error::Internal("database lock poisoned".to_string()))?;
        f(&guard)
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Builds the full router: API, portal, and error fallbacks.
pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/", get(portal_index))
        .route("/web", get(portal_index))
        .route("/web/", get(portal_index))
        .route("/web/{*path}", get(portal_asset))
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/days", get(get_days))
        .route("/api/v1/segments", get(get_segments))
        .route("/api/v1/segments/{id}/gaps", get(get_gaps))
        .route("/api/v1/segments/{id}/audio", get(get_audio))
        .route("/api/v1/events", get(get_events))
        .route("/api/v1/events/{id}", get(get_event).patch(patch_event))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}

/// Binds the configured address and serves until the process stops.
///
/// Run this on a Tokio runtime sized by [`build_runtime`] so
/// `server.http_threads` is honoured.
pub async fn serve(state: Arc<HttpState>) -> Result<()> {
    let address = format!(
        "{}:{}",
        state.config.server.bind_address, state.config.server.port
    );
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|error| Error::Http(format!("cannot bind {address}: {error}")))?;
    tracing::info!(%address, "HTTP API listening");
    let app = router(state);
    axum::serve(listener, app)
        .await
        .map_err(|error| Error::Http(error.to_string()))
}

/// Builds the multi-threaded runtime the HTTP server should run on, sized by
/// the already-validated `server.http_threads` (TECH_SPEC §11: 1..=8).
pub fn build_runtime(config: &ServerConfig) -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.http_threads)
        .enable_all()
        .build()
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
}

async fn method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "method not allowed for this endpoint",
    )
}

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DateQuery {
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DaysQuery {
    from: Option<String>,
    to: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReviewBody {
    review_status: String,
}

/// A rejected query parameter. Kept as a small error type rather than a
/// `Response` so the failure path stays cheap.
enum ParamError {
    Missing(&'static str),
    Invalid(String),
}

impl ParamError {
    fn into_response(self) -> Response {
        match self {
            ParamError::Missing(name) => error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                format!("missing required query parameter `{name}` (expected YYYY-MM-DD)"),
            ),
            ParamError::Invalid(message) => {
                error_response(StatusCode::BAD_REQUEST, "invalid_parameter", message)
            }
        }
    }
}

fn required_date(
    value: Option<&str>,
    name: &'static str,
) -> std::result::Result<NaiveDate, ParamError> {
    let Some(raw) = value.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Err(ParamError::Missing(name));
    };
    parse_date(raw).map_err(|error| ParamError::Invalid(error.to_string()))
}

// ---------------------------------------------------------------------------
// /status
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct StatusView {
    service: ServiceView,
    recording: RecordingView,
    microphone: MicrophoneView,
    detector: DetectorView,
    storage: StorageView,
    capture: CaptureView,
    queues: QueuesView,
    dsp: DspView,
    time: TimeView,
    signal_state: &'static str,
}

#[derive(Debug, Serialize)]
struct ServiceView {
    state: &'static str,
    version: &'static str,
    now_utc: DateTime<Utc>,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct RecordingView {
    state: &'static str,
}

#[derive(Debug, Serialize)]
struct MicrophoneView {
    state: &'static str,
    device: String,
    capture_rate_hz: u32,
    channels: u16,
    sample_format: String,
}

#[derive(Debug, Serialize)]
struct DetectorView {
    state: &'static str,
    noise_floor_dbfs: Option<f32>,
    band_noise_floor_dbfs: Option<f32>,
    events_total: u64,
    events_discarded_short: u64,
    event_fk_retries: u64,
}

#[derive(Debug, Serialize)]
struct StorageView {
    used_bytes: u64,
    max_bytes: u64,
    free_bytes: Option<u64>,
    recording_dir: String,
    storage_pressure: bool,
}

#[derive(Debug, Serialize)]
struct CaptureView {
    xrun_count: u64,
    lost_frames_total: u64,
    capture_ring_overflow_frames: u64,
    dropped_detection_blocks: u64,
    gap_detection_limited: bool,
    clipped_samples_total: u64,
}

#[derive(Debug, Serialize)]
struct QueuesView {
    recording_high_water: u64,
    detection_high_water: u64,
    recording_control_slots: u64,
    detection_control_slots: u64,
}

#[derive(Debug, Serialize)]
struct DspView {
    count: u64,
    avg_ns: u64,
    p99_ns: u64,
    max_ns: u64,
}

#[derive(Debug, Serialize)]
struct TimeView {
    time_quality: &'static str,
    time_synced: bool,
    clock_drift_ppm: Option<f64>,
}

async fn get_status(State(state): State<Arc<HttpState>>) -> Response {
    let metrics = &state.metrics;
    // A failure here must not take the status page down; an unreadable usage
    // counter is reported as 0 and logged rather than invented.
    let used_bytes = match state.with_database(|db| db.bytes_of_recordings()) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "status could not read recording usage");
            0
        }
    };
    let reported = state.runtime.reported();
    let degraded = metrics.degraded.load(Ordering::Relaxed);
    let signal_state = if metrics.signal_silent.load(Ordering::Relaxed) {
        "silent"
    } else if state.runtime.is_running() {
        "ok"
    } else {
        "unknown"
    };
    let dsp = metrics.dsp_block_time.summary();

    json_response(
        StatusCode::OK,
        &StatusView {
            service: ServiceView {
                state: if degraded { "degraded" } else { "ok" },
                version: env!("CARGO_PKG_VERSION"),
                now_utc: Utc::now(),
                last_error: metrics.last_error(),
            },
            recording: RecordingView {
                state: state.runtime.recording().as_str(),
            },
            microphone: MicrophoneView {
                state: state.runtime.microphone().as_str(),
                device: reported.device,
                capture_rate_hz: reported.capture_rate_hz,
                channels: reported.channels,
                sample_format: reported.sample_format,
            },
            detector: DetectorView {
                state: metrics.detector_state().as_str(),
                noise_floor_dbfs: metrics.noise_floor_dbfs(),
                band_noise_floor_dbfs: metrics.band_noise_floor_dbfs(),
                events_total: metrics.events_total.load(Ordering::Relaxed),
                events_discarded_short: metrics.events_discarded_short.load(Ordering::Relaxed),
                event_fk_retries: metrics.event_fk_retries.load(Ordering::Relaxed),
            },
            storage: StorageView {
                used_bytes,
                max_bytes: state.config.storage.recording_max_bytes,
                free_bytes: free_bytes(&state.config.storage.recording_dir),
                recording_dir: state.config.storage.recording_dir.display().to_string(),
                storage_pressure: metrics.storage_pressure.load(Ordering::Relaxed),
            },
            capture: CaptureView {
                xrun_count: metrics.xrun_count.load(Ordering::Relaxed),
                lost_frames_total: metrics.lost_frames_total.load(Ordering::Relaxed),
                capture_ring_overflow_frames: metrics
                    .capture_ring_overflow_frames
                    .load(Ordering::Relaxed),
                dropped_detection_blocks: metrics
                    .dropped_detection_blocks
                    .load(Ordering::Relaxed),
                gap_detection_limited: metrics.gap_detection_limited.load(Ordering::Relaxed),
                clipped_samples_total: metrics.clipped_samples_total.load(Ordering::Relaxed),
            },
            queues: QueuesView {
                recording_high_water: metrics.recording_queue_high_water.load(Ordering::Relaxed),
                detection_high_water: metrics.detection_queue_high_water.load(Ordering::Relaxed),
                recording_control_slots: metrics
                    .recording_queue_control_slots
                    .load(Ordering::Relaxed),
                detection_control_slots: metrics
                    .detection_queue_control_slots
                    .load(Ordering::Relaxed),
            },
            dsp: DspView {
                count: dsp.count,
                avg_ns: dsp.avg_ns,
                p99_ns: dsp.p99_ns,
                max_ns: dsp.max_ns,
            },
            time: TimeView {
                time_quality: reported.time_quality.map_or("unknown", TimeQuality::as_str),
                time_synced: reported.time_synced,
                clock_drift_ppm: reported.clock_drift_ppm,
            },
            signal_state,
        },
    )
}

/// Free bytes on the filesystem holding the recordings, or `None` when the
/// platform or the call cannot answer (the field is nullable on purpose).
#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated C string and `stats` is a valid
    // pointer to uninitialized memory of the exact struct `statvfs` fills in.
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: a zero return means `statvfs` initialized the whole struct.
    let stats = unsafe { stats.assume_init() };
    // `c_ulong` is 32-bit on armv7 and 64-bit on aarch64/x86_64, so these casts
    // are load-bearing on the Pi even though they are no-ops on a 64-bit host.
    #[allow(clippy::unnecessary_cast)]
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Views shared by several endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SegmentView {
    id: String,
    started_at_utc: DateTime<Utc>,
    ended_at_utc: Option<DateTime<Utc>>,
    capture_rate_hz: u32,
    channels: u16,
    sample_format: String,
    frame_count: u64,
    size_bytes: u64,
    duration_ms: u64,
    status: String,
    time_quality: String,
    end_reason: Option<String>,
    lost_frames_total: u64,
    gap_count: u64,
    xrun_count: u64,
    clock_drift_ppm: Option<f64>,
    audio_url: String,
}

#[derive(Debug, Serialize)]
struct EventView {
    id: String,
    segment_id: String,
    capture_rate_hz: u32,
    start_offset_frames: u64,
    end_offset_frames: u64,
    started_at_utc: DateTime<Utc>,
    ended_at_utc: DateTime<Utc>,
    duration_ms: u32,
    rule_score: Option<f32>,
    detector_version: String,
    peak_dbfs: Option<f32>,
    mean_level_dbfs: Option<f32>,
    mean_band_ratio: Option<f32>,
    noise_floor_dbfs: Option<f32>,
    end_reason: String,
    review_status: String,
    continued: bool,
}

#[derive(Debug, Serialize)]
struct GapView {
    offset_frames: u64,
    lost_frames: u64,
    kind: &'static str,
    estimate_source: &'static str,
    at_utc: DateTime<Utc>,
}

/// Audio duration of a segment from the authoritative stored frame count, so a
/// segment containing gaps is not overstated by wall-clock differences.
fn duration_ms(frame_count: u64, capture_rate_hz: u32) -> u64 {
    if capture_rate_hz == 0 {
        return 0;
    }
    frame_count.saturating_mul(1000) / u64::from(capture_rate_hz)
}

fn segment_view(segment: &SegmentRow) -> SegmentView {
    SegmentView {
        id: segment.id.clone(),
        started_at_utc: segment.started_at_utc,
        ended_at_utc: segment.ended_at_utc,
        capture_rate_hz: segment.capture_rate_hz,
        channels: segment.channels,
        sample_format: segment.sample_format.clone(),
        frame_count: segment.frame_count,
        size_bytes: segment.size_bytes,
        duration_ms: duration_ms(segment.frame_count, segment.capture_rate_hz),
        status: segment.status.clone(),
        time_quality: segment.time_quality.clone(),
        end_reason: segment.end_reason.clone(),
        lost_frames_total: segment.lost_frames_total,
        gap_count: segment.gap_count,
        xrun_count: segment.xrun_count,
        clock_drift_ppm: segment.clock_drift_ppm,
        audio_url: format!("{API_PREFIX}/segments/{}/audio", segment.id),
    }
}

fn event_view(event: &EventRow, capture_rate_hz: u32) -> EventView {
    EventView {
        id: event.id.clone(),
        segment_id: event.segment_id.clone(),
        capture_rate_hz,
        start_offset_frames: event.start_offset_frames,
        end_offset_frames: event.end_offset_frames,
        started_at_utc: event.started_at_utc,
        ended_at_utc: event.ended_at_utc,
        duration_ms: event.duration_ms,
        rule_score: event.rule_score,
        detector_version: event.detector_version.clone(),
        peak_dbfs: event.peak_dbfs,
        mean_level_dbfs: event.mean_level_dbfs,
        mean_band_ratio: event.mean_band_ratio,
        noise_floor_dbfs: event.noise_floor_dbfs,
        end_reason: event.end_reason.clone(),
        review_status: event.review_status.clone(),
        continued: event.continued,
    }
}

/// Capture rate for an event: the rate of the segment it belongs to, falling
/// back to the detection rate only if the segment row has disappeared.
fn capture_rate_for(db: &Database, segment_id: &str, fallback: u32) -> Result<u32> {
    Ok(db
        .segment(segment_id)?
        .map_or(fallback, |segment| segment.capture_rate_hz))
}

// ---------------------------------------------------------------------------
// /days
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DaysView {
    from: String,
    to: String,
    days: Vec<DayView>,
}

#[derive(Debug, Serialize)]
struct DayView {
    date: String,
    segment_count: usize,
    event_count: usize,
    recorded_ms: u64,
    lost_frames_total: u64,
}

async fn get_days(
    State(state): State<Arc<HttpState>>,
    query: std::result::Result<Query<DaysQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                rejection.body_text(),
            );
        }
    };
    let from = match required_date(query.from.as_deref(), "from") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    let to = match required_date(query.to.as_deref(), "to") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    if from > to {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!("`from` ({from}) must not be after `to` ({to})"),
        );
    }
    if (to - from).num_days() >= MAX_DAYS_SPAN {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!("requested span must be at most {MAX_DAYS_SPAN} days"),
        );
    }

    let timezone = state.config.server.timezone();
    let collected = state.with_database(|db| {
        let mut days = Vec::new();
        let mut date = from;
        loop {
            let (start, end) = local_day_range(date, timezone);
            let segments = db.segments_between(start, end)?;
            let events = db.events_between(start, end)?;
            // Only nights that actually hold data, per TECH_SPEC §8/§9: the date
            // list is a recording list, not a calendar.
            if !segments.is_empty() || !events.is_empty() {
                days.push(DayView {
                    date: date.format("%Y-%m-%d").to_string(),
                    segment_count: segments.len(),
                    event_count: events.len(),
                    recorded_ms: segments
                        .iter()
                        .map(|segment| duration_ms(segment.frame_count, segment.capture_rate_hz))
                        .sum(),
                    lost_frames_total: segments.iter().map(|s| s.lost_frames_total).sum(),
                });
            }
            if date == to {
                break;
            }
            match date.succ_opt() {
                Some(next) => date = next,
                None => break,
            }
        }
        Ok(days)
    });

    match collected {
        Ok(days) => json_response(
            StatusCode::OK,
            &DaysView {
                from: from.format("%Y-%m-%d").to_string(),
                to: to.format("%Y-%m-%d").to_string(),
                days,
            },
        ),
        Err(error) => internal_error(&error),
    }
}

// ---------------------------------------------------------------------------
// /segments
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SegmentsView {
    date: String,
    segments: Vec<SegmentView>,
}

async fn get_segments(
    State(state): State<Arc<HttpState>>,
    query: std::result::Result<Query<DateQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                rejection.body_text(),
            );
        }
    };
    let date = match required_date(query.date.as_deref(), "date") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    let (start, end) = local_day_range(date, state.config.server.timezone());

    match state.with_database(|db| db.segments_between(start, end)) {
        Ok(rows) => json_response(
            StatusCode::OK,
            &SegmentsView {
                date: date.format("%Y-%m-%d").to_string(),
                segments: rows.iter().map(segment_view).collect(),
            },
        ),
        Err(error) => internal_error(&error),
    }
}

// ---------------------------------------------------------------------------
// /segments/{id}/gaps
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct GapsView {
    segment_id: String,
    capture_rate_hz: u32,
    gaps: Vec<GapView>,
}

async fn get_gaps(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    let found = state.with_database(|db| {
        let Some(segment) = db.segment(&id)? else {
            return Ok(None);
        };
        let gaps = db.gaps_for_segment(&id)?;
        Ok(Some((segment.capture_rate_hz, gaps)))
    });

    match found {
        Ok(Some((capture_rate_hz, gaps))) => json_response(
            StatusCode::OK,
            &GapsView {
                segment_id: id,
                capture_rate_hz,
                gaps: gaps
                    .iter()
                    .map(|gap: &GapRecord| GapView {
                        offset_frames: gap.offset_frames,
                        lost_frames: gap.lost_frames,
                        kind: gap.kind.as_str(),
                        estimate_source: gap.estimate_source.as_str(),
                        at_utc: gap.at_utc,
                    })
                    .collect(),
            },
        ),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no segment with id {id}"),
        ),
        Err(error) => internal_error(&error),
    }
}

// ---------------------------------------------------------------------------
// /events
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct EventsView {
    date: String,
    /// Nominal rate for the day (the detector's fixed 16 kHz in the MVP). Each
    /// event also carries the capture rate of its own segment, which is what a
    /// client must use to convert offsets to seconds.
    capture_rate_hz: u32,
    events: Vec<EventView>,
}

async fn get_events(
    State(state): State<Arc<HttpState>>,
    query: std::result::Result<Query<DateQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                rejection.body_text(),
            );
        }
    };
    let date = match required_date(query.date.as_deref(), "date") {
        Ok(date) => date,
        Err(error) => return error.into_response(),
    };
    let (start, end) = local_day_range(date, state.config.server.timezone());
    let fallback_rate = state.config.detector.detection_rate_hz;

    let collected = state.with_database(|db| {
        let events = db.events_between(start, end)?;
        let segments = db.segments_between(start, end)?;
        let mut rates: HashMap<String, u32> = segments
            .into_iter()
            .map(|segment| (segment.id.clone(), segment.capture_rate_hz))
            .collect();
        let mut views = Vec::with_capacity(events.len());
        for event in &events {
            let rate = match rates.get(&event.segment_id) {
                Some(rate) => *rate,
                None => {
                    // An in-progress segment that began before the day starts is
                    // not returned by `segments_between`, so look it up directly.
                    let rate = capture_rate_for(db, &event.segment_id, fallback_rate)?;
                    rates.insert(event.segment_id.clone(), rate);
                    rate
                }
            };
            views.push(event_view(event, rate));
        }
        Ok(views)
    });

    match collected {
        Ok(events) => json_response(
            StatusCode::OK,
            &EventsView {
                date: date.format("%Y-%m-%d").to_string(),
                capture_rate_hz: fallback_rate,
                events,
            },
        ),
        Err(error) => internal_error(&error),
    }
}

#[derive(Debug, Serialize)]
struct EventDetailView {
    event: EventView,
    segment: SegmentView,
}

async fn get_event(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    let fallback_rate = state.config.detector.detection_rate_hz;
    let found = state.with_database(|db| match db.event_with_segment(&id)? {
        Some((event, segment)) => Ok(Some((event, segment))),
        None => Ok(None),
    });

    match found {
        Ok(Some((event, segment))) => {
            let rate = capture_rate_or(&segment, fallback_rate);
            json_response(
                StatusCode::OK,
                &EventDetailView {
                    event: event_view(&event, rate),
                    segment: segment_view(&segment),
                },
            )
        }
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no event with id {id}"),
        ),
        Err(error) => internal_error(&error),
    }
}

fn capture_rate_or(segment: &SegmentRow, fallback: u32) -> u32 {
    if segment.capture_rate_hz > 0 {
        segment.capture_rate_hz
    } else {
        fallback
    }
}

/// `PATCH /api/v1/events/{id}` — only the three review labels are accepted.
/// Returns the updated event object itself.
async fn patch_event(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
    body: std::result::Result<Json<ReviewBody>, JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                format!("expected JSON body {{\"review_status\":...}}: {}", rejection.body_text()),
            );
        }
    };
    let status = match ReviewStatus::try_from(body.review_status.as_str()) {
        Ok(status) => status,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_review_status",
                format!(
                    "review_status must be one of \"snore\", \"not_snore\", \"unreviewed\", got {:?}",
                    body.review_status
                ),
            );
        }
    };

    let fallback_rate = state.config.detector.detection_rate_hz;
    let updated = state.with_database(|db| {
        if !db.set_review_status(&id, status)? {
            return Ok(None);
        }
        let Some(event) = db.event(&id)? else {
            return Ok(None);
        };
        let rate = capture_rate_for(db, &event.segment_id, fallback_rate)?;
        Ok(Some(event_view(&event, rate)))
    });

    match updated {
        Ok(Some(event)) => json_response(StatusCode::OK, &event),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no event with id {id}"),
        ),
        Err(error) => internal_error(&error),
    }
}

// ---------------------------------------------------------------------------
// /segments/{id}/audio
// ---------------------------------------------------------------------------

/// Outcome of interpreting a `Range` header against a known file length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeSelection {
    /// No usable range: answer with the whole file and `200`.
    Full,
    /// Inclusive byte offsets, answered with `206`.
    Partial { start: u64, end: u64 },
    /// A well-formed range that selects nothing: `416`.
    Unsatisfiable,
}

fn parse_range_header(header_value: Option<&str>, total: u64) -> RangeSelection {
    let Some(raw) = header_value else {
        return RangeSelection::Full;
    };
    let raw = raw.trim();
    let Some((unit, spec)) = raw.split_once('=') else {
        return RangeSelection::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return RangeSelection::Full;
    }
    let spec = spec.trim();
    // Multiple ranges are explicitly unsupported: answer with the full file.
    if spec.contains(',') {
        return RangeSelection::Full;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeSelection::Full;
    };
    let (first, last) = (first.trim(), last.trim());

    if first.is_empty() {
        // Suffix form: the last `n` bytes.
        let Ok(suffix) = last.parse::<u64>() else {
            return RangeSelection::Full;
        };
        if suffix == 0 || total == 0 {
            return RangeSelection::Unsatisfiable;
        }
        return RangeSelection::Partial {
            start: total.saturating_sub(suffix),
            end: total - 1,
        };
    }

    let Ok(start) = first.parse::<u64>() else {
        return RangeSelection::Full;
    };
    if start >= total {
        return RangeSelection::Unsatisfiable;
    }
    if last.is_empty() {
        return RangeSelection::Partial {
            start,
            end: total - 1,
        };
    }
    let Ok(end) = last.parse::<u64>() else {
        return RangeSelection::Full;
    };
    if end < start {
        // Invalid syntax, not an unsatisfiable range: ignore it (RFC 9110).
        return RangeSelection::Full;
    }
    RangeSelection::Partial {
        start,
        end: end.min(total - 1),
    }
}

fn etag_for(segment_id: &str, size_bytes: u64) -> String {
    format!("\"{segment_id}-{size_bytes}\"")
}

fn insert_header(response: &mut Response, name: header::HeaderName, value: &str) {
    match HeaderValue::from_str(value) {
        Ok(value) => {
            response.headers_mut().insert(name, value);
        }
        Err(error) => tracing::warn!(%error, "could not encode response header"),
    }
}

/// Builds an audio-endpoint error: always the standard envelope, plus the range
/// metadata a client needs to recover.
fn audio_error(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
    total: Option<u64>,
    etag: Option<&str>,
) -> Response {
    let mut response = error_response(status, code, message);
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(total) = total {
        insert_header(
            &mut response,
            header::CONTENT_RANGE,
            &format!("bytes */{total}"),
        );
    }
    if let Some(etag) = etag {
        insert_header(&mut response, header::ETAG, etag);
    }
    response
}

/// Streams `len` bytes of `file` starting at the current position in fixed-size
/// chunks. The file is never read into a single buffer.
fn audio_body(file: tokio::fs::File, len: u64) -> Body {
    let stream = futures_util::stream::unfold((file, len), |(mut file, remaining)| async move {
        if remaining == 0 {
            return None;
        }
        let wanted = remaining.min(AUDIO_CHUNK_BYTES as u64) as usize;
        let mut buffer = vec![0u8; wanted];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                let read = read as u64;
                Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from(buffer)),
                    (file, remaining - read),
                ))
            }
            Err(error) => Some((Err(error), (file, 0))),
        }
    });
    Body::from_stream(stream)
}

async fn get_audio(
    State(state): State<Arc<HttpState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Response {
    let segment = match state.with_database(|db| db.segment(&id)) {
        Ok(Some(segment)) => segment,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no segment with id {id}"),
            );
        }
        Err(error) => return internal_error(&error),
    };

    match segment.status.as_str() {
        "deleted" => {
            return audio_error(
                StatusCode::GONE,
                "audio_expired",
                "the recording for this segment was deleted by retention",
                None,
                None,
            );
        }
        "recording" => {
            return audio_error(
                StatusCode::CONFLICT,
                "audio_not_ready",
                "the segment is still being recorded",
                None,
                None,
            );
        }
        "missing" => {
            return audio_error(
                StatusCode::NOT_FOUND,
                "audio_missing",
                "the recording file for this segment is missing",
                None,
                None,
            );
        }
        _ => {}
    }

    // The only source of a playback path: the DB `relative_path` joined to the
    // configured recordings root. Nothing from the request is used here.
    let relative = Path::new(&segment.relative_path);
    if let Err(error) = validate_relative_path(relative) {
        tracing::error!(segment_id = %segment.id, %error, "stored recording path is not safe to serve");
        return internal_error(&error);
    }
    let path = state.config.storage.recording_dir.join(relative);

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                segment_id = %segment.id,
                path = %path.display(),
                "segment row exists but its recording file is missing"
            );
            return audio_error(
                StatusCode::NOT_FOUND,
                "audio_missing",
                "the recording file for this segment is missing",
                None,
                None,
            );
        }
        Err(error) => return internal_error(&Error::io(&path, error)),
    };

    let file_len = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(error) => return internal_error(&Error::io(&path, error)),
    };
    if file_len != segment.size_bytes {
        tracing::warn!(
            segment_id = %segment.id,
            db_size_bytes = segment.size_bytes,
            file_size_bytes = file_len,
            "database size_bytes disagrees with the recording file; serving the real length"
        );
    }
    let etag = etag_for(&segment.id, file_len);

    let (start, length, status) = match parse_range_header(
        headers.get(header::RANGE).and_then(|value| value.to_str().ok()),
        file_len,
    ) {
        RangeSelection::Full => (0, file_len, StatusCode::OK),
        RangeSelection::Partial { start, end } => (start, end - start + 1, StatusCode::PARTIAL_CONTENT),
        RangeSelection::Unsatisfiable => {
            return audio_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "range_not_satisfiable",
                format!("requested range is outside the {file_len}-byte recording"),
                Some(file_len),
                Some(&etag),
            );
        }
    };

    if start > 0
        && let Err(error) = file.seek(SeekFrom::Start(start)).await
    {
        return internal_error(&Error::io(&path, error));
    }

    let mut response = Response::builder()
        .status(status)
        .body(audio_body(file, length))
        .unwrap_or_else(|error| {
            tracing::error!(%error, "could not build audio response");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "could not build the audio response",
            )
        });
    {
        let out = response.headers_mut();
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
        out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    insert_header(&mut response, header::CONTENT_LENGTH, &length.to_string());
    insert_header(&mut response, header::ETAG, &etag);
    if status == StatusCode::PARTIAL_CONTENT {
        insert_header(
            &mut response,
            header::CONTENT_RANGE,
            &format!("bytes {start}-{}/{file_len}", start + length - 1),
        );
    }
    response
}

// ---------------------------------------------------------------------------
// Static portal
// ---------------------------------------------------------------------------

async fn portal_index(State(state): State<Arc<HttpState>>) -> Response {
    serve_portal_file(&state, "index.html").await
}

async fn portal_asset(
    State(state): State<Arc<HttpState>>,
    RoutePath(path): RoutePath<String>,
) -> Response {
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    serve_portal_file(&state, relative).await
}

/// Rejects anything that is not a plain relative path below the portal root:
/// absolute paths, `..`, `.` and root components never reach the filesystem.
fn portal_path_is_safe(relative: &str) -> bool {
    let path = Path::new(relative);
    if relative.is_empty() || path.is_absolute() {
        return false;
    }
    let mut has_name = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_name = true,
            _ => return false,
        }
    }
    has_name
}

fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("webmanifest") => "application/manifest+json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

async fn serve_portal_file(state: &HttpState, relative: &str) -> Response {
    let root = &state.web_root;
    if !root.is_dir() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "portal_unavailable",
            format!(
                "the portal directory {} is not installed; nothing is served at this path",
                root.display()
            ),
        );
    }
    if !portal_path_is_safe(relative) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "portal paths must be relative and must not contain `..`",
        );
    }

    let canonical_root = match tokio::fs::canonicalize(root).await {
        Ok(path) => path,
        Err(error) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "portal_unavailable",
                format!("cannot read the portal directory {}: {error}", root.display()),
            );
        }
    };
    let candidate = root.join(relative);
    let canonical = match tokio::fs::canonicalize(&candidate).await {
        Ok(path) => path,
        Err(_) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no portal file at {relative}"),
            );
        }
    };
    // Symlinks may not lead outside the portal root.
    if !canonical.starts_with(&canonical_root) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "portal path escapes the portal directory",
        );
    }

    match tokio::fs::read(&canonical).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type_for(&canonical)),
            );
            response
        }
        Err(error) => internal_error(&Error::io(&canonical, error)),
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the pure helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing_covers_the_three_supported_forms() {
        assert_eq!(parse_range_header(None, 10), RangeSelection::Full);
        assert_eq!(parse_range_header(Some(""), 10), RangeSelection::Full);
        assert_eq!(
            parse_range_header(Some("bytes=0-3"), 10),
            RangeSelection::Partial { start: 0, end: 3 }
        );
        assert_eq!(
            parse_range_header(Some("bytes=4-"), 10),
            RangeSelection::Partial { start: 4, end: 9 }
        );
        assert_eq!(
            parse_range_header(Some("bytes=-3"), 10),
            RangeSelection::Partial { start: 7, end: 9 }
        );
        assert_eq!(
            parse_range_header(Some("bytes=0-99"), 10),
            RangeSelection::Partial { start: 0, end: 9 }
        );
    }

    #[test]
    fn range_parsing_separates_ignored_from_unsatisfiable() {
        // Not a single well-formed byte range: the whole file is sent instead.
        for ignored in [
            "bytes=0-1,5-6",
            "items=0-1",
            "bytes=abc",
            "bytes=5-1",
            "bytes=1-2-3",
            "bytes",
        ] {
            assert_eq!(
                parse_range_header(Some(ignored), 10),
                RangeSelection::Full,
                "{ignored}"
            );
        }
        // Well formed but selecting nothing.
        for unsatisfiable in ["bytes=10-", "bytes=11-20", "bytes=-0", "bytes=0-"] {
            let total = if unsatisfiable == "bytes=0-" { 0 } else { 10 };
            assert_eq!(
                parse_range_header(Some(unsatisfiable), total),
                RangeSelection::Unsatisfiable,
                "{unsatisfiable}"
            );
        }
    }

    #[test]
    fn portal_paths_are_confined_to_the_root() {
        assert!(portal_path_is_safe("index.html"));
        assert!(portal_path_is_safe("assets/app.js"));
        assert!(!portal_path_is_safe(""));
        assert!(!portal_path_is_safe("../secret"));
        assert!(!portal_path_is_safe("assets/../../secret"));
        assert!(!portal_path_is_safe("/etc/passwd"));
        assert!(!portal_path_is_safe("./index.html"));
    }

    #[test]
    fn content_types_cover_the_portal_assets() {
        assert_eq!(content_type_for(Path::new("index.html")), "text/html; charset=utf-8");
        assert_eq!(content_type_for(Path::new("app.css")), "text/css; charset=utf-8");
        assert_eq!(content_type_for(Path::new("app.js")), "text/javascript; charset=utf-8");
        assert_eq!(content_type_for(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(content_type_for(Path::new("x.bin")), "application/octet-stream");
    }

    #[test]
    fn runtime_status_defaults_to_unknown_and_round_trips() {
        let runtime = RuntimeStatus::new();
        assert_eq!(runtime.recording(), RecordingState::Unknown);
        assert_eq!(runtime.microphone(), MicrophoneState::Unknown);
        assert!(!runtime.is_running());
        runtime.set_recording(RecordingState::Stopped);
        runtime.set_microphone(MicrophoneState::Disconnected);
        runtime.set_running(true);
        assert_eq!(runtime.recording().as_str(), "stopped");
        assert_eq!(runtime.microphone().as_str(), "disconnected");
        assert!(runtime.is_running());
    }

    #[test]
    fn duration_uses_the_stored_frame_count() {
        assert_eq!(duration_ms(16_000, 16_000), 1000);
        assert_eq!(duration_ms(0, 16_000), 0);
        assert_eq!(duration_ms(16_000, 0), 0);
    }

    #[test]
    fn etag_matches_the_documented_shape() {
        assert_eq!(etag_for("seg-1", 44), "\"seg-1-44\"");
    }
}
