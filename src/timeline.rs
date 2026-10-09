//! Monotonic/UTC anchors and gap-aware segment time mapping (TECH_SPEC §4.3,
//! task T3.3).
//!
//! Drift and gaps are determined from *adjacent* capture blocks, never from a
//! long-running comparison between the USB microphone clock and system time.
//! Stored PCM offsets are authoritative; `wall_at_offset` adds only gaps whose
//! offset is at or before the queried position.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeQuality {
    Synced,
    Unsynced,
    Corrected,
}

impl TimeQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            TimeQuality::Synced => "synced",
            TimeQuality::Unsynced => "unsynced",
            TimeQuality::Corrected => "corrected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateSource {
    Timestamp,
    Counter,
    Unknown,
}

impl EstimateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EstimateSource::Timestamp => "timestamp",
            EstimateSource::Counter => "counter",
            EstimateSource::Unknown => "unknown",
        }
    }
}

/// One clock correspondence. `mono_ns` is a CLOCK_MONOTONIC value, not wall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockAnchor {
    pub mono_ns: u64,
    pub utc: DateTime<Utc>,
    pub time_synced: bool,
}

impl ClockAnchor {
    /// Converts a monotonic timestamp using this linear anchor.
    pub fn wall_at(&self, mono_ns: u64) -> DateTime<Utc> {
        let delta = mono_ns as i128 - self.mono_ns as i128;
        self.utc + ChronoDuration::nanoseconds(delta.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }
}

/// Records an anchor at construction and on demand every 60 seconds.
#[derive(Debug, Clone)]
pub struct ClockMap {
    boot_id: String,
    anchors: Vec<ClockAnchor>,
    last_sync_mono_ns: u64,
    time_quality: TimeQuality,
    correction_ns: i64,
}

impl ClockMap {
    pub fn new(boot_id: impl Into<String>, initial: ClockAnchor) -> Self {
        let time_quality = if initial.time_synced { TimeQuality::Synced } else { TimeQuality::Unsynced };
        ClockMap {
            boot_id: boot_id.into(),
            anchors: vec![initial],
            last_sync_mono_ns: initial.mono_ns,
            time_quality,
            correction_ns: 0,
        }
    }

    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    pub fn anchors(&self) -> &[ClockAnchor] {
        &self.anchors
    }

    pub fn time_quality(&self) -> TimeQuality {
        self.time_quality
    }

    pub fn time_synced(&self) -> bool {
        self.anchors.last().is_some_and(|a| a.time_synced)
    }

    pub fn correction_ns(&self) -> i64 {
        self.correction_ns
    }

    /// Whether the 60-second resynchronization interval has elapsed.
    pub fn should_refresh(&self, mono_ns: u64) -> bool {
        mono_ns.saturating_sub(self.last_sync_mono_ns) >= 60_000_000_000
    }

    /// Adds a new anchor and detects a wall-clock step over 1 second.
    ///
    /// Returns the step in nanoseconds when it must rotate the current segment.
    /// If the prior segment was unsynced, belongs to this boot, and NTP has now
    /// synchronized, that prior interval can be corrected by the step (§4.3).
    pub fn refresh(&mut self, next: ClockAnchor) -> Option<i64> {
        let previous = *self.anchors.last()?;
        let mono_delta = next.mono_ns as i128 - previous.mono_ns as i128;
        let utc_delta = (next.utc - previous.utc).num_nanoseconds().unwrap_or(i64::MAX) as i128;
        let step = utc_delta - mono_delta;
        self.last_sync_mono_ns = next.mono_ns;
        self.anchors.push(next);
        if step.unsigned_abs() > 1_000_000_000 {
            let step_i64 = step.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            if !previous.time_synced && next.time_synced {
                self.correction_ns = self.correction_ns.saturating_add(step_i64);
                self.time_quality = TimeQuality::Corrected;
            }
            return Some(step_i64);
        }
        if next.time_synced && self.time_quality == TimeQuality::Unsynced {
            self.time_quality = TimeQuality::Synced;
        }
        None
    }

    /// Maps a monotonic timestamp through the latest anchor. Segment rotation on
    /// a wall-clock step ensures one linear mapping per segment.
    pub fn wall_at(&self, mono_ns: u64) -> Option<DateTime<Utc>> {
        self.anchors.last().map(|anchor| anchor.wall_at(mono_ns))
    }
}

/// Offset of one captured gap in the authoritative stored-frame timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapRecord {
    pub offset_frames: u64,
    pub lost_frames: u64,
    pub kind: GapKind,
    pub estimate_source: EstimateSource,
    pub at_utc: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    Xrun,
    StreamError,
    CaptureRingOverflow,
    QueueOverflow,
    StreamRebuild,
}

impl GapKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GapKind::Xrun => "xrun",
            GapKind::StreamError => "stream_error",
            GapKind::CaptureRingOverflow => "capture_ring_overflow",
            GapKind::QueueOverflow => "queue_overflow",
            GapKind::StreamRebuild => "stream_rebuild",
        }
    }
}

/// Wall-clock mapping for one WAV segment's stored offsets.
#[derive(Debug, Clone)]
pub struct SegmentTimeline {
    pub started_at_utc: DateTime<Utc>,
    pub capture_rate_hz: u32,
    pub time_quality: TimeQuality,
    pub boot_id: String,
    pub gaps: Vec<GapRecord>,
}

impl SegmentTimeline {
    pub fn new(started_at_utc: DateTime<Utc>, capture_rate_hz: u32, time_quality: TimeQuality, boot_id: impl Into<String>) -> Self {
        SegmentTimeline {
            started_at_utc,
            capture_rate_hz,
            time_quality,
            boot_id: boot_id.into(),
            gaps: Vec::new(),
        }
    }

    pub fn add_gap(&mut self, gap: GapRecord) {
        self.gaps.push(gap);
        self.gaps.sort_by_key(|g| g.offset_frames);
    }

    /// `wall(o) = started + (o + sum(lost_frames where gap.offset <= o))/rate`.
    pub fn wall_at_offset(&self, offset_frames: u64) -> DateTime<Utc> {
        let lost: u64 = self
            .gaps
            .iter()
            .filter(|gap| gap.offset_frames <= offset_frames)
            .map(|gap| gap.lost_frames)
            .sum();
        let real_frames = offset_frames.saturating_add(lost);
        self.started_at_utc + frames_to_duration(real_frames, self.capture_rate_hz)
    }

    /// Inverse of [`SegmentTimeline::wall_at_offset`] to the nearest stored frame.
    /// The gap interval maps to the one offset at which the gap was recorded.
    pub fn offset_at_wall(&self, wall: DateTime<Utc>, max_offset_frames: u64) -> u64 {
        if wall <= self.started_at_utc {
            return 0;
        }
        let target_ns = (wall - self.started_at_utc).num_nanoseconds().unwrap_or(i64::MAX).max(0) as u128;
        let target_real_frames = target_ns * self.capture_rate_hz as u128 / 1_000_000_000u128;
        let mut lost_before = 0u64;
        let mut stored_cursor = 0u64;
        for gap in &self.gaps {
            let gap_real_position = gap.offset_frames.saturating_add(lost_before) as u128;
            if target_real_frames < gap_real_position {
                break;
            }
            if target_real_frames < gap_real_position + gap.lost_frames as u128 {
                return gap.offset_frames.min(max_offset_frames);
            }
            lost_before = lost_before.saturating_add(gap.lost_frames);
            stored_cursor = gap.offset_frames;
        }
        let stored = (target_real_frames as u64).saturating_sub(lost_before);
        stored.max(stored_cursor).min(max_offset_frames)
    }
}

/// Estimates a gap from adjacent callback blocks only, never from a cumulative
/// time comparison (TECH_SPEC §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapEstimate {
    pub lost_frames: u64,
    pub source: EstimateSource,
}

pub fn estimate_timestamp_gap(
    previous_start_ns: u64,
    previous_frames: u64,
    next_start_ns: u64,
    rate_hz: u32,
    tolerance_ms: u32,
) -> Option<GapEstimate> {
    if rate_hz == 0 {
        return None;
    }
    let previous_duration_ns = previous_frames as u128 * 1_000_000_000u128 / rate_hz as u128;
    let previous_end_ns = previous_start_ns as u128 + previous_duration_ns;
    let gap_ns = (next_start_ns as u128).saturating_sub(previous_end_ns);
    if gap_ns <= u128::from(tolerance_ms) * 1_000_000u128 {
        return None;
    }
    let lost = (gap_ns * rate_hz as u128 + 500_000_000u128) / 1_000_000_000u128;
    Some(GapEstimate {
        lost_frames: lost.min(u64::MAX as u128) as u64,
        source: EstimateSource::Timestamp,
    })
}

/// A known sample-counter discontinuity needs no timestamp estimate.
pub fn estimate_counter_gap(lost_frames: u64) -> Option<GapEstimate> {
    (lost_frames > 0).then_some(GapEstimate {
        lost_frames,
        source: EstimateSource::Counter,
    })
}

/// Drift in ppm between an observed frame count and the monotonic elapsed time.
pub fn clock_drift_ppm(frame_count: u64, nominal_rate_hz: u32, mono_elapsed_ns: u64) -> Option<f64> {
    if nominal_rate_hz == 0 || mono_elapsed_ns == 0 {
        return None;
    }
    let expected_ns = frame_count as f64 * 1_000_000_000.0 / nominal_rate_hz as f64;
    Some((expected_ns - mono_elapsed_ns as f64) / mono_elapsed_ns as f64 * 1_000_000.0)
}

fn frames_to_duration(frames: u64, rate_hz: u32) -> ChronoDuration {
    if rate_hz == 0 {
        return ChronoDuration::zero();
    }
    let ns = (frames as u128 * 1_000_000_000u128 / rate_hz as u128).min(i64::MAX as u128) as i64;
    ChronoDuration::nanoseconds(ns)
}

/// Host boot identifier, if available. Linux provides a UUID in procfs; other
/// hosts get a process-scoped fallback which is never mistaken for a real Pi boot.
pub fn boot_id() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(value) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        return value.trim().to_string();
    }
    format!("process-{}-{}", std::process::id(), monotonicish_nonce())
}

fn monotonicish_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().unwrap()
    }

    #[test]
    fn wall_offset_round_trips_with_multiple_gaps() {
        let mut t = SegmentTimeline::new(utc(1_700_000_000), 16_000, TimeQuality::Synced, "boot");
        t.add_gap(GapRecord {
            offset_frames: 16_000,
            lost_frames: 8_000,
            kind: GapKind::Xrun,
            estimate_source: EstimateSource::Timestamp,
            at_utc: utc(1_700_000_001),
        });
        t.add_gap(GapRecord {
            offset_frames: 48_000,
            lost_frames: 4_000,
            kind: GapKind::StreamRebuild,
            estimate_source: EstimateSource::Counter,
            at_utc: utc(1_700_000_003),
        });
        let offsets = [0, 8_000, 15_999, 16_000, 32_000, 47_999, 48_000, 64_000];
        for offset in offsets {
            let wall = t.wall_at_offset(offset);
            let round_trip = t.offset_at_wall(wall, 100_000);
            assert_eq!(round_trip, offset, "offset {offset}");
        }
    }

    #[test]
    fn gap_boundaries_follow_the_authoritative_offset_formula() {
        let mut t = SegmentTimeline::new(utc(100), 1_000, TimeQuality::Synced, "boot");
        t.add_gap(GapRecord {
            offset_frames: 1_000,
            lost_frames: 500,
            kind: GapKind::Xrun,
            estimate_source: EstimateSource::Timestamp,
            at_utc: utc(101),
        });
        assert_eq!(t.wall_at_offset(999), utc(100) + ChronoDuration::milliseconds(999));
        // A gap is at the first stored offset after the lost time, so its wall
        // mapping includes that gap at the boundary.
        assert_eq!(t.wall_at_offset(1_000), utc(101) + ChronoDuration::milliseconds(500));
        assert_eq!(t.offset_at_wall(utc(101) + ChronoDuration::milliseconds(250), 2_000), 1_000);
    }

    #[test]
    fn timestamp_gap_uses_adjacent_block_difference_and_tolerance() {
        let rate = 48_000;
        let first_frames = 480;
        let first_start = 10_000_000u64;
        let normal_next = first_start + 10_000_000;
        assert_eq!(estimate_timestamp_gap(first_start, first_frames, normal_next, rate, 20), None);
        let delayed_next = normal_next + 100_000_000;
        let gap = estimate_timestamp_gap(first_start, first_frames, delayed_next, rate, 20).unwrap();
        assert_eq!(gap.source, EstimateSource::Timestamp);
        assert_eq!(gap.lost_frames, 4_800);
    }

    #[test]
    fn counter_gap_is_exact_and_zero_is_absent() {
        assert_eq!(estimate_counter_gap(0), None);
        assert_eq!(estimate_counter_gap(123).unwrap().lost_frames, 123);
        assert_eq!(estimate_counter_gap(123).unwrap().source, EstimateSource::Counter);
    }

    #[test]
    fn clock_step_over_one_second_requests_rotation_and_corrects_unsynced_time() {
        let first = ClockAnchor { mono_ns: 1_000_000_000, utc: utc(100), time_synced: false };
        let mut map = ClockMap::new("boot-a", first);
        let step = map.refresh(ClockAnchor {
            mono_ns: 61_000_000_000,
            utc: utc(162),
            time_synced: true,
        });
        assert_eq!(step, Some(2_000_000_000));
        assert_eq!(map.time_quality(), TimeQuality::Corrected);
        assert_eq!(map.correction_ns(), 2_000_000_000);
    }

    #[test]
    fn ordinary_clock_progress_does_not_rotate_or_report_drift_as_gap() {
        let first = ClockAnchor { mono_ns: 1_000_000_000, utc: utc(100), time_synced: true };
        let mut map = ClockMap::new("boot-a", first);
        assert_eq!(map.refresh(ClockAnchor {
            mono_ns: 61_000_000_000,
            utc: utc(160),
            time_synced: true,
        }), None);
        // A 100 ppm capture clock drift is metadata; it is not inferred as a gap.
        assert!((clock_drift_ppm(16_001_600, 16_000, 1_000_000_000_000).unwrap() - 100.0).abs() < 0.01);
    }

    #[test]
    fn monotonic_anchor_maps_in_both_directions() {
        let anchor = ClockAnchor { mono_ns: 5_000_000_000, utc: utc(1_000), time_synced: true };
        assert_eq!(anchor.wall_at(6_250_000_000), utc(1_001) + ChronoDuration::milliseconds(250));
    }
}
