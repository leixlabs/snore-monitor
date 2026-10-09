//! Integration tests for the HTTP API (TECH_SPEC §8, §12; tasks T5.1–T5.6).
//!
//! Every test drives the real router through `tower::ServiceExt::oneshot`, so
//! the assertions cover the same code path a browser or the portal would use.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use snore_monitor::config::{AudioConfig, Config, DetectorConfig, ServerConfig, StorageConfig};
use snore_monitor::detector::{EndReason, EventDraft};
use futures_util::StreamExt;
use snore_monitor::http_server::{
    HttpState, MicrophoneState, RecordingState, RuntimeStatus, router, serve,
};
use snore_monitor::metrics::Metrics;
use snore_monitor::recorder::{WavFormat, write_header};
use snore_monitor::storage::{Database, ReviewStatus, SegmentCompleted, SegmentStarted, SegmentStatus};
use snore_monitor::timeline::{EstimateSource, GapKind, GapRecord, TimeQuality};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    state: Arc<HttpState>,
    router: Router,
    database: Arc<Mutex<Database>>,
    metrics: Arc<Metrics>,
    runtime: Arc<RuntimeStatus>,
    recording_dir: PathBuf,
    root: PathBuf,
    _dir: TempDir,
}

fn utc(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().unwrap()
}

fn harness_with_timezone(timezone: &str) -> Harness {
    harness_with(tempfile::tempdir().unwrap(), timezone, 0)
}

fn harness_with(dir: TempDir, timezone: &str, port: u16) -> Harness {
    let root = dir.path().to_path_buf();
    let recording_dir = root.join("recordings");
    std::fs::create_dir_all(&recording_dir).unwrap();
    let database = Arc::new(Mutex::new(Database::in_memory().unwrap()));
    let metrics = Arc::new(Metrics::new());
    let runtime = Arc::new(RuntimeStatus::new());
    let config = Config {
        server: ServerConfig {
            bind_address: "127.0.0.1".to_string(),
            port,
            http_threads: 1,
            timezone: timezone.to_string(),
        },
        audio: AudioConfig::default(),
        detector: DetectorConfig::default(),
        storage: StorageConfig {
            recording_dir: recording_dir.clone(),
            database_path: root.join("snore.sqlite"),
            recording_max_bytes: 1 << 30,
            recording_cleanup_target_bytes: 1 << 29,
            recording_reserve_bytes: 1 << 20,
            retention_check_interval_seconds: 60,
        },
    };
    let state = Arc::new(HttpState::new(
        config,
        Arc::clone(&database),
        Arc::clone(&metrics),
        Arc::clone(&runtime),
    ));
    Harness {
        router: router(Arc::clone(&state)),
        state,
        database,
        metrics,
        runtime,
        recording_dir,
        root,
        _dir: dir,
    }
}

fn harness() -> Harness {
    // A fixed timezone keeps every date boundary assertion independent of the
    // machine running the tests.
    harness_with_timezone("UTC")
}

struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Raw {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!("body is not JSON ({error}): {}", String::from_utf8_lossy(&self.body))
        })
    }

    fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

async fn send(harness: &Harness, request: Request<Body>) -> Raw {
    let response = harness.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024).await.unwrap();
    Raw {
        status,
        headers,
        body: body.to_vec(),
    }
}

async fn get(harness: &Harness, uri: &str) -> Raw {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    send(harness, request).await
}

async fn head(harness: &Harness, uri: &str, range: Option<&str>) -> Raw {
    let mut builder = Request::builder().method("HEAD").uri(uri);
    if let Some(range) = range {
        builder = builder.header(header::RANGE, range);
    }
    send(harness, builder.body(Body::empty()).unwrap()).await
}

async fn get_range(harness: &Harness, uri: &str, range: &str) -> Raw {
    let request = Request::builder()
        .uri(uri)
        .header(header::RANGE, range)
        .body(Body::empty())
        .unwrap();
    send(harness, request).await
}

async fn patch(harness: &Harness, uri: &str, body: &str) -> Raw {
    let request = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(harness, request).await
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct SegmentSpec {
    id: &'static str,
    started: DateTime<Utc>,
    ended: Option<DateTime<Utc>>,
    frames: u64,
    rate: u32,
    relative_path: String,
    size_bytes: u64,
    status: SegmentStatus,
    lost_frames_total: u64,
    gap_count: u64,
    xrun_count: u64,
}

impl SegmentSpec {
    fn new(id: &'static str, started: DateTime<Utc>, relative_path: &str) -> Self {
        SegmentSpec {
            id,
            started,
            ended: Some(started + chrono::Duration::minutes(30)),
            frames: 16_000,
            rate: 16_000,
            relative_path: relative_path.to_string(),
            size_bytes: 44,
            status: SegmentStatus::Complete,
            lost_frames_total: 0,
            gap_count: 0,
            xrun_count: 0,
        }
    }
}

fn add_segment(harness: &Harness, spec: &SegmentSpec) {
    let db = harness.database.lock().unwrap();
    db.insert_segment_started(&SegmentStarted {
        id: spec.id.to_string(),
        relative_path: PathBuf::from(&spec.relative_path),
        started_at_utc: spec.started,
        capture_rate_hz: spec.rate,
        channels: 1,
        sample_format: "S16_LE".to_string(),
        channel_mapping: "mono:0".to_string(),
        time_quality: TimeQuality::Synced,
        boot_id: "boot-test".to_string(),
    })
    .unwrap();
    if spec.status == SegmentStatus::Recording {
        // An in-progress segment is only ever opened: no end time, no totals yet.
        return;
    }
    db.complete_segment(&SegmentCompleted {
        id: spec.id.to_string(),
        ended_at_utc: spec.ended.unwrap_or(spec.started),
        frame_count: spec.frames,
        size_bytes: spec.size_bytes,
        end_reason: "duration".to_string(),
        clock_drift_ppm: None,
        xrun_count: spec.xrun_count,
        gap_count: spec.gap_count,
        lost_frames_total: spec.lost_frames_total,
        status: spec.status,
    })
    .unwrap();
}

fn add_event(
    harness: &Harness,
    id: &str,
    segment_id: &str,
    started: DateTime<Utc>,
    start_offset_frames: u64,
    end_offset_frames: u64,
    rate: u32,
) {
    let duration_ms = ((end_offset_frames - start_offset_frames) * 1000 / u64::from(rate)) as u32;
    let db = harness.database.lock().unwrap();
    db.insert_event(&EventDraft {
        id: id.to_string(),
        segment_id: segment_id.to_string(),
        start_offset_frames,
        end_offset_frames,
        started_at_utc: started,
        ended_at_utc: started + chrono::Duration::milliseconds(i64::from(duration_ms)),
        duration_ms,
        rule_score: Some(0.75),
        detector_version: "rule-v1".to_string(),
        peak_dbfs: Some(-12.5),
        mean_level_dbfs: Some(-31.0),
        mean_band_ratio: Some(0.6),
        noise_floor_dbfs: Some(-62.0),
        end_reason: EndReason::Normal,
        continued: false,
    })
    .unwrap();
}

/// Writes a real 44-byte-header WAV with a deterministic payload and returns its
/// exact byte contents.
fn write_recording(harness: &Harness, relative: &str, payload_len: usize) -> Vec<u8> {
    let path = harness.recording_dir.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let payload: Vec<u8> = (0..payload_len).map(|index| (index % 251) as u8).collect();
    let mut file = std::fs::File::create(&path).unwrap();
    write_header(
        &mut file,
        &WavFormat::new(16_000, 1).unwrap(),
        payload_len as u64,
    )
    .unwrap();
    file.write_all(&payload).unwrap();
    file.sync_all().unwrap();
    std::fs::read(&path).unwrap()
}

// ---------------------------------------------------------------------------
// /status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_unknown_rather_than_inventing_health() {
    let harness = harness();
    let response = get(&harness, "/api/v1/status").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();

    assert_eq!(body["service"]["state"], "ok");
    assert_eq!(body["service"]["version"], "0.1.0");
    assert!(body["service"]["last_error"].is_null());
    DateTime::parse_from_rfc3339(body["service"]["now_utc"].as_str().unwrap()).unwrap();

    assert_eq!(body["recording"]["state"], "unknown");
    assert_eq!(body["microphone"]["state"], "unknown");
    assert_eq!(body["microphone"]["device"], "");
    // The configured stream is reported until the negotiation reports its own.
    assert_eq!(body["microphone"]["capture_rate_hz"], 16_000);
    assert_eq!(body["microphone"]["channels"], 1);
    assert_eq!(body["microphone"]["sample_format"], "S16_LE");

    assert_eq!(body["detector"]["state"], "unknown");
    assert!(body["detector"]["noise_floor_dbfs"].is_null());
    assert!(body["detector"]["band_noise_floor_dbfs"].is_null());
    assert_eq!(body["detector"]["events_total"], 0);
    assert_eq!(body["detector"]["events_discarded_short"], 0);
    assert_eq!(body["detector"]["event_fk_retries"], 0);

    assert_eq!(body["storage"]["used_bytes"], 0);
    assert_eq!(body["storage"]["max_bytes"], 1 << 30);
    assert_eq!(body["storage"]["recording_dir"], harness.recording_dir.display().to_string());
    assert_eq!(body["storage"]["storage_pressure"], false);
    #[cfg(unix)]
    assert!(body["storage"]["free_bytes"].is_u64());

    assert_eq!(body["capture"]["xrun_count"], 0);
    assert_eq!(body["capture"]["lost_frames_total"], 0);
    assert_eq!(body["capture"]["capture_ring_overflow_frames"], 0);
    assert_eq!(body["capture"]["dropped_detection_blocks"], 0);
    assert_eq!(body["capture"]["gap_detection_limited"], false);
    assert_eq!(body["capture"]["clipped_samples_total"], 0);

    assert_eq!(body["queues"]["recording_high_water"], 0);
    assert_eq!(body["queues"]["detection_high_water"], 0);
    assert_eq!(body["queues"]["recording_control_slots"], 0);
    assert_eq!(body["queues"]["detection_control_slots"], 0);

    assert_eq!(body["dsp"]["count"], 0);
    assert_eq!(body["dsp"]["avg_ns"], 0);
    assert_eq!(body["dsp"]["p99_ns"], 0);
    assert_eq!(body["dsp"]["max_ns"], 0);

    assert_eq!(body["time"]["time_quality"], "unknown");
    assert_eq!(body["time"]["time_synced"], false);
    assert!(body["time"]["clock_drift_ppm"].is_null());

    assert_eq!(body["signal_state"], "unknown");
}

#[tokio::test]
async fn status_reflects_metrics_and_supervisor_reports() {
    let harness = harness();
    harness.runtime.set_running(true);
    harness.runtime.set_recording(RecordingState::Stopped);
    harness.runtime.set_microphone(MicrophoneState::Disconnected);
    harness.runtime.set_device("USB PnP Sound Device");
    harness.runtime.set_capture_format(48_000, 2, "S16_LE");
    harness
        .runtime
        .set_time(Some(TimeQuality::Corrected), true, Some(4.25));

    harness.metrics.events_total.store(9, Ordering::Relaxed);
    harness.metrics.events_discarded_short.store(2, Ordering::Relaxed);
    harness.metrics.event_fk_retries.store(1, Ordering::Relaxed);
    harness.metrics.xrun_count.store(3, Ordering::Relaxed);
    harness.metrics.lost_frames_total.store(480, Ordering::Relaxed);
    harness.metrics.clipped_samples_total.store(17, Ordering::Relaxed);
    harness.metrics.storage_pressure.store(true, Ordering::Relaxed);
    harness.metrics.degraded.store(true, Ordering::Relaxed);
    harness.metrics.set_noise_floor_dbfs(Some(-63.5));
    harness.metrics.set_band_noise_floor_dbfs(Some(-70.25));
    harness.metrics.set_last_error("CPAL stream error");

    let body = get(&harness, "/api/v1/status").await.json();

    assert_eq!(body["service"]["state"], "degraded");
    assert_eq!(body["service"]["last_error"], "CPAL stream error");
    assert_eq!(body["recording"]["state"], "stopped");
    assert_eq!(body["microphone"]["state"], "disconnected");
    assert_eq!(body["microphone"]["device"], "USB PnP Sound Device");
    assert_eq!(body["microphone"]["capture_rate_hz"], 48_000);
    assert_eq!(body["microphone"]["channels"], 2);
    assert_eq!(body["detector"]["noise_floor_dbfs"], -63.5);
    assert_eq!(body["detector"]["band_noise_floor_dbfs"], -70.25);
    assert_eq!(body["detector"]["events_total"], 9);
    assert_eq!(body["detector"]["events_discarded_short"], 2);
    assert_eq!(body["detector"]["event_fk_retries"], 1);
    assert_eq!(body["capture"]["xrun_count"], 3);
    assert_eq!(body["capture"]["lost_frames_total"], 480);
    assert_eq!(body["capture"]["clipped_samples_total"], 17);
    assert_eq!(body["storage"]["storage_pressure"], true);
    assert_eq!(body["time"]["time_quality"], "corrected");
    assert_eq!(body["time"]["time_synced"], true);
    assert_eq!(body["time"]["clock_drift_ppm"], 4.25);
    // Not silent, and the pipeline is running: the only case that may say `ok`.
    assert_eq!(body["signal_state"], "ok");

    harness.metrics.signal_silent.store(true, Ordering::Relaxed);
    let body = get(&harness, "/api/v1/status").await.json();
    assert_eq!(body["signal_state"], "silent");
}

#[tokio::test]
async fn status_counts_recorded_bytes_from_the_database() {
    let harness = harness();
    let mut spec = SegmentSpec::new("seg-bytes", utc(1_735_689_600), "recordings/2025/01/01/a.wav");
    spec.size_bytes = 1_000;
    add_segment(&harness, &spec);
    let body = get(&harness, "/api/v1/status").await.json();
    assert_eq!(body["storage"]["used_bytes"], 1_000);
}

// ---------------------------------------------------------------------------
// /days, /segments, /events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn days_lists_only_nights_that_hold_data() {
    let harness = harness();
    // 2025-01-01T12:00:00Z and 2025-01-01T20:00:00Z.
    let mut first = SegmentSpec::new("seg-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav");
    first.frames = 16_000;
    add_segment(&harness, &first);
    let mut second = SegmentSpec::new("seg-2", utc(1_735_761_600), "recordings/2025/01/01/b.wav");
    second.frames = 32_000;
    add_segment(&harness, &second);
    add_event(&harness, "evt-1", "seg-1", utc(1_735_732_810), 0, 3_200, 16_000);

    let body = get(&harness, "/api/v1/days?from=2025-01-01&to=2025-01-03")
        .await
        .json();
    assert_eq!(body["from"], "2025-01-01");
    assert_eq!(body["to"], "2025-01-03");
    let days = body["days"].as_array().unwrap();
    assert_eq!(days.len(), 1, "only the night with data is listed: {days:?}");
    assert_eq!(days[0]["date"], "2025-01-01");
    assert_eq!(days[0]["segment_count"], 2);
    assert_eq!(days[0]["event_count"], 1);
    assert_eq!(days[0]["recorded_ms"], 3_000);
    assert_eq!(days[0]["lost_frames_total"], 0);
}

#[tokio::test]
async fn segments_include_a_segment_that_crosses_local_midnight() {
    let harness = harness();
    // 2025-01-01T23:30Z -> 2025-01-02T00:30Z.
    let mut crossing = SegmentSpec::new(
        "seg-cross",
        utc(1_735_774_200),
        "recordings/2025/01/01/cross.wav",
    );
    crossing.ended = Some(utc(1_735_777_800));
    crossing.gap_count = 1;
    crossing.lost_frames_total = 80;
    crossing.xrun_count = 1;
    add_segment(&harness, &crossing);
    // 2025-01-02T10:00Z -> 2025-01-02T10:30Z.
    add_segment(
        &harness,
        &SegmentSpec::new("seg-morning", utc(1_735_812_000), "recordings/2025/01/02/m.wav"),
    );

    let first = get(&harness, "/api/v1/segments?date=2025-01-01").await.json();
    assert_eq!(first["date"], "2025-01-01");
    let segments = first["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0]["id"], "seg-cross");

    let second = get(&harness, "/api/v1/segments?date=2025-01-02").await.json();
    let segments = second["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 2, "the crossing segment belongs to both days");

    let crossing = &second["segments"][0];
    assert_eq!(crossing["id"], "seg-cross");
    assert_eq!(crossing["capture_rate_hz"], 16_000);
    assert_eq!(crossing["channels"], 1);
    assert_eq!(crossing["sample_format"], "S16_LE");
    assert_eq!(crossing["frame_count"], 16_000);
    assert_eq!(crossing["size_bytes"], 44);
    assert_eq!(crossing["duration_ms"], 1_000);
    assert_eq!(crossing["status"], "complete");
    assert_eq!(crossing["time_quality"], "synced");
    assert_eq!(crossing["end_reason"], "duration");
    assert_eq!(crossing["lost_frames_total"], 80);
    assert_eq!(crossing["gap_count"], 1);
    assert_eq!(crossing["xrun_count"], 1);
    assert!(crossing["clock_drift_ppm"].is_null());
    assert!(crossing["ended_at_utc"].is_string());
    assert_eq!(crossing["audio_url"], "/api/v1/segments/seg-cross/audio");
}

#[tokio::test]
async fn date_boundaries_follow_the_configured_timezone() {
    let harness = harness_with_timezone("America/New_York");
    // 2025-01-01T03:00Z is 2024-12-31 22:00 in New York.
    let mut spec = SegmentSpec::new(
        "seg-evening",
        utc(1_735_700_400),
        "recordings/2025/01/01/evening.wav",
    );
    spec.ended = Some(utc(1_735_700_700));
    add_segment(&harness, &spec);

    let previous = get(&harness, "/api/v1/segments?date=2024-12-31").await.json();
    assert_eq!(previous["segments"].as_array().unwrap().len(), 1);
    let next = get(&harness, "/api/v1/segments?date=2025-01-01").await.json();
    assert_eq!(next["segments"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn an_in_progress_segment_reports_a_null_end() {
    let harness = harness();
    let mut spec = SegmentSpec::new("seg-live", utc(1_735_732_800), "recordings/2025/01/01/live.wav");
    spec.ended = None;
    spec.status = SegmentStatus::Recording;
    add_segment(&harness, &spec);
    let body = get(&harness, "/api/v1/segments?date=2025-01-01").await.json();
    assert_eq!(body["segments"][0]["status"], "recording");
    assert!(body["segments"][0]["ended_at_utc"].is_null());
}

#[tokio::test]
async fn events_carry_offsets_and_the_segment_capture_rate() {
    let harness = harness();
    let mut spec = SegmentSpec::new("seg-48k", utc(1_735_732_800), "recordings/2025/01/01/a.wav");
    spec.rate = 48_000;
    add_segment(&harness, &spec);
    add_event(&harness, "evt-48k", "seg-48k", utc(1_735_732_812), 4_800, 9_600, 48_000);

    let body = get(&harness, "/api/v1/events?date=2025-01-01").await.json();
    assert_eq!(body["date"], "2025-01-01");
    assert_eq!(body["capture_rate_hz"], 16_000);
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["id"], "evt-48k");
    assert_eq!(event["segment_id"], "seg-48k");
    assert_eq!(event["capture_rate_hz"], 48_000);
    assert_eq!(event["start_offset_frames"], 4_800);
    assert_eq!(event["end_offset_frames"], 9_600);
    assert_eq!(event["duration_ms"], 100);
    assert_eq!(event["rule_score"], 0.75);
    assert_eq!(event["detector_version"], "rule-v1");
    assert_eq!(event["peak_dbfs"], -12.5);
    assert_eq!(event["mean_level_dbfs"], -31.0);
    assert_eq!(event["mean_band_ratio"], 0.6);
    assert_eq!(event["noise_floor_dbfs"], -62.0);
    assert_eq!(event["end_reason"], "normal");
    assert_eq!(event["review_status"], "unreviewed");
    assert_eq!(event["continued"], false);
    assert!(event["started_at_utc"].is_string());
    assert!(event["ended_at_utc"].is_string());
}

#[tokio::test]
async fn event_detail_returns_the_event_and_its_segment() {
    let harness = harness();
    add_segment(
        &harness,
        &SegmentSpec::new("seg-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav"),
    );
    add_event(&harness, "evt-1", "seg-1", utc(1_735_732_812), 0, 3_200, 16_000);

    let response = get(&harness, "/api/v1/events/evt-1").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();
    assert_eq!(body["event"]["id"], "evt-1");
    assert_eq!(body["event"]["capture_rate_hz"], 16_000);
    assert_eq!(body["segment"]["id"], "seg-1");
    assert_eq!(body["segment"]["audio_url"], "/api/v1/segments/seg-1/audio");

    let missing = get(&harness, "/api/v1/events/nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn gaps_are_listed_with_their_offset_and_estimate_source() {
    let harness = harness();
    let mut spec = SegmentSpec::new("seg-gap", utc(1_735_732_800), "recordings/2025/01/01/a.wav");
    spec.gap_count = 1;
    spec.lost_frames_total = 80;
    add_segment(&harness, &spec);
    {
        let db = harness.database.lock().unwrap();
        db.insert_gap(
            "seg-gap",
            &GapRecord {
                offset_frames: 16_000,
                lost_frames: 80,
                kind: GapKind::Xrun,
                estimate_source: EstimateSource::Timestamp,
                at_utc: utc(1_735_732_801),
            },
        )
        .unwrap();
    }

    let response = get(&harness, "/api/v1/segments/seg-gap/gaps").await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();
    assert_eq!(body["segment_id"], "seg-gap");
    assert_eq!(body["capture_rate_hz"], 16_000);
    let gaps = body["gaps"].as_array().unwrap();
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0]["offset_frames"], 16_000);
    assert_eq!(gaps[0]["lost_frames"], 80);
    assert_eq!(gaps[0]["kind"], "xrun");
    assert_eq!(gaps[0]["estimate_source"], "timestamp");
    assert!(gaps[0]["at_utc"].is_string());

    let missing = get(&harness, "/api/v1/segments/nope/gaps").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn dates_without_data_return_empty_arrays() {
    let harness = harness();
    let segments = get(&harness, "/api/v1/segments?date=2030-05-05").await;
    assert_eq!(segments.status, StatusCode::OK);
    assert_eq!(segments.json()["segments"].as_array().unwrap().len(), 0);

    let events = get(&harness, "/api/v1/events?date=2030-05-05").await;
    assert_eq!(events.status, StatusCode::OK);
    assert_eq!(events.json()["events"].as_array().unwrap().len(), 0);

    let days = get(&harness, "/api/v1/days?from=2030-05-05&to=2030-05-07").await;
    assert_eq!(days.status, StatusCode::OK);
    assert_eq!(days.json()["days"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn invalid_parameters_use_the_error_envelope() {
    let harness = harness();
    for uri in [
        "/api/v1/segments",
        "/api/v1/segments?date=",
        "/api/v1/segments?date=2025-13-01",
        "/api/v1/segments?date=01-01-2025",
        "/api/v1/events?date=not-a-date",
        "/api/v1/days?from=2025-01-01",
        "/api/v1/days?to=2025-01-01",
        "/api/v1/days?from=2025-01-02&to=2025-01-01",
        "/api/v1/days?from=2025-01-01&to=2030-01-01",
    ] {
        let response = get(&harness, uri).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{uri}");
        let body = response.json();
        assert_eq!(body["error"]["code"], "invalid_parameter", "{uri}");
        assert!(body["error"]["message"].is_string(), "{uri}");
    }
}

#[tokio::test]
async fn unknown_routes_and_methods_use_the_error_envelope() {
    let harness = harness();
    let missing = get(&harness, "/api/v1/nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/status")
        .body(Body::empty())
        .unwrap();
    let wrong_method = send(&harness, request).await;
    assert_eq!(wrong_method.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(wrong_method.json()["error"]["code"], "method_not_allowed");
}

// ---------------------------------------------------------------------------
// PATCH /api/v1/events/{id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patch_accepts_the_three_labels_and_returns_the_event() {
    let harness = harness();
    add_segment(
        &harness,
        &SegmentSpec::new("seg-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav"),
    );
    add_event(&harness, "evt-1", "seg-1", utc(1_735_732_812), 0, 3_200, 16_000);

    for (label, expected) in [
        ("snore", ReviewStatus::Snore),
        ("not_snore", ReviewStatus::NotSnore),
        ("unreviewed", ReviewStatus::Unreviewed),
    ] {
        let response = patch(
            &harness,
            "/api/v1/events/evt-1",
            &format!("{{\"review_status\":\"{label}\"}}"),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{label}");
        let body = response.json();
        assert_eq!(body["id"], "evt-1");
        assert_eq!(body["review_status"], label);
        assert_eq!(body["capture_rate_hz"], 16_000);
        let stored = harness
            .database
            .lock()
            .unwrap()
            .event("evt-1")
            .unwrap()
            .unwrap();
        assert_eq!(stored.review_status, expected.as_str());
    }
}

#[tokio::test]
async fn patch_rejects_anything_else() {
    let harness = harness();
    add_segment(
        &harness,
        &SegmentSpec::new("seg-1", utc(1_735_732_800), "recordings/2025/01/01/a.wav"),
    );
    add_event(&harness, "evt-1", "seg-1", utc(1_735_732_812), 0, 3_200, 16_000);

    for body in [
        "{\"review_status\":\"maybe\"}",
        "{\"review_status\":\"\"}",
        "{\"review_status\":\"SNORE\"}",
        "{}",
        "{\"review_status\":3}",
        "not json",
    ] {
        let response = patch(&harness, "/api/v1/events/evt-1", body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{body}");
        let json = response.json();
        assert!(json["error"]["code"].is_string(), "{body}");
    }

    // A rejected patch must not have changed the stored label.
    let stored = harness
        .database
        .lock()
        .unwrap()
        .event("evt-1")
        .unwrap()
        .unwrap();
    assert_eq!(stored.review_status, "unreviewed");

    let missing = patch(
        &harness,
        "/api/v1/events/nope",
        "{\"review_status\":\"snore\"}",
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");
}

// ---------------------------------------------------------------------------
// Audio range endpoint
// ---------------------------------------------------------------------------

const PAYLOAD: usize = 100;

fn audio_segment(harness: &Harness, id: &'static str, status: SegmentStatus) -> Vec<u8> {
    let relative = format!("recordings/2025/01/01/{id}.wav");
    let bytes = write_recording(harness, &relative, PAYLOAD);
    let mut spec = SegmentSpec::new(id, utc(1_735_732_800), &relative);
    spec.size_bytes = bytes.len() as u64;
    spec.status = status;
    add_segment(harness, &spec);
    bytes
}

#[tokio::test]
async fn audio_returns_the_whole_file_with_range_metadata() {
    let harness = harness();
    let bytes = audio_segment(&harness, "seg-audio", SegmentStatus::Complete);
    let response = get(&harness, "/api/v1/segments/seg-audio/audio").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header(header::CONTENT_TYPE).unwrap(), "audio/wav");
    assert_eq!(response.header(header::ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(
        response.header(header::CONTENT_LENGTH).unwrap(),
        bytes.len().to_string()
    );
    assert_eq!(
        response.header(header::ETAG).unwrap(),
        format!("\"seg-audio-{}\"", bytes.len())
    );
    assert!(response.header(header::CONTENT_RANGE).is_none());
    assert_eq!(response.body, bytes);
}

#[tokio::test]
async fn audio_honours_the_three_range_forms() {
    let harness = harness();
    let bytes = audio_segment(&harness, "seg-range", SegmentStatus::Complete);
    let total = bytes.len();
    let uri = "/api/v1/segments/seg-range/audio";

    for (range, expected_start, expected_end) in [
        ("bytes=0-9", 0usize, 9usize),
        ("bytes=10-", 10, total - 1),
        ("bytes=-4", total - 4, total - 1),
        ("bytes=0-9999", 0, total - 1),
    ] {
        let response = get_range(&harness, uri, range).await;
        assert_eq!(response.status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(
            response.header(header::CONTENT_RANGE).unwrap(),
            format!("bytes {expected_start}-{expected_end}/{total}"),
            "{range}"
        );
        assert_eq!(
            response.header(header::CONTENT_LENGTH).unwrap(),
            (expected_end - expected_start + 1).to_string(),
            "{range}"
        );
        assert_eq!(
            response.body,
            bytes[expected_start..=expected_end].to_vec(),
            "{range}"
        );
        assert_eq!(
            response.header(header::ETAG).unwrap(),
            format!("\"seg-range-{total}\"")
        );
    }
}

#[tokio::test]
async fn audio_ignores_multi_ranges_and_rejects_unsatisfiable_ones() {
    let harness = harness();
    let bytes = audio_segment(&harness, "seg-multi", SegmentStatus::Complete);
    let uri = "/api/v1/segments/seg-multi/audio";

    // Multiple ranges are not supported: RFC 9110 allows the full response.
    for range in ["bytes=0-1,5-6", "bytes=0-1, 5-6", "items=0-1", "bytes=5-1"] {
        let response = get_range(&harness, uri, range).await;
        assert_eq!(response.status, StatusCode::OK, "{range}");
        assert_eq!(response.body, bytes, "{range}");
    }

    let response = get_range(&harness, uri, "bytes=999999-").await;
    assert_eq!(response.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.header(header::CONTENT_RANGE).unwrap(),
        format!("bytes */{}", bytes.len())
    );
    assert_eq!(response.header(header::ACCEPT_RANGES).unwrap(), "bytes");
    let body = response.json();
    assert_eq!(body["error"]["code"], "range_not_satisfiable");
}

#[tokio::test]
async fn audio_supports_head_with_and_without_a_range() {
    let harness = harness();
    let bytes = audio_segment(&harness, "seg-head", SegmentStatus::Complete);
    let uri = "/api/v1/segments/seg-head/audio";

    let response = head(&harness, uri, None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.is_empty(), "HEAD carries no body");
    assert_eq!(
        response.header(header::CONTENT_LENGTH).unwrap(),
        bytes.len().to_string()
    );
    assert_eq!(response.header(header::ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(response.header(header::CONTENT_TYPE).unwrap(), "audio/wav");

    let response = head(&harness, uri, Some("bytes=0-9")).await;
    assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
    assert!(response.body.is_empty());
    assert_eq!(response.header(header::CONTENT_LENGTH).unwrap(), "10");
    assert_eq!(
        response.header(header::CONTENT_RANGE).unwrap(),
        format!("bytes 0-9/{}", bytes.len())
    );
}

#[tokio::test]
async fn audio_reports_404_409_and_410() {
    let harness = harness();
    audio_segment(&harness, "seg-live", SegmentStatus::Recording);
    audio_segment(&harness, "seg-gone", SegmentStatus::Deleted);

    let missing = get(&harness, "/api/v1/segments/nope/audio").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");

    let recording = get(&harness, "/api/v1/segments/seg-live/audio").await;
    assert_eq!(recording.status, StatusCode::CONFLICT);
    assert_eq!(recording.json()["error"]["code"], "audio_not_ready");

    let deleted = get(&harness, "/api/v1/segments/seg-gone/audio").await;
    assert_eq!(deleted.status, StatusCode::GONE);
    assert_eq!(deleted.json()["error"]["code"], "audio_expired");

    // A `complete` row whose file vanished is a 404, never a fake stream.
    let mut orphan = SegmentSpec::new(
        "seg-orphan",
        utc(1_735_732_800),
        "recordings/2025/01/01/orphan.wav",
    );
    orphan.size_bytes = 144;
    add_segment(&harness, &orphan);
    let orphaned = get(&harness, "/api/v1/segments/seg-orphan/audio").await;
    assert_eq!(orphaned.status, StatusCode::NOT_FOUND);
    assert_eq!(orphaned.json()["error"]["code"], "audio_missing");
}

#[tokio::test]
async fn audio_serves_a_file_whose_size_disagrees_with_the_database() {
    let harness = harness();
    let bytes = audio_segment(&harness, "seg-drift", SegmentStatus::Complete);
    // Corrupt the stored size the way a half-written completion could.
    {
        let db = harness.database.lock().unwrap();
        db.connection()
            .execute(
                "UPDATE recording_segments SET size_bytes=1 WHERE id='seg-drift'",
                [],
            )
            .unwrap();
    }
    let response = get(&harness, "/api/v1/segments/seg-drift/audio").await;
    assert_eq!(response.status, StatusCode::OK);
    // The real file length wins so ranges stay consistent with the bytes sent.
    assert_eq!(
        response.header(header::CONTENT_LENGTH).unwrap(),
        bytes.len().to_string()
    );
    assert_eq!(
        response.header(header::ETAG).unwrap(),
        format!("\"seg-drift-{}\"", bytes.len())
    );
    assert_eq!(response.body, bytes);
}

// ---------------------------------------------------------------------------
// Portal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn portal_serves_index_and_assets() {
    let harness = harness();
    let web = harness.root.join("web");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("index.html"), b"<html>portal</html>").unwrap();
    std::fs::write(web.join("assets/app.js"), b"console.log('portal')").unwrap();

    let index = get(&harness, "/").await;
    assert_eq!(index.status, StatusCode::OK);
    assert_eq!(index.header(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");
    assert_eq!(index.body, b"<html>portal</html>");

    for uri in ["/web", "/web/"] {
        let response = get(&harness, uri).await;
        assert_eq!(response.status, StatusCode::OK, "{uri}");
        assert_eq!(response.body, b"<html>portal</html>", "{uri}");
    }

    let asset = get(&harness, "/web/assets/app.js").await;
    assert_eq!(asset.status, StatusCode::OK);
    assert_eq!(asset.header(header::CONTENT_TYPE).unwrap(), "text/javascript; charset=utf-8");
    assert_eq!(asset.body, b"console.log('portal')");

    let missing = get(&harness, "/web/assets/nope.js").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn portal_rejects_path_traversal() {
    let harness = harness();
    let web = harness.root.join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(web.join("index.html"), b"<html>portal</html>").unwrap();
    std::fs::write(harness.root.join("secret.txt"), b"top secret").unwrap();

    for uri in [
        "/web/%2e%2e/secret.txt",
        "/web/%2e%2e%2fsecret.txt",
        "/web/assets/%2e%2e/%2e%2e/secret.txt",
    ] {
        let response = get(&harness, uri).await;
        assert_ne!(response.status, StatusCode::OK, "{uri} must not be served");
        assert!(
            !String::from_utf8_lossy(&response.body).contains("top secret"),
            "{uri} leaked a file outside the portal root"
        );
    }
}

#[tokio::test]
async fn portal_without_a_web_directory_is_an_explicit_error() {
    let harness = harness();
    assert!(!harness.root.join("web").exists());
    let response = get(&harness, "/").await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    let body = response.json();
    assert_eq!(body["error"]["code"], "portal_unavailable");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("web"),
        "the message must name the expected directory"
    );
}

// ---------------------------------------------------------------------------
// Concurrency and real listening socket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_partially_read_audio_response_does_not_block_other_requests() {
    let harness = harness();
    let relative = "recordings/2025/01/01/big.wav";
    let bytes = write_recording(&harness, relative, 4 * 1024 * 1024);
    let mut spec = SegmentSpec::new("seg-big", utc(1_735_732_800), relative);
    spec.size_bytes = bytes.len() as u64;
    add_segment(&harness, &spec);

    let request = Request::builder()
        .uri("/api/v1/segments/seg-big/audio")
        .body(Body::empty())
        .unwrap();
    let response = harness.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Consume one chunk and leave the 4 MiB response open: the rest is still
    // sitting on disk, not in memory.
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(
        first.len() <= 64 * 1024,
        "the stream must be chunked, got {} bytes",
        first.len()
    );

    // Another request is served while that stream is half-read.
    let status = get(&harness, "/api/v1/status").await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.json()["service"]["state"], "ok");
    let segments = get(&harness, "/api/v1/segments?date=2025-01-01").await;
    assert_eq!(segments.status, StatusCode::OK);
    assert_eq!(segments.json()["segments"].as_array().unwrap().len(), 1);

    // Drain the remainder to prove the open range really was served in full.
    let mut received = first.len();
    while let Some(chunk) = stream.next().await {
        received += chunk.unwrap().len();
    }
    assert_eq!(received, bytes.len());
}

#[tokio::test]
async fn serve_answers_over_a_real_tcp_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let port = free_port();
    let harness = harness_with(tempfile::tempdir().unwrap(), "UTC", port);
    let state = Arc::clone(&harness.state);
    let server = tokio::spawn(async move { serve(state).await });

    let address = format!("127.0.0.1:{port}");
    let mut stream = None;
    for _ in 0..200 {
        match tokio::net::TcpStream::connect(&address).await {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    let mut stream = stream.expect("serve() never accepted a connection");

    stream
        .write_all(b"GET /api/v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("\"service\""), "{text}");
    assert!(text.contains("\"signal_state\""), "{text}");

    server.abort();
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

#[test]
fn runtime_is_sized_by_the_configured_http_threads() {
    let config = ServerConfig {
        bind_address: "127.0.0.1".to_string(),
        port: 0,
        http_threads: 2,
        timezone: String::new(),
    };
    let runtime = snore_monitor::http_server::build_runtime(&config).unwrap();
    assert_eq!(runtime.metrics().num_workers(), 2);
}

#[test]
fn the_portal_root_defaults_next_to_the_recordings_directory() {
    let dir = tempfile::tempdir().unwrap();
    let recording_dir = dir.path().join("recordings");
    let state = HttpState::new(
        Config {
            server: ServerConfig {
                bind_address: "127.0.0.1".to_string(),
                port: 0,
                http_threads: 1,
                timezone: String::new(),
            },
            audio: AudioConfig::default(),
            detector: DetectorConfig::default(),
            storage: StorageConfig {
                recording_dir: recording_dir.clone(),
                database_path: dir.path().join("db.sqlite"),
                recording_max_bytes: 1 << 30,
                recording_cleanup_target_bytes: 1 << 29,
                recording_reserve_bytes: 1 << 20,
                retention_check_interval_seconds: 60,
            },
        },
        Arc::new(Mutex::new(Database::in_memory().unwrap())),
        Arc::new(Metrics::new()),
        Arc::new(RuntimeStatus::new()),
    );
    assert_eq!(state.web_root(), dir.path().join("web"));
    assert_eq!(
        state
            .with_web_root(Path::new("/opt/snore/web"))
            .web_root(),
        Path::new("/opt/snore/web")
    );
}
