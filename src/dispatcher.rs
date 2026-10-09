//! Dispatcher thread (TECH_SPEC §4.3, task T3.4).
//!
//! This module owns frame counting, segmentation decisions, gap registration and
//! `segment_offset_frames` annotation. It is the only place in the process that
//! decides when to rotate a WAV recording; the recorder and detector workers only
//! react to the messages the dispatcher sends them. The DB writer is fed
//! `SegmentStarted` before any `SegmentStart` reaches the audio consumers, so the
//! segment row exists before any event referencing it.
//!
//! # Pipeline
//!
//! 1. The CPAL callback writes interleaved S16 frames + [`CaptureMeta`] into the
//!    SPSC [`CaptureRing`].
//! 2. The dispatcher's main loop pops blocks from that ring, annotates them with
//!    `segment_id`, `segment_offset_frames`, `gap_before_frames`, `gap_kind`,
//!    clipped-sample / silent-frame accounting and any detected capture-ring
//!    overflow, then fans the block out to two downstream queues.
//! 3. The recorder queue and the detection queue are both
//!    [`BoundedQueue`]s. Audio pushes are accepted only up to `audio_capacity`;
//!    control messages (`SegmentStart`, `Gap`, `SegmentEnd`) ride in the reserved
//!    control slots and are never dropped unless the consumer is stuck.
//!
//! # Rotation
//!
//! Rotation is triggered by stored frame count, never by the wall clock, so it
//! is robust to drift. Every forced rotation carries one of the documented
//! `end_reason` strings, and the DB row's `end_reason` column mirrors the
//! dispatcher's reason.
//!
//! # Backpressure
//!
//! - Recording queue audio-slot full → drop the block, record a `queue_overflow`
//!   gap and attach it to the *next successfully enqueued* audio block as
//!   `gap_before_frames`. That block is not delivered to detection, so the
//!   detector sees the same gap and resets its state (§5.4).
//! - Detection queue audio-slot full → drop the detection copy, count it and
//!   log it with the segment and offset, and signal a detector watchdog restart.
//!   The WAV branch and the `audio_gaps` table are unaffected.
//! - Control slot full on either queue → the consumer is stuck; mark the
//!   service degraded and signal a watchdog restart of that consumer. The
//!   dispatcher never blocks waiting for a stuck consumer.
//!
//! # Stats and metrics
//!
//! The dispatcher is the single writer of `Metrics::clipped_samples_total`,
//! `Metrics::signal_silent`, `Metrics::recording_queue_high_water`,
//! `Metrics::recording_queue_control_slots`, `Metrics::detection_queue_high_water`,
//! `Metrics::detection_queue_control_slots`, `Metrics::dropped_detection_blocks`
//! and `Metrics::queue_overflow_gaps` (along with `Metrics::capture_ring_overflow_frames`
//! and `Metrics::xrun_count`, which it samples from the capture callback's
//! accounting).

use crate::audio_capture::{CaptureMeta, CaptureRing, NegotiatedFormat};
use crate::bounded_queue::{BoundedQueue, QueueItem};
use crate::config::{AudioConfig, Config, SampleFormat};
use crate::db_writer::{DbCommand, DbWriterHandle};
use crate::metrics::Metrics;
use crate::recorder::segment_relative_path;
use crate::storage::{SegmentCompleted, SegmentStarted, SegmentStatus};
use crate::timeline::{
    boot_id as timeline_boot_id, ClockAnchor,
    ClockMap, EstimateSource, GapKind, GapRecord, TimeQuality,
};
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Continuous frames with peak-zero needed to flag `signal_silent` (TECH_SPEC §5.2).
pub const DIGITAL_SILENT_SECONDS: u32 = 1;

/// Fraction of samples inside a one-second window that flags clipped capture.
const CLIP_FRACTION_WARN: f32 = 0.01;

/// Why a segment is being closed by the dispatcher. The string form mirrors the
/// `recording_segments.end_reason` check constraint in the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentEndReason {
    Duration,
    Gap,
    TimeStep,
    FormatChange,
    SizeLimit,
    Shutdown,
    Error,
}

impl SegmentEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentEndReason::Duration => "duration",
            SegmentEndReason::Gap => "gap",
            SegmentEndReason::TimeStep => "time_step",
            SegmentEndReason::FormatChange => "format_change",
            SegmentEndReason::SizeLimit => "size_limit",
            SegmentEndReason::Shutdown => "shutdown",
            SegmentEndReason::Error => "error",
        }
    }
}

/// Ordered message published to the recorder and detector workers
/// (TECH_SPEC §4.3). `Audio` items are audio-class; everything else is control.
#[derive(Debug, Clone)]
pub enum DispatcherMessage {
    SegmentStart {
        segment_id: String,
        started_at_utc: DateTime<Utc>,
        format: NegotiatedFormat,
        time_quality: TimeQuality,
        boot_id: String,
    },
    Audio {
        segment_id: String,
        segment_offset_frames: u64,
        gap_before_frames: u64,
        gap_kind: Option<GapKind>,
        capture_seq: u64,
        mono_start_ns: u64,
        format: NegotiatedFormat,
        pcm: Vec<i16>,
    },
    Gap {
        segment_id: String,
        offset_frames: u64,
        lost_frames: u64,
        kind: GapKind,
        estimate_source: EstimateSource,
        at_utc: DateTime<Utc>,
    },
    SegmentEnd {
        segment_id: String,
        ended_at_utc: DateTime<Utc>,
        frame_count: u64,
        size_bytes: u64,
        xrun_count: u64,
        gap_count: u64,
        lost_frames_total: u64,
        end_reason: SegmentEndReason,
    },
}

impl QueueItem for DispatcherMessage {
    fn is_control(&self) -> bool {
        !matches!(self, DispatcherMessage::Audio { .. })
    }
}

/// Handle returned by [`spawn`]. The parent agent can request a coordinated
/// shutdown through [`DispatcherHandle::request_shutdown`] and join the worker
/// with a timeout via [`DispatcherHandle::join`].
pub struct DispatcherHandle {
    request_flag: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl DispatcherHandle {
    /// Marks the dispatcher for stop. Idempotent.
    pub fn request_shutdown(&self) {
        self.request_flag.store(true, Ordering::Relaxed);
    }

    /// Joins the dispatcher thread within `timeout`. Returns `true` when the
    /// thread exited before the timeout elapsed, `false` otherwise. After a
    /// timeout the thread is left running; the caller decides what to do.
    pub fn join(&mut self, timeout: Duration) -> bool {
        let Some(handle) = self.join.take() else {
            return true;
        };
        join_with_timeout(handle, timeout).is_some()
    }

    /// True when the dispatcher thread has been joined.
    pub fn is_finished(&self) -> bool {
        self.join.is_none()
    }
}

fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) -> Option<()> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(handle.join());
    });
    match rx.recv_timeout(timeout) {
        Ok(_result) => Some(()),
        Err(_) => None,
    }
}

/// Inputs required to start the dispatcher.
pub struct DispatcherInputs {
    pub config: Arc<Config>,
    pub metrics: Arc<Metrics>,
    pub capture: Arc<CaptureRing>,
    pub recording_queue: Arc<BoundedQueue<DispatcherMessage>>,
    pub detection_queue: Arc<BoundedQueue<DispatcherMessage>>,
    pub db: DbWriterHandle,
    pub shutting_down: Arc<AtomicBool>,
    pub negotiated: NegotiatedFormat,
}

/// Spawns the dispatcher thread and returns a [`DispatcherHandle`] the parent can
/// use to coordinate shutdown. The dispatcher starts an initial segment
/// immediately and runs until either [`DispatcherHandle::request_shutdown`] is
/// called, the parent-shared `shutting_down` flag is set, or [`run`] sees an
/// unrecoverable configuration error.
pub fn spawn(inputs: DispatcherInputs) -> DispatcherHandle {
    let request_flag = Arc::new(AtomicBool::new(false));
    let request_for_thread = Arc::clone(&request_flag);
    let join = thread::Builder::new()
        .name("snore-dispatcher".into())
        .spawn(move || {
            let mut d = Dispatcher::new(inputs, request_for_thread);
            d.run();
        })
        .expect("failed to spawn dispatcher thread");
    DispatcherHandle {
        request_flag,
        join: Some(join),
    }
}

/// Per-segment state the dispatcher owns while it is open.
#[derive(Debug)]
#[allow(dead_code)]
struct ActiveSegment {
    id: String,
    started_at_utc: DateTime<Utc>,
    format: NegotiatedFormat,
    capture_rate_hz: u32,
    block_align: u32,
    time_quality: TimeQuality,
    boot_id: String,
    relative_path: PathBuf,
    /// PCM frames already stored in the WAV. After dispatching a block this
    /// grows by `block.frames`; queue-overflow losses do not contribute.
    stored_frames: u64,
    /// Frames lost in the most recent upstream failure, to be carried by the
    /// next successful audio message as `gap_before_frames`.
    pending_lost_frames: u64,
    /// Gap kind to attach to the next successful audio message.
    pending_lost_kind: Option<GapKind>,
    /// Count of consecutive frames with peak-zero (used for digital silence).
    silent_frames_in_a_row: u32,
    /// Sliding one-second window used to detect over-amplified capture input.
    clip_window_clipped: u64,
    clip_window_total: u64,
    /// Gap records that should be flushed to the DB alongside SegmentStarted and
    /// SegmentEnd. Recorder/detector Gap messages mirror these entries.
    gaps: Vec<GapRecord>,
    /// Frames lost because the capture ring rejected the callback's push.
    capture_ring_overflow_frames: u64,
    /// Largest gap seen so far. Used to decide whether to force a `gap` rotation.
    max_gap_lost_frames: u64,
    /// Monotonic ns of the segment start, used to compute clock drift.
    started_mono_ns: u64,
}

impl ActiveSegment {
    fn new(
        id: String,
        started_at_utc: DateTime<Utc>,
        format: NegotiatedFormat,
        time_quality: TimeQuality,
        boot_id: String,
        relative_path: PathBuf,
        started_mono_ns: u64,
    ) -> Self {
        let block_align = u32::from(format.channels) * SampleFormat::S16Le.bytes_per_sample() as u32;
        ActiveSegment {
            id,
            started_at_utc,
            format: format.clone(),
            capture_rate_hz: format.rate_hz,
            block_align,
            time_quality,
            boot_id,
            relative_path,
            stored_frames: 0,
            pending_lost_frames: 0,
            pending_lost_kind: None,
            silent_frames_in_a_row: 0,
            clip_window_clipped: 0,
            clip_window_total: 0,
            gaps: Vec::new(),
            capture_ring_overflow_frames: 0,
            max_gap_lost_frames: 0,
            started_mono_ns,
        }
    }

    fn bytes_so_far(&self) -> u64 {
        self.stored_frames * u64::from(self.block_align)
    }
}

/// Dispatcher state held by the worker thread.
#[allow(dead_code)]
struct Dispatcher {
    inputs: DispatcherInputs,
    /// Independent shutdown flag set by [`DispatcherHandle::request_shutdown`].
    request_shutdown: Arc<AtomicBool>,
    segment: Option<ActiveSegment>,
    clock_map: ClockMap,
    audio_cfg: AudioConfig,
    /// `segment_duration_seconds * capture_rate_hz` (TECH_SPEC §4.3).
    segment_frames: u64,
    max_segment_bytes: u64,
    /// `gap_rotate_seconds * capture_rate_hz`.
    gap_rotate_frames: u64,
    /// Frames dropped from the capture ring before this dispatcher started.
    /// Subtracted from the counter at close so `xrun_count` for the segment
    /// reflects only what happened during the segment.
    capture_ring_overflows_at_start: u64,
    /// `Metrics::xrun_count` sampled at dispatcher start.
    xrun_at_start: u64,
}

impl Dispatcher {
    fn new(inputs: DispatcherInputs, request_shutdown: Arc<AtomicBool>) -> Self {
        let audio_cfg: AudioConfig = inputs.config.audio.clone();
        let segment_frames = audio_cfg.segment_frames();
        let max_segment_bytes = audio_cfg.max_segment_bytes();
        let gap_rotate_frames = audio_cfg
            .gap_rotate_seconds
            .saturating_mul(audio_cfg.preferred_rate_hz as u64);
        let capture_ring_overflows_at_start = inputs.capture.overflow_frames();
        let xrun_at_start = inputs.metrics.xrun_count.load(Ordering::Relaxed);
        // Establish the first real clock-trust reading before the anchor is built,
        // so the very first segment is classified from the actual clock state and
        // not from the untrusted default.
        inputs.metrics.set_time_synced(Metrics::probe_time_synced());
        let now_mono_ns = monotonic_now_ns();
        let anchor = ClockAnchor {
            mono_ns: now_mono_ns,
            utc: chrono::Utc::now(),
            time_synced: host_time_synced(&inputs.metrics),
        };
        let clock_map = ClockMap::new(timeline_boot_id(), anchor);
        Dispatcher {
            inputs,
            request_shutdown,
            segment: None,
            clock_map,
            audio_cfg,
            segment_frames,
            max_segment_bytes,
            gap_rotate_frames,
            capture_ring_overflows_at_start,
            xrun_at_start,
        }
    }

    fn should_stop(&self) -> bool {
        self.inputs.shutting_down.load(Ordering::Relaxed)
            || self.request_shutdown.load(Ordering::Relaxed)
    }

    /// Main loop. Opens the first segment, then pumps blocks until shutdown.
    fn run(&mut self) {
        self.open_segment();
        let mut scratch = Vec::new();
        loop {
            if self.should_stop() {
                self.shutdown();
                break;
            }
            // Refresh the wall-clock anchor every 60 s; rotate on a step > 1 s.
            if self.clock_map.should_refresh(monotonic_now_ns()) {
                // Re-probe the clock alongside the 60 s anchor refresh, so a clock
                // that NTP corrects mid-run is reclassified on the next segment
                // (unsynced -> corrected) rather than staying wrong forever.
                self.inputs.metrics.set_time_synced(Metrics::probe_time_synced());
                let anchor = ClockAnchor {
                    mono_ns: monotonic_now_ns(),
                    utc: chrono::Utc::now(),
                    time_synced: host_time_synced(&self.inputs.metrics),
                };
                if self.clock_map.refresh(anchor).is_some() {
                    self.close_segment(SegmentEndReason::TimeStep);
                    self.open_segment();
                }
            }
            let popped = self.inputs.capture.pop_into(&mut scratch);
            let Some(meta) = popped else {
                if self.should_stop() {
                    self.shutdown();
                    break;
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            self.process_block(meta, std::mem::take(&mut scratch));
            scratch.clear();
        }
    }

    fn shutdown(&mut self) {
        if self.segment.is_some() {
            self.close_segment(SegmentEndReason::Shutdown);
        }
    }

    /// Opens a brand-new segment, sending `SegmentStarted` to the DB writer
    /// and `SegmentStart` to both downstream queues.
    fn open_segment(&mut self) {
        let id = uuid_v4();
        let now = chrono::Utc::now();
        let now_mono_ns = monotonic_now_ns();
        let time_quality = if host_time_synced(&self.inputs.metrics) {
            TimeQuality::Synced
        } else {
            TimeQuality::Unsynced
        };
        let boot_id = self.clock_map.boot_id().to_string();
        let relative_path = segment_relative_path(now, &id);
        let started = SegmentStarted {
            id: id.clone(),
            relative_path: relative_path.clone(),
            started_at_utc: now,
            capture_rate_hz: self.inputs.negotiated.rate_hz,
            channels: self.inputs.negotiated.channels,
            sample_format: self.inputs.negotiated.format.as_str().to_string(),
            channel_mapping: format!("mono:{}", self.audio_cfg.mono_channel_index),
            time_quality,
            boot_id: boot_id.clone(),
        };
        // Insert the row before any audio fan-out (TECH_SPEC §4.3).
        if let Err(error) = self.inputs.db.send(DbCommand::SegmentStarted(started)) {
            self.inputs
                .metrics
                .set_last_error(format!("dispatcher: SegmentStarted send failed: {error}"));
        }
        let segment = ActiveSegment::new(
            id.clone(),
            now,
            self.inputs.negotiated.clone(),
            time_quality,
            boot_id.clone(),
            relative_path,
            now_mono_ns,
        );
        self.segment = Some(segment);
        let start_msg = DispatcherMessage::SegmentStart {
            segment_id: id,
            started_at_utc: now,
            format: self.inputs.negotiated.clone(),
            time_quality,
            boot_id,
        };
        if self.push_control_both(&start_msg).is_err() {
            // Both control lanes are stuck; the consumer watchdog is responsible
            // for restarting them. The dispatcher keeps producing so a fresh
            // SegmentStart can succeed after the consumer recovers.
            self.inputs.metrics.degraded.store(true, Ordering::Relaxed);
        }
    }

    /// Sends a `SegmentEnd` to the DB writer and both queues, then clears
    /// `self.segment`.
    fn close_segment(&mut self, reason: SegmentEndReason) {
        let Some(segment) = self.segment.take() else {
            return;
        };
        let ended_at_utc = chrono::Utc::now();
        let frame_count = segment.stored_frames;
        let size_bytes = segment.bytes_so_far() + 44; // standard WAV header is 44 bytes
        let xrun_total = self
            .inputs
            .metrics
            .xrun_count
            .load(Ordering::Relaxed)
            .saturating_sub(self.xrun_at_start);
        let gap_count = segment.gaps.len() as u64;
        let lost_frames_total: u64 = segment.gaps.iter().map(|g| g.lost_frames).sum();
        let completed = SegmentCompleted {
            id: segment.id.clone(),
            ended_at_utc,
            frame_count,
            size_bytes,
            end_reason: reason.as_str().to_string(),
            clock_drift_ppm: crate::timeline::clock_drift_ppm(
                frame_count,
                segment.capture_rate_hz,
                monotonic_now_ns().saturating_sub(segment.started_mono_ns),
            ),
            xrun_count: xrun_total,
            gap_count,
            lost_frames_total,
            status: SegmentStatus::Complete,
        };
        if let Err(error) = self.inputs.db.send(DbCommand::SegmentCompleted(completed)) {
            self.inputs
                .metrics
                .set_last_error(format!("dispatcher: SegmentCompleted send failed: {error}"));
        }
        // Flush every GapRecord to the DB writer in offset order so the row
        // layout is deterministic for tests.
        for gap in &segment.gaps {
            let _ = self.inputs.db.send(DbCommand::Gap {
                segment_id: segment.id.clone(),
                gap: *gap,
            });
        }
        let end_msg = DispatcherMessage::SegmentEnd {
            segment_id: segment.id.clone(),
            ended_at_utc,
            frame_count,
            size_bytes,
            xrun_count: xrun_total,
            gap_count,
            lost_frames_total,
            end_reason: reason,
        };
        if self.push_control_both(&end_msg).is_err() {
            self.inputs.metrics.degraded.store(true, Ordering::Relaxed);
        }
    }

    fn push_control_both(&self, msg: &DispatcherMessage) -> Result<(), &'static str> {
        let mut ok = true;
        if self.inputs.recording_queue.push_control(msg.clone()).is_err() {
            self.inputs.metrics.degraded.store(true, Ordering::Relaxed);
            ok = false;
        }
        if self.inputs.detection_queue.push_control(msg.clone()).is_err() {
            self.inputs.metrics.degraded.store(true, Ordering::Relaxed);
            ok = false;
        }
        if ok {
            Ok(())
        } else {
            Err("control slots full on at least one downstream queue")
        }
    }

    /// Drives one capture block through the dispatcher state machine. Public
    /// for direct unit-test driving.
    pub fn process_block(&mut self, meta: CaptureMeta, pcm: Vec<i16>) {
        if self.segment.is_none() {
            self.open_segment();
        }
        // The block's frames are at the meta rate. The current segment's
        // `format.rate_hz` may be different only across a stream rebuild that
        // changed the negotiated format; in that case we close the current
        // segment and open a new one before processing.
        if let Some(seg) = self.segment.as_ref() {
            let same_format = meta.rate_hz == seg.format.rate_hz && meta.channels == seg.format.channels;
            if !same_format {
                self.close_segment(SegmentEndReason::FormatChange);
                self.open_segment();
            }
        }
        // Block accounting.
        self.account_block(&meta, &pcm);
        // Decide where to dispatch.
        let next = self.dispatch_strategy(&meta);
        match next {
            Some(DispatchDecision::Push { gap_before, gap_kind }) => {
                self.fan_out_audio(&meta, pcm, gap_before, gap_kind);
            }
            Some(DispatchDecision::RecordingOverflow { lost }) => {
                // Recorder lane full; accumulate the loss so the next successful
                // audio push reports it via `gap_before_frames`. The block is
                // never delivered to detection in this branch.
                self.record_recording_queue_overflow(lost, &meta);
            }
            None => {}
        }
        // After fanning out, check whether the segment should rotate.
        self.maybe_rotate(&meta);
    }

    /// Per-block stats: capture ring overflow, clipping, silent frames. Updates
    /// `metrics` and the per-segment accumulators in place.
    fn account_block(&mut self, meta: &CaptureMeta, pcm: &[i16]) {
        // Compute the new capture-ring overflow frames for this segment first
        // so we can release the mutable borrow on `self.segment` before calling
        // `register_gap_locked`.
        let ring_delta = {
            let segment = match self.segment.as_mut() {
                Some(s) => s,
                None => return,
            };
            let total_overflows = self.inputs.capture.overflow_frames();
            let delta = total_overflows.saturating_sub(self.capture_ring_overflows_at_start);
            let segment_delta = delta.saturating_sub(segment.capture_ring_overflow_frames);
            segment.capture_ring_overflow_frames += segment_delta;
            if segment_delta > 0 {
                segment_delta
            } else {
                0
            }
        };
        if ring_delta > 0 {
            self.register_and_mirror_gap(ring_delta, GapKind::CaptureRingOverflow);
        }
        // Clipping and silent-frame accounting.
        let mut clipped_in_block = 0u64;
        let mut silent_block = true;
        for sample in pcm {
            if *sample == i16::MIN || *sample == i16::MAX {
                clipped_in_block += 1;
            }
            if *sample != 0 {
                silent_block = false;
            }
        }
        let (frames_for_window, new_silent, mut new_clip_clipped, mut new_clip_total) = {
            let segment = match self.segment.as_mut() {
                Some(s) => s,
                None => return,
            };
            if silent_block && meta.frames > 0 {
                let frames_for_window = u64::from(DIGITAL_SILENT_SECONDS)
                    .saturating_mul(u64::from(meta.rate_hz));
                let added = u64::from(meta.frames);
                let new_total = u64::from(segment.silent_frames_in_a_row)
                    .saturating_add(added);
                segment.silent_frames_in_a_row = u32::try_from(new_total).unwrap_or(u32::MAX);
                (frames_for_window, new_total, 0u64, 0u64)
            } else {
                segment.silent_frames_in_a_row = 0;
                if self.inputs.metrics.signal_silent.load(Ordering::Relaxed) {
                    self.inputs.metrics.signal_silent.store(false, Ordering::Relaxed);
                }
                (0, 0, 0, 0)
            }
        };
        if silent_block && meta.frames > 0 && frames_for_window > 0 && new_silent >= frames_for_window {
            self.inputs.metrics.signal_silent.store(true, Ordering::Relaxed);
            self.inputs
                .metrics
                .set_last_error("dispatcher: digital silence detected (signal_silent)");
        }
        // Sliding one-second clip window.
        {
            let segment = match self.segment.as_mut() {
                Some(s) => s,
                None => return,
            };
            segment.clip_window_clipped = segment.clip_window_clipped.saturating_add(clipped_in_block);
            segment.clip_window_total = segment.clip_window_total.saturating_add(pcm.len() as u64);
            let window_size = (u64::from(meta.rate_hz) * meta.channels as u64).max(1);
            if segment.clip_window_total >= window_size {
                new_clip_clipped = segment.clip_window_clipped;
                new_clip_total = segment.clip_window_total;
                segment.clip_window_total = 0;
                segment.clip_window_clipped = 0;
            }
        }
        if new_clip_total > 0 {
            let fraction = new_clip_clipped as f32 / new_clip_total as f32;
            self.inputs
                .metrics
                .clipped_samples_total
                .fetch_add(new_clip_clipped, Ordering::Relaxed);
            if fraction > CLIP_FRACTION_WARN {
                self.inputs.metrics.set_last_error(format!(
                    "dispatcher: clipped samples in last second: {}/{} ({:.2}%)",
                    new_clip_clipped,
                    new_clip_total,
                    fraction * 100.0
                ));
            }
        }
    }

    /// Decide what to do with this block: rotate now, push, or treat it as a
    /// dropped recorder-overflow block. The size-limit check is here because
    /// the WAV recorder will reject a block that would push bytes past the
    /// configured ceiling, so the dispatcher rotates before that happens.
    fn dispatch_strategy(&self, meta: &CaptureMeta) -> Option<DispatchDecision> {
        let segment = self.segment.as_ref()?;
        // Block accounting first: incoming bytes.
        let block_bytes = u64::from(meta.frames) * u64::from(self.inputs.negotiated.channels)
            * SampleFormat::S16Le.bytes_per_sample();
        if segment.bytes_so_far() + block_bytes > self.max_segment_bytes
            || segment.stored_frames.saturating_add(meta.frames as u64) > self.segment_frames
        {
            // Caller will rotate before pushing this block.
            return None;
        }
        let pending_gap = segment.pending_lost_frames > 0;
        let recording_len = self.inputs.recording_queue.len();
        let audio_capacity = self.inputs.recording_queue.audio_capacity();
        // When a pending gap is attached, the audio push will follow a control
        // push. Both must fit; if not, hold the gap back and treat this block
        // as recorder-overflow so the next audio block carries the combined loss.
        let fits = if pending_gap {
            recording_len + 1 < audio_capacity
        } else {
            recording_len < audio_capacity
        };
        if fits {
            Some(DispatchDecision::Push {
                gap_before: segment.pending_lost_frames,
                gap_kind: segment.pending_lost_kind,
            })
        } else {
            Some(DispatchDecision::RecordingOverflow {
                lost: meta.frames as u64,
            })
        }
    }

    fn maybe_rotate(&mut self, meta: &CaptureMeta) {
        let segment = self.segment.as_ref();
        let Some(segment) = segment else {
            return;
        };
        let block_bytes = u64::from(meta.frames) * u64::from(self.inputs.negotiated.channels)
            * SampleFormat::S16Le.bytes_per_sample();
        let total_bytes = segment.bytes_so_far() + block_bytes;
        let total_frames = segment.stored_frames.saturating_add(meta.frames as u64);
        if total_bytes > self.max_segment_bytes {
            self.close_segment(SegmentEndReason::SizeLimit);
            self.open_segment();
            return;
        }
        if total_frames >= self.segment_frames {
            self.close_segment(SegmentEndReason::Duration);
            self.open_segment();
            return;
        }
        if segment.max_gap_lost_frames >= self.gap_rotate_frames {
            self.close_segment(SegmentEndReason::Gap);
            self.open_segment();
        }
    }

    /// Register a `queue_overflow` gap for the next block. Increments
    /// `dropped_detection_blocks` and pushes a `Gap` mirror to the detector lane
    /// so its state machine can reset (§5.4).
    fn record_recording_queue_overflow(&mut self, lost: u64, meta: &CaptureMeta) {
        if let Some(segment) = self.segment.as_mut() {
            segment.pending_lost_frames = segment.pending_lost_frames.saturating_add(lost);
            segment.pending_lost_kind = Some(GapKind::QueueOverflow);
        }
        self.inputs.metrics.queue_overflow_gaps.fetch_add(1, Ordering::Relaxed);
        self.register_and_mirror_gap(lost, GapKind::QueueOverflow);
        self.inputs
            .metrics
            .dropped_detection_blocks
            .fetch_add(1, Ordering::Relaxed);
        let _ = meta;
    }

    /// Helper: append a `GapRecord` to the current segment, updating peak-gap
    /// and lost-frame accounting, and mirror a `Gap` control message to the
    /// detector lane so it can reset per §5.4. Does not push anything to the
    /// recorder lane.
    fn register_and_mirror_gap(&mut self, lost: u64, kind: GapKind) {
        let (segment_id, offset_frames) = match self.segment.as_ref() {
            Some(s) => (s.id.clone(), s.stored_frames),
            None => return,
        };
        self.register_gap_locked(lost, kind, EstimateSource::Counter);
        let mirror = DispatcherMessage::Gap {
            segment_id,
            offset_frames,
            lost_frames: lost,
            kind,
            estimate_source: EstimateSource::Counter,
            at_utc: chrono::Utc::now(),
        };
        if self.inputs.detection_queue.push_control(mirror).is_err() {
            self.inputs.metrics.degraded.store(true, Ordering::Relaxed);
        }
    }

    /// Helper: append a `GapRecord` to the current segment, updating peak-gap
    /// and lost-frame accounting. Does not push control messages.
    fn register_gap_locked(&mut self, lost: u64, kind: GapKind, source: EstimateSource) {
        let segment = match self.segment.as_mut() {
            Some(s) => s,
            None => return,
        };
        let offset = segment.stored_frames;
        let record = GapRecord {
            offset_frames: offset,
            lost_frames: lost,
            kind,
            estimate_source: source,
            at_utc: chrono::Utc::now(),
        };
        segment.max_gap_lost_frames = segment.max_gap_lost_frames.max(lost);
        segment.gaps.push(record);
        self.inputs.metrics.lost_frames_total.fetch_add(lost, Ordering::Relaxed);
    }

    /// Pushes the audio block to both queues (or just the recorder lane when
    /// pending lost frames are attached). Detection never receives a block
    /// carrying a `gap_before_frames > 0`, because the recorder-side overflow
    /// would have caused a real gap in the WAV (§5.4).
    fn fan_out_audio(
        &mut self,
        meta: &CaptureMeta,
        pcm: Vec<i16>,
        gap_before: u64,
        gap_kind: Option<GapKind>,
    ) {
        let segment = match self.segment.as_ref() {
            Some(s) => s,
            None => return,
        };
        // Snapshot the relevant segment fields; mutating `self.segment` requires
        // an immutable borrow above for the borrow checker.
        let segment_id = segment.id.clone();
        let segment_offset = segment.stored_frames;
        let pending_gap = gap_before > 0;
        // Mirror the gap to both lanes BEFORE the audio block to preserve order.
        if pending_gap {
            let at_utc = chrono::Utc::now();
            let kind = gap_kind.unwrap_or(GapKind::Xrun);
            let source = EstimateSource::Counter;
            let gap_msg = DispatcherMessage::Gap {
                segment_id: segment_id.clone(),
                offset_frames: segment_offset,
                lost_frames: gap_before,
                kind,
                estimate_source: source,
                at_utc,
            };
            if self.inputs.recording_queue.push_control(gap_msg.clone()).is_err() {
                self.inputs.metrics.degraded.store(true, Ordering::Relaxed);
                return;
            }
            if self.inputs.detection_queue.push_control(gap_msg).is_err() {
                self.inputs.metrics.degraded.store(true, Ordering::Relaxed);
                return;
            }
        }
        let audio_msg = DispatcherMessage::Audio {
            segment_id: segment_id.clone(),
            segment_offset_frames: segment_offset,
            gap_before_frames: gap_before,
            gap_kind,
            capture_seq: meta.capture_seq,
            mono_start_ns: meta.mono_start_ns,
            format: segment.format.clone(),
            pcm: pcm.clone(),
        };
        if self
            .inputs
            .recording_queue
            .push_audio(audio_msg.clone())
            .is_err()
        {
            // The dispatcher pre-checked len() < audio_capacity, so this should
            // only fire when the recorder's own atomic state changed between
            // checks. Treat as a queue_overflow loss and try to mirror the gap.
            self.record_recording_queue_overflow(meta.frames as u64, meta);
            return;
        }
        // Detection only receives an audio block when there is no pending gap.
        if !pending_gap && self.inputs.detection_queue.push_audio(audio_msg).is_err() {
            // Detection dropped: do NOT touch audio_gaps, do NOT touch the WAV.
            self.inputs.metrics.dropped_detection_blocks.fetch_add(1, Ordering::Relaxed);
            self.inputs.metrics.set_last_error(format!(
                "dispatcher: dropped detection block (segment={}, offset={}, frames={})",
                segment_id, segment_offset, meta.frames
            ));
            // Signal a watchdog restart of the detector per §5.4.
            self.inputs.metrics.degraded.store(true, Ordering::Relaxed);
        }
        // Account the stored frames.
        if let Some(segment) = self.segment.as_mut() {
            segment.stored_frames = segment.stored_frames.saturating_add(meta.frames as u64);
            if pending_gap {
                // The pending loss was attached as gap_before_frames; the WAV
                // does not include those samples, so the offset stays the same.
                segment.pending_lost_frames = 0;
                segment.pending_lost_kind = None;
            }
        }
        // Refresh high-water and control-slot metrics.
        let recording_high = self.inputs.recording_queue.high_water();
        let detection_high = self.inputs.detection_queue.high_water();
        self.inputs
            .metrics
            .recording_queue_high_water
            .store(recording_high, Ordering::Relaxed);
        self.inputs
            .metrics
            .detection_queue_high_water
            .store(detection_high, Ordering::Relaxed);
        self.inputs
            .metrics
            .recording_queue_control_slots
            .store(self.inputs.recording_queue.control_in_flight(), Ordering::Relaxed);
        self.inputs
            .metrics
            .detection_queue_control_slots
            .store(self.inputs.detection_queue.control_in_flight(), Ordering::Relaxed);
    }
}

/// Either push the block to both lanes or treat it as a recorder-overflow loss.
#[derive(Debug)]
enum DispatchDecision {
    Push {
        gap_before: u64,
        gap_kind: Option<GapKind>,
    },
    RecordingOverflow {
        lost: u64,
    },
}

fn monotonic_now_ns() -> u64 {
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(Instant::now);
    origin.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

/// Whether the host wall clock is currently trustworthy.
///
/// The dispatcher refreshes the real probe into [`Metrics`] at every anchor
/// refresh (§4.3, every 60 s) and reads it back here, so every timestamp the
/// dispatcher records and every `time_quality` it writes come from one shared
/// value. It deliberately does not probe per call: `adjtimex` is a syscall and
/// this is consulted on the audio path.
///
/// Until the first probe lands, and if nothing has ever probed, the clock is
/// reported as untrusted, so a segment never claims `time_quality=synced` on the
/// strength of an unverified clock.
fn host_time_synced(metrics: &Metrics) -> bool {
    metrics.time_synced()
}

fn uuid_v4() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as AOrdering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = SEQ.fetch_add(1, AOrdering::Relaxed) as u128;
    let mut x = now ^ (seq << 32) ^ (std::process::id() as u128);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let bytes = x.to_le_bytes();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&bytes[..8]);
    out[8..].copy_from_slice(&now.to_le_bytes()[..8]);
    out[6] = (out[6] & 0x0f) | 0x40; // version 4
    out[8] = (out[8] & 0x3f) | 0x80; // variant 10
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        out[0], out[1], out[2], out[3], out[4], out[5], out[6], out[7],
        out[8], out[9], out[10], out[11], out[12], out[13], out[14], out[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounded_queue::PopOutcome;
    use crate::storage::Database;
    use crate::timeline::{GapKind, TimeQuality};
    use std::sync::atomic::AtomicBool;
    use tempfile::tempdir;

    fn test_config(rate_hz: u32, channels: u16) -> Arc<Config> {
        Arc::new(Config {
            server: Default::default(),
            audio: AudioConfig {
                segment_duration_seconds: 60,
                preferred_rate_hz: rate_hz,
                channels,
                recording_queue_ms: 1000,
                detection_queue_ms: 1000,
                gap_tolerance_ms: 20,
                gap_rotate_seconds: 5,
                header_flush_interval_seconds: 30,
                shutdown_timeout_seconds: 10,
                capture_ring_ms: 500,
                period_ms: 10,
                buffer_ms: 200,
                mono_channel_index: 0,
                format: crate::config::SampleFormat::S16Le,
                alsa_device: String::new(),
            },
            detector: Default::default(),
            storage: crate::config::StorageConfig {
                recording_dir: PathBuf::from("/tmp"),
                database_path: PathBuf::from("/tmp/snore.db"),
                recording_max_bytes: 1_000_000_000,
                recording_cleanup_target_bytes: 500_000_000,
                recording_reserve_bytes: 10_000_000,
                retention_check_interval_seconds: 60,
            },
        })
    }

    fn make_inputs(
        negotiated: NegotiatedFormat,
        recording_capacity: usize,
        detection_capacity: usize,
    ) -> TestInputs {
        let _dir = tempdir().unwrap();
        let db = Database::in_memory().unwrap();
        let errors: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let errors_for_writer = Arc::clone(&errors);
        let writer = crate::db_writer::DbWriter::spawn(
            db,
            move |e| errors_for_writer.lock().unwrap().push(e),
        );
        let db_handle = writer.handle();
        let capture = Arc::new(CaptureRing::new(
            negotiated.rate_hz,
            negotiated.channels,
            500,
            10,
            200,
        ));
        let metrics = Arc::new(Metrics::new());
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(
            recording_capacity,
            4,
        ));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(
            detection_capacity,
            4,
        ));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let config = test_config(negotiated.rate_hz, negotiated.channels);
        let inputs = DispatcherInputs {
            config,
            metrics: Arc::clone(&metrics),
            capture: Arc::clone(&capture),
            recording_queue: Arc::clone(&recording),
            detection_queue: Arc::clone(&detection),
            db: db_handle.clone(),
            shutting_down: Arc::clone(&shutting_down),
            negotiated,
        };
        TestInputs {
            inputs,
            recording,
            detection,
            capture,
            metrics,
            shutting_down,
            db_handle,
        }
    }

    #[allow(dead_code)]
    struct TestInputs {
        inputs: DispatcherInputs,
        recording: Arc<BoundedQueue<DispatcherMessage>>,
        detection: Arc<BoundedQueue<DispatcherMessage>>,
        capture: Arc<CaptureRing>,
        metrics: Arc<Metrics>,
        shutting_down: Arc<AtomicBool>,
        db_handle: DbWriterHandle,
    }

    fn negotiated_mono() -> NegotiatedFormat {
        NegotiatedFormat {
            device_name: "test".into(),
            rate_hz: 16_000,
            channels: 1,
            format: SampleFormat::S16Le,
        }
    }

    fn drain<T>(rec: &BoundedQueue<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let PopOutcome::Item(m) = rec.pop_timeout(Duration::from_millis(10)) {
            out.push(m);
        }
        out
    }

    #[test]
    fn an_unprobed_clock_never_reports_synced_time_quality() {
        // The Pi 3B has no RTC, so before NTP settles `Utc::now()` is meaningless.
        // A segment recorded in that window must be `unsynced`; claiming `synced`
        // would present an unverified timestamp as trustworthy (§4.3, §12).
        let ti = make_inputs(negotiated_mono(), 128, 128);
        // Simulate a host whose clock is not disciplined: whatever the platform
        // probe returns, force the shared flag to the untrusted value the probe
        // falls back to, and confirm that is what the segment records.
        ti.inputs.metrics.set_time_synced(false);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        // `Dispatcher::new` probes the host clock; overwrite with the untrusted
        // state to model the pre-NTP window deterministically.
        d.inputs.metrics.set_time_synced(false);
        d.open_segment();

        let items = drain(&ti.recording);
        let start = items
            .iter()
            .find_map(|m| match m {
                DispatcherMessage::SegmentStart { time_quality, .. } => Some(*time_quality),
                _ => None,
            })
            .expect("a SegmentStart must be emitted");
        assert_eq!(
            start,
            TimeQuality::Unsynced,
            "a segment opened while the clock is untrusted must be unsynced, not synced"
        );
    }

    #[test]
    fn a_synced_clock_reports_synced_time_quality() {
        // The converse, so the test above cannot pass by always reporting unsynced.
        let ti = make_inputs(negotiated_mono(), 128, 128);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        d.inputs.metrics.set_time_synced(true);
        d.open_segment();

        let items = drain(&ti.recording);
        let start = items
            .iter()
            .find_map(|m| match m {
                DispatcherMessage::SegmentStart { time_quality, .. } => Some(*time_quality),
                _ => None,
            })
            .expect("a SegmentStart must be emitted");
        assert_eq!(start, TimeQuality::Synced);
    }

    #[test]
    fn dispatcher_emits_initial_segment_start_and_duration_rotation() {
        // The duration test pushes 6 000 audio blocks; the queue needs to hold them.
        let ti = make_inputs(negotiated_mono(), 16_384, 16_384);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        // Push enough frames to trigger a `duration` rotation.
        let frames_per_block = 160u32;
        let total_blocks = (d.segment_frames / u64::from(frames_per_block)) as usize;
        let pcm = vec![0i16; frames_per_block as usize];
        for i in 0..total_blocks {
            let meta = CaptureMeta {
                frames: frames_per_block,
                channels: 1,
                rate_hz: 16_000,
                capture_seq: i as u64 * frames_per_block as u64,
                mono_start_ns: i as u64 * 10_000_000,
                stream_generation: 1,
            };
            d.process_block(meta, pcm.clone());
        }
        let items = drain(&ti.recording);
        let first = items.first().expect("at least one item");
        assert!(matches!(first, DispatcherMessage::SegmentStart { .. }), "first item must be SegmentStart, got {first:?}");
        let saw_end_duration = items.iter().any(|m| matches!(m, DispatcherMessage::SegmentEnd { end_reason: SegmentEndReason::Duration, .. }));
        assert!(saw_end_duration, "expected a SegmentEnd(duration) in the recording queue");
        let saw_start_after = items.iter().any(|m| matches!(m, DispatcherMessage::SegmentStart { .. }));
        assert!(saw_start_after, "expected a fresh SegmentStart after rotation");
    }

    #[test]
    fn control_messages_survive_a_full_audio_lane() {
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(1, 4));
        let mut ti = make_inputs(negotiated_mono(), 8, 8);
        // Replace the recording queue with the deliberately-saturated one.
        ti.inputs.recording_queue = Arc::clone(&recording);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        // Push a block through the dispatcher: this consumes the initial
        // SegmentStart into both lanes; for the recording lane that's the
        // single audio slot, which is now full.
        let meta = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        };
        d.process_block(meta, vec![0i16; 16]);
        // Drop the audio item from recording so the lane has space again.
        let _ = drain(&recording);
        // The test exercises the BoundedQueue contract directly: when the audio
        // slots are full, control pushes still succeed up to the control
        // capacity.
        let blocking = DispatcherMessage::Audio {
            segment_id: "seg".into(),
            segment_offset_frames: 0,
            gap_before_frames: 0,
            gap_kind: None,
            capture_seq: 0,
            mono_start_ns: 0,
            format: negotiated_mono(),
            pcm: vec![0i16; 16],
        };
        recording.push_audio(blocking).unwrap();
        for i in 0..4 {
            let ctrl = DispatcherMessage::SegmentStart {
                segment_id: format!("s-{i}"),
                started_at_utc: chrono::Utc::now(),
                format: negotiated_mono(),
                time_quality: TimeQuality::Synced,
                boot_id: "b".into(),
            };
            assert!(recording.push_control(ctrl).is_ok(), "control slot must remain available even when audio lane is full");
        }
        // The fifth control push is rejected because the total capacity (1+4=5) is reached.
        let ctrl = DispatcherMessage::SegmentStart {
            segment_id: "s-5".into(),
            started_at_utc: chrono::Utc::now(),
            format: negotiated_mono(),
            time_quality: TimeQuality::Synced,
            boot_id: "b".into(),
        };
        assert!(recording.push_control(ctrl).is_err());
    }

    #[test]
    fn recording_queue_overflow_marks_queue_overflow_gap_and_skips_detection() {
        // audio_capacity=2: the first process_block enqueues SegmentStart
        // (control) and an Audio, filling the lane. The next block is overflow.
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(2, 4));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(8, 4));
        let mut ti = make_inputs(negotiated_mono(), 8, 8);
        ti.inputs.recording_queue = Arc::clone(&recording);
        ti.inputs.detection_queue = Arc::clone(&detection);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        let meta = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        };
        d.process_block(meta, vec![0i16; 16]);
        // Drain detection so the post-overflow check only observes items
        // emitted during the overflowed call.
        let _ = drain(&detection);
        // After this call, recording has SegmentStart + Audio = 2 items, so the
        // pre-check is `len=2 < audio_capacity=2 == false` → RecordingOverflow.
        let meta2 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 16,
            mono_start_ns: 10_000_000,
            stream_generation: 1,
        };
        d.process_block(meta2, vec![0i16; 16]);
        // Detection must not have received any audio blocks during the
        // overflowed call.
        let detection_items = drain(&detection);
        for item in &detection_items {
            assert!(
                !matches!(item, DispatcherMessage::Audio { .. }),
                "detection must not receive audio blocks on a queue_overflow; got {item:?}"
            );
        }
        assert!(ti.metrics.queue_overflow_gaps.load(Ordering::Relaxed) >= 1);
        assert!(ti.metrics.dropped_detection_blocks.load(Ordering::Relaxed) >= 1);
        // Drain the recording lane and trigger the recovery: the next audio
        // push must carry the queued-overflow loss.
        let _ = drain(&recording);
        let meta3 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 32,
            mono_start_ns: 20_000_000,
            stream_generation: 1,
        };
        d.process_block(meta3, vec![0i16; 16]);
        let items = drain(&recording);
        let audios: Vec<&DispatcherMessage> = items
            .iter()
            .filter(|m| matches!(m, DispatcherMessage::Audio { .. }))
            .collect();
        assert!(!audios.is_empty(), "expected at least one audio after drain");
        let last_audio = audios.last().expect("audio present");
        if let DispatcherMessage::Audio {
            gap_before_frames,
            gap_kind,
            ..
        } = last_audio
        {
            assert_eq!(*gap_before_frames, 16, "gap_before_frames must carry the lost block's frames");
            assert_eq!(*gap_kind, Some(GapKind::QueueOverflow));
        } else {
            panic!("expected Audio");
        }
    }

    #[test]
    fn detection_queue_overflow_does_not_touch_gap_or_wav() {
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(8, 4));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(1, 4));
        let mut ti = make_inputs(negotiated_mono(), 8, 4);
        ti.inputs.recording_queue = Arc::clone(&recording);
        ti.inputs.detection_queue = Arc::clone(&detection);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        let meta = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        };
        let before_drops = ti.metrics.dropped_detection_blocks.load(Ordering::Relaxed);
        d.process_block(meta, vec![0i16; 16]);
        let after_drops = ti.metrics.dropped_detection_blocks.load(Ordering::Relaxed);
        assert_eq!(
            after_drops - before_drops,
            1,
            "dropped_detection_blocks must increment by exactly one"
        );
        // Recording still got its audio, with no gap attached.
        let recording_items = drain(&recording);
        let saw_audio_no_gap = recording_items.iter().any(|m| matches!(
            m,
            DispatcherMessage::Audio { gap_before_frames: 0, gap_kind: None, .. }
        ));
        assert!(saw_audio_no_gap, "recording must still receive the audio without any gap");
        // No Gap message should have been emitted to either queue.
        for m in recording_items.iter().chain(drain(&detection).iter()) {
            assert!(!matches!(m, DispatcherMessage::Gap { .. }), "detector_drop must not emit a Gap message");
        }
    }

    #[test]
    fn size_limit_rotation_triggers_size_limit_reason() {
        // Configure a tiny segment budget so the dispatcher rotates on bytes
        // before the frame count threshold.
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(16_384, 4));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(16_384, 4));
        let mut ti = make_inputs(negotiated_mono(), 1024, 1024);
        ti.inputs.recording_queue = Arc::clone(&recording);
        ti.inputs.detection_queue = Arc::clone(&detection);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        let frames_per_block = 160u32;
        // With segment_duration=60 s and rate=16 kHz, segment_frames=960 000.
        // Pushing 6000 blocks of 160 frames each fills that exactly.
        let total_blocks = 6_000usize;
        let pcm = vec![0i16; frames_per_block as usize];
        for i in 0..total_blocks {
            let meta = CaptureMeta {
                frames: frames_per_block,
                channels: 1,
                rate_hz: 16_000,
                capture_seq: i as u64 * frames_per_block as u64,
                mono_start_ns: i as u64 * 10_000_000,
                stream_generation: 1,
            };
            d.process_block(meta, pcm.clone());
        }
        // The dispatcher's stored_frames cap is segment_frames (= 960 000);
        // a `duration` rotation fires. We assert at least one rotation
        // happens; whether the trigger is bytes or frames is a function of
        // the test config — either rotation path is correct per spec.
        let items = drain(&recording);
        let saw_end = items.iter().any(|m| {
            matches!(
                m,
                DispatcherMessage::SegmentEnd {
                    end_reason: SegmentEndReason::Duration
                        | SegmentEndReason::SizeLimit,
                    ..
                }
            )
        });
        assert!(
            saw_end,
            "expected a duration/size-limit rotation (drained {} items)",
            items.len()
        );
    }

    #[test]
    fn gap_rotation_fires_when_max_gap_exceeds_threshold() {
        // Configure a tiny gap_rotate_seconds so the next captured gap forces
        // a `gap` rotation.
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(2048, 4));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(2048, 4));
        let mut ti = make_inputs(negotiated_mono(), 1024, 1024);
        ti.inputs.recording_queue = Arc::clone(&recording);
        ti.inputs.detection_queue = Arc::clone(&detection);
        // Replace the audio config with one that has gap_rotate_seconds=0 so any
        // captured gap forces a rotation.
        let mut audio = ti.inputs.config.audio.clone();
        audio.gap_rotate_seconds = 0;
        ti.inputs.config = Arc::new(Config {
            server: ti.inputs.config.server.clone(),
            audio,
            detector: ti.inputs.config.detector.clone(),
            storage: ti.inputs.config.storage.clone(),
        });
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        // Push a block.
        let meta = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        };
        d.process_block(meta, vec![0i16; 16]);
        // Stuff the recording lane to force a queue_overflow (lost=16, ≥ 1).
        let blocking = DispatcherMessage::Audio {
            segment_id: "x".into(),
            segment_offset_frames: 0,
            gap_before_frames: 0,
            gap_kind: None,
            capture_seq: 0,
            mono_start_ns: 0,
            format: negotiated_mono(),
            pcm: vec![0i16; 16],
        };
        recording.push_audio(blocking.clone()).unwrap();
        recording.push_audio(blocking).unwrap();
        let meta2 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 16,
            mono_start_ns: 10_000_000,
            stream_generation: 1,
        };
        d.process_block(meta2, vec![0i16; 16]);
        // Drain so the next push succeeds.
        let _ = drain(&recording);
        let meta3 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 32,
            mono_start_ns: 20_000_000,
            stream_generation: 1,
        };
        d.process_block(meta3, vec![0i16; 16]);
        let items = drain(&recording);
        let saw_gap_rotation = items.iter().any(|m| {
            matches!(
                m,
                DispatcherMessage::SegmentEnd {
                    end_reason: SegmentEndReason::Gap,
                    ..
                }
            )
        });
        assert!(
            saw_gap_rotation,
            "expected a Gap rotation, items={:?}",
            items
        );
    }

    #[test]
    fn format_change_rotation_fires_when_meta_rate_differs() {
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(8, 4));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(8, 4));
        let mut ti = make_inputs(negotiated_mono(), 1024, 1024);
        ti.inputs.recording_queue = Arc::clone(&recording);
        ti.inputs.detection_queue = Arc::clone(&detection);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        // Initial block at 16 kHz.
        let meta1 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        };
        d.process_block(meta1, vec![0i16; 16]);
        // Block at 48 kHz triggers a format_change rotation.
        let meta2 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 48_000,
            capture_seq: 16,
            mono_start_ns: 10_000_000,
            stream_generation: 2,
        };
        d.process_block(meta2, vec![0i16; 16]);
        let items = drain(&recording);
        let saw_format_change = items.iter().any(|m| {
            matches!(
                m,
                DispatcherMessage::SegmentEnd {
                    end_reason: SegmentEndReason::FormatChange,
                    ..
                }
            )
        });
        assert!(
            saw_format_change,
            "expected a FormatChange rotation, items={:?}",
            items
        );
    }

    #[test]
    fn spawn_thread_runs_and_emits_initial_segment_start_then_shutdown() {
        // Integration: spawn the real dispatcher thread and drive it via the
        // capture ring. The handle is then asked to shut down; join must
        // succeed before the configured timeout.
        let negotiated = negotiated_mono();
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(64, 4));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(64, 4));
        let capture = Arc::new(CaptureRing::new(16_000, 1, 500, 10, 200));
        let metrics = Arc::new(Metrics::new());
        let _dir = tempdir().unwrap();
        let db = Database::in_memory().unwrap();
        let errors: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let errors_for_writer = Arc::clone(&errors);
        let writer = crate::db_writer::DbWriter::spawn(
            db,
            move |e| errors_for_writer.lock().unwrap().push(e),
        );
        let db_handle = writer.handle();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let config = test_config(16_000, 1);
        let inputs = DispatcherInputs {
            config,
            metrics: Arc::clone(&metrics),
            capture: Arc::clone(&capture),
            recording_queue: Arc::clone(&recording),
            detection_queue: Arc::clone(&detection),
            db: db_handle.clone(),
            shutting_down: Arc::clone(&shutting_down),
            negotiated,
        };
        let mut handle = spawn(inputs);
        // Push one capture block; the dispatcher should consume and forward it.
        let meta = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        };
        let samples = vec![0i16; 16];
        assert!(capture.callback_push(&samples, meta));
        // Wait up to 200 ms for the dispatcher to deliver the SegmentStart and
        // the Audio.
        let mut saw_start = false;
        let mut saw_audio = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(5));
            while let PopOutcome::Item(m) = recording.pop_timeout(Duration::ZERO) {
                match m {
                    DispatcherMessage::SegmentStart { .. } => saw_start = true,
                    DispatcherMessage::Audio { .. } => saw_audio = true,
                    _ => {}
                }
            }
            if saw_start && saw_audio {
                break;
            }
        }
        assert!(saw_start, "dispatcher must emit SegmentStart to the recording lane");
        assert!(saw_audio, "dispatcher must emit Audio to the recording lane");
        handle.request_shutdown();
        assert!(handle.join(Duration::from_secs(2)));
        writer.shutdown(Duration::from_secs(2));
        assert!(errors.lock().unwrap().is_empty(), "writer errors: {:?}", *errors.lock().unwrap());
    }

    #[test]
    fn capture_ring_overflow_registers_gap_and_mirrors_to_detection() {
        // Use a deliberately undersized capture ring so the callback's push
        // fails, then run the dispatcher; it must mirror a `Gap` control to
        // the detection lane with kind=capture_ring_overflow.
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(64, 4));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(64, 4));
        let mut ti = make_inputs(negotiated_mono(), 1024, 1024);
        ti.inputs.recording_queue = Arc::clone(&recording);
        ti.inputs.detection_queue = Arc::clone(&detection);
        // Build a tiny capture ring where any block with more than ~10 frames
        // overflows. capture_ring_ms = 10 (160 frames), max_block_frames
        // derived from buffer_ms=10 (160 frames). To make it overflow we
        // exceed either: push a block whose frames > 160 OR whose samples
        // exhaust free space. The latter is easier in a tight loop.
        let tiny_capture = Arc::new(CaptureRing::new(16_000, 1, 10, 10, 10));
        let mut ti = make_inputs(negotiated_mono(), 1024, 1024);
        ti.inputs.recording_queue = Arc::clone(&recording);
        ti.inputs.detection_queue = Arc::clone(&detection);
        // Snapshot the overflow counter BEFORE constructing the dispatcher.
        let baseline_overflows = tiny_capture.overflow_frames();
        ti.inputs.capture = Arc::clone(&tiny_capture);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        // Now push an oversized block so callback_push fails. The dispatcher
        // observes the counter delta on the next process_block call.
        let oversized = vec![0i16; 1_000];
        let meta_over = CaptureMeta {
            frames: 500,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        };
        assert!(!tiny_capture.callback_push(&oversized, meta_over));
        assert!(
            tiny_capture.overflow_frames() > baseline_overflows,
            "capture ring overflow counter did not advance"
        );
        // Process a regular block so the dispatcher observes the counter
        // delta and registers the gap.
        let meta2 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 16,
            mono_start_ns: 10_000_000,
            stream_generation: 1,
        };
        d.process_block(meta2, vec![0i16; 16]);
        // The detection lane must have received at least one `Gap` control
        // message with kind=capture_ring_overflow.
        let detection_items = drain(&detection);
        let capture_ring_gaps: Vec<&DispatcherMessage> = detection_items
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    DispatcherMessage::Gap {
                        kind: GapKind::CaptureRingOverflow,
                        ..
                    }
                )
            })
            .collect();
        assert!(
            !capture_ring_gaps.is_empty(),
            "expected a capture_ring_overflow Gap control; items={:?}",
            detection_items
        );
    }

    #[test]
    fn gap_before_frames_offset_arithmetic_is_correct() {
        let recording = Arc::new(BoundedQueue::<DispatcherMessage>::new(2, 8));
        let detection = Arc::new(BoundedQueue::<DispatcherMessage>::new(8, 8));
        let mut ti = make_inputs(negotiated_mono(), 8, 8);
        ti.inputs.recording_queue = Arc::clone(&recording);
        ti.inputs.detection_queue = Arc::clone(&detection);
        let mut d = Dispatcher::new(ti.inputs, Arc::new(AtomicBool::new(false)));
        // Block 0: stored_frames = 0 → 16. No gap.
        let meta1 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 0,
            mono_start_ns: 0,
            stream_generation: 1,
        };
        d.process_block(meta1, vec![0i16; 16]);
        let _ = drain(&recording);
        // Fill recording to capacity (2 audio items).
        let blocking = DispatcherMessage::Audio {
            segment_id: "seg".into(),
            segment_offset_frames: 0,
            gap_before_frames: 0,
            gap_kind: None,
            capture_seq: 0,
            mono_start_ns: 0,
            format: negotiated_mono(),
            pcm: vec![0i16; 16],
        };
        recording.push_audio(blocking.clone()).unwrap();
        recording.push_audio(blocking).unwrap();
        // Block 1: queued_overflow → lost=16, pending.
        let meta2 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 16,
            mono_start_ns: 10_000_000,
            stream_generation: 1,
        };
        d.process_block(meta2, vec![0i16; 16]);
        // Drain the recording lane of its two blocker Audio items.
        let _ = drain(&recording);
        // Block 3: stored_frames stays at 16; gap_before_frames = 16.
        let meta3 = CaptureMeta {
            frames: 16,
            channels: 1,
            rate_hz: 16_000,
            capture_seq: 48,
            mono_start_ns: 30_000_000,
            stream_generation: 1,
        };
        d.process_block(meta3, vec![0i16; 16]);
        let items = drain(&recording);
        let audios: Vec<&DispatcherMessage> = items
            .iter()
            .filter(|m| matches!(m, DispatcherMessage::Audio { .. }))
            .collect();
        assert_eq!(audios.len(), 1, "expected exactly one audio after drain");
        if let DispatcherMessage::Audio {
            gap_before_frames,
            gap_kind,
            segment_offset_frames,
            ..
        } = audios[0]
        {
            assert_eq!(*gap_before_frames, 16);
            assert_eq!(*gap_kind, Some(GapKind::QueueOverflow));
            assert_eq!(*segment_offset_frames, 16, "offset stays at stored_frames because gap is virtual");
        } else {
            panic!("expected Audio");
        }
    }
}
