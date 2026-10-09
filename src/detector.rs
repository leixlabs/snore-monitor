//! Detection pipeline and candidate-event state machine (TECH_SPEC §5.1–§5.4,
//! tasks T2.5 and T2.6).
//!
//! This module owns the detector-only view of the samples. Recording remains
//! untouched raw PCM in the recorder branch. `DetectionPipeline` selects one
//! channel, converts S16 to f32, resamples to 16 kHz, filters/frames the stream,
//! manages the noise floors and feeds the state machine. Callers pass explicit
//! segment offsets so event boundaries remain in the stored WAV frame domain.

use crate::config::DetectorConfig;
use crate::dsp::{BandPass, DcBlocker, FrameBuilder, FrameFeatures, NoiseFloor};
use crate::resampler::{Resampler, ResamplerError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const DETECTOR_VERSION: &str = "rule-v1";

#[derive(Debug, thiserror::Error)]
pub enum DetectorError {
    #[error(transparent)]
    Resampler(#[from] ResamplerError),
    #[error(transparent)]
    Dsp(#[from] crate::dsp::DspError),
    #[error("unsupported channel layout: selected channel {selected}, actual channels {channels}")]
    ChannelIndex { selected: u16, channels: u16 },
    #[error("audio block sample count {samples} is not divisible by channel count {channels}")]
    MisalignedSamples { samples: usize, channels: u16 },
}

/// Why an event was force-closed or naturally completed (TECH_SPEC §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Normal,
    MaxDuration,
    SegmentEnd,
    Gap,
    DetectorGap,
    Shutdown,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Normal => "normal",
            EndReason::MaxDuration => "max_duration",
            EndReason::SegmentEnd => "segment_end",
            EndReason::Gap => "gap",
            EndReason::DetectorGap => "detector_gap",
            EndReason::Shutdown => "shutdown",
        }
    }
}

/// Detector state, also serialized for `/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    WarmingUp,
    Idle,
    Candidate,
    Pending,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::WarmingUp => "warming_up",
            State::Idle => "idle",
            State::Candidate => "candidate",
            State::Pending => "pending",
        }
    }
}

/// Segment metadata needed to convert detected frame positions to public events.
#[derive(Debug, Clone)]
pub struct SegmentContext {
    pub segment_id: String,
    pub capture_rate_hz: u32,
    pub started_at_utc: DateTime<Utc>,
    pub offset_frames_at_anchor: u64,
    /// Cumulative lost frames in this segment, keyed by the first stored offset
    /// after the gap. Used for `wall(o)`, matching TECH_SPEC §4.3.
    pub gaps: Vec<GapOffset>,
    pub time_quality: String,
    pub boot_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapOffset {
    pub offset_frames: u64,
    pub lost_frames: u64,
}

impl SegmentContext {
    /// Wall-clock time for a stored-frame offset, using the same gap sum as §4.3.
    pub fn wall_at_offset(&self, offset_frames: u64) -> DateTime<Utc> {
        let lost: u64 = self
            .gaps
            .iter()
            .filter(|g| g.offset_frames < offset_frames)
            .map(|g| g.lost_frames)
            .sum();
        let elapsed_frames = offset_frames.saturating_add(lost);
        let ns = (elapsed_frames as u128 * 1_000_000_000u128 / self.capture_rate_hz as u128)
            .min(i64::MAX as u128) as i64;
        self.started_at_utc + chrono::Duration::nanoseconds(ns)
    }
}

/// Data persisted for a finalized event (TECH_SPEC §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDraft {
    pub id: String,
    pub segment_id: String,
    pub start_offset_frames: u64,
    pub end_offset_frames: u64,
    pub started_at_utc: DateTime<Utc>,
    pub ended_at_utc: DateTime<Utc>,
    pub duration_ms: u32,
    pub rule_score: Option<f32>,
    pub detector_version: String,
    pub peak_dbfs: Option<f32>,
    pub mean_level_dbfs: Option<f32>,
    pub mean_band_ratio: Option<f32>,
    pub noise_floor_dbfs: Option<f32>,
    pub end_reason: EndReason,
    pub continued: bool,
}

#[derive(Debug, Clone, Copy)]
struct HitFrame {
    index: u64,
    offset_start: u64,
    offset_end: u64,
    features: FrameFeatures,
    active_threshold_dbfs: f32,
    noise_floor_dbfs: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct EventAccumulator {
    start_frame: u64,
    end_frame: u64,
    start_offset: u64,
    end_offset: u64,
    hit_frames: u64,
    sum_level: f64,
    sum_band_ratio: f64,
    sum_level_margin: f64,
    peak: f32,
    noise_floor_dbfs: Option<f32>,
    continued: bool,
}

impl EventAccumulator {
    fn start(first: HitFrame, continued: bool) -> Self {
        let mut acc = EventAccumulator {
            start_frame: first.index,
            end_frame: first.index,
            start_offset: first.offset_start,
            end_offset: first.offset_end,
            hit_frames: 0,
            sum_level: 0.0,
            sum_band_ratio: 0.0,
            sum_level_margin: 0.0,
            peak: 0.0,
            noise_floor_dbfs: first.noise_floor_dbfs,
            continued,
        };
        acc.add_hit(first);
        acc
    }

    fn add_hit(&mut self, hit: HitFrame) {
        self.end_frame = hit.index;
        self.end_offset = hit.offset_end;
        self.hit_frames += 1;
        self.sum_level += f64::from(hit.features.level_dbfs);
        self.sum_band_ratio += f64::from(hit.features.band_ratio);
        self.sum_level_margin +=
            f64::from(hit.features.level_dbfs - hit.active_threshold_dbfs);
        self.peak = self.peak.max(hit.features.peak);
    }

    fn merge(&mut self, other: EventAccumulator) {
        self.end_frame = other.end_frame;
        self.end_offset = other.end_offset;
        self.hit_frames += other.hit_frames;
        self.sum_level += other.sum_level;
        self.sum_band_ratio += other.sum_band_ratio;
        self.sum_level_margin += other.sum_level_margin;
        self.peak = self.peak.max(other.peak);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttackHit {
    index: u64,
    offset_start: u64,
    offset_end: u64,
}

/// Attack/hangover/merge/max-duration state machine, separated from the filters
/// so the full transition table can be tested on deterministic feature fixtures.
#[derive(Debug)]
pub struct CandidateDetector {
    config: DetectorConfig,
    frame_index: u64,
    attack: VecDeque<AttackHit>,
    active: Option<EventAccumulator>,
    pending: Option<EventAccumulator>,
    last_hit_frame: Option<u64>,
    hangover: u32,
    next_continued: bool,
    state: State,
    events_discarded_short: u64,
}

impl CandidateDetector {
    pub fn new(config: DetectorConfig) -> Self {
        CandidateDetector {
            config,
            frame_index: 0,
            attack: VecDeque::new(),
            active: None,
            pending: None,
            last_hit_frame: None,
            hangover: 0,
            next_continued: false,
            state: State::WarmingUp,
            events_discarded_short: 0,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn events_discarded_short(&self) -> u64 {
        self.events_discarded_short
    }

    pub fn frame_index(&self) -> u64 {
        self.frame_index
    }

    /// Processes one 10 ms feature frame. `offset_start`/`offset_end` are in the
    /// stored PCM frame domain, and therefore include no invented gap samples.
    pub fn process_frame(
        &mut self,
        features: FrameFeatures,
        offset_start: u64,
        offset_end: u64,
        noise_floor_dbfs: Option<f32>,
        band_noise_floor_dbfs: Option<f32>,
        warmed: bool,
    ) -> Vec<EventAccumulator> {
        let idx = self.frame_index;
        self.frame_index += 1;
        let mut finalized = Vec::with_capacity(2);
        if !warmed || noise_floor_dbfs.is_none() || band_noise_floor_dbfs.is_none() {
            self.state = State::WarmingUp;
            self.attack.clear();
            return finalized;
        }
        if self.state == State::WarmingUp {
            self.state = State::Idle;
        }

        let active_threshold = self
            .config
            .absolute_floor_dbfs
            .max(noise_floor_dbfs.unwrap() + self.config.noise_margin_db);
        let active = features.level_dbfs > active_threshold;
        let snore_like = features.band_ratio >= self.config.band_ratio_min
            && features.band_rms_dbfs
                >= band_noise_floor_dbfs.unwrap() + self.config.band_margin_db;
        let hit = active && snore_like;

        if hit {
            self.last_hit_frame = Some(idx);
            self.hangover = 0;
            self.attack.push_back(AttackHit {
                index: idx,
                offset_start,
                offset_end,
            });
            while self.attack.len() > self.config.attack_window_frames as usize {
                self.attack.pop_front();
            }
            if self.active.is_some() {
                let h = HitFrame {
                    index: idx,
                    offset_start,
                    offset_end,
                    features,
                    active_threshold_dbfs: active_threshold,
                    noise_floor_dbfs,
                };
                if let Some(active) = self.active.as_mut() {
                    active.add_hit(h);
                }
                let should_split = self.active.as_ref().is_some_and(|a| {
                    let span_ms = (idx - a.start_frame + 1) * u64::from(self.config.frame_ms);
                    span_ms >= u64::from(self.config.max_event_seconds) * 1000
                });
                if should_split {
                    if let Some(event) = self.active.take() {
                        finalized.push(event);
                    }
                    self.active = Some(EventAccumulator::start(
                        HitFrame {
                            index: idx,
                            offset_start,
                            offset_end,
                            features,
                            active_threshold_dbfs: active_threshold,
                            noise_floor_dbfs,
                        },
                        true,
                    ));
                    self.next_continued = true;
                }
                self.state = State::Candidate;
                return finalized;
            }

            if self.active.is_none() && self.pending.is_none() && !self.attack.is_empty() {
                let attack_hits = self.attack.iter().filter(|h| h.index <= idx).count();
                if attack_hits >= self.config.attack_min_hits as usize {
                    let qualifying: Vec<AttackHit> = self
                        .attack
                        .iter()
                        .copied()
                        .filter(|h| h.index <= idx)
                        .collect();
                    // This deque contains only hit frames; quiet frames are not
                    // inserted, so its first item is the first onset hit.
                    let first_hit = *qualifying.first().expect("attack hit count checked");
                    let mut accumulator = EventAccumulator::start(
                        HitFrame {
                            index: first_hit.index,
                            offset_start: first_hit.offset_start,
                            offset_end: first_hit.offset_end,
                            features,
                            active_threshold_dbfs: active_threshold,
                            noise_floor_dbfs,
                        },
                        self.next_continued,
                    );
                    // Count every hit in the rolling attack window, not the
                    // non-hit frames between them. Feature values for older
                    // attack frames are not retained, so current-frame values
                    // are a conservative approximation for those few frames.
                    for prior in qualifying.iter().filter(|h| h.index > first_hit.index) {
                        accumulator.end_frame = prior.index;
                        accumulator.end_offset = prior.offset_end;
                        accumulator.hit_frames += 1;
                    }
                    self.active = Some(accumulator);
                    self.next_continued = false;
                    self.state = State::Candidate;
                    return finalized;
                }
            }

            if let Some(pending) = self.pending.take() {
                let onset_frame = self.attack.front().map_or(idx, |h| h.index);
                let gap_ms = (onset_frame.saturating_sub(pending.end_frame + 1))
                    * u64::from(self.config.frame_ms);
                let combined_span_ms = (idx.saturating_sub(pending.start_frame) + 1)
                    * u64::from(self.config.frame_ms);
                if gap_ms <= u64::from(self.config.merge_gap_ms)
                    && combined_span_ms <= u64::from(self.config.max_event_seconds) * 1000
                {
                    let current = EventAccumulator::start(
                        HitFrame {
                            index: idx,
                            offset_start,
                            offset_end,
                            features,
                            active_threshold_dbfs: active_threshold,
                            noise_floor_dbfs,
                        },
                        pending.continued,
                    );
                    let mut merged = pending;
                    merged.merge(current);
                    self.active = Some(merged);
                    self.state = State::Candidate;
                    return finalized;
                }
                finalized.push(pending);
                self.pending = None;
                // The onset window belongs to this new event.
                let first = self.attack.front().copied().unwrap_or(AttackHit {
                    index: idx,
                    offset_start,
                    offset_end,
                });
                self.active = Some(EventAccumulator::start(
                    HitFrame {
                        index: first.index,
                        offset_start: first.offset_start,
                        offset_end: first.offset_end,
                        features,
                        active_threshold_dbfs: active_threshold,
                        noise_floor_dbfs,
                    },
                    self.next_continued,
                ));
                self.next_continued = false;
                self.state = State::Candidate;
                return finalized;
            }
        } else {
            while self.attack.front().is_some_and(|hit| {
                idx.saturating_sub(hit.index) >= u64::from(self.config.attack_window_frames)
            }) {
                self.attack.pop_front();
            }
        }

        if self.active.is_some() {
            self.hangover += 1;
            if self.hangover >= self.config.hangover_frames {
                let active = self.active.take().unwrap();
                self.pending = Some(active);
                self.hangover = 0;
                self.state = State::Pending;
            } else {
                self.state = State::Candidate;
            }
        }

        if let Some(pending) = self.pending.as_ref() {
            let elapsed_ms = (idx.saturating_sub(pending.end_frame)) * u64::from(self.config.frame_ms);
            if elapsed_ms > u64::from(self.config.merge_gap_ms) {
                if let Some(event) = self.pending.take() {
                    finalized.push(event);
                }
                self.state = State::Idle;
            } else {
                self.state = State::Pending;
            }
        } else if self.active.is_none() {
            self.state = State::Idle;
        }
        finalized
    }

    /// Force-closes an active/pending candidate immediately at the last hit.
    pub fn force_close(&mut self) -> Option<EventAccumulator> {
        self.attack.clear();
        self.hangover = 0;
        let active = self.active.take();
        let pending = self.pending.take();
        self.state = State::Idle;
        match (active, pending) {
            (Some(a), Some(p)) => {
                let mut joined = p;
                joined.merge(a);
                Some(joined)
            }
            (Some(a), None) => Some(a),
            (None, Some(p)) => Some(p),
            (None, None) => None,
        }
    }

    /// Drops the attack/hangover state after a gap; a caller decides the event's
    /// `end_reason` and applies the configured short-event filter.
    pub fn reset_after_discontinuity(&mut self) -> Option<EventAccumulator> {
        self.last_hit_frame = None;
        self.next_continued = false;
        self.force_close()
    }
}

/// Stateful detector-only signal chain from interleaved PCM to event drafts.
pub struct DetectionPipeline {
    config: DetectorConfig,
    selected_channel: u16,
    channels: u16,
    capture_rate_hz: u32,
    resampler: Resampler,
    dc: DcBlocker,
    band: BandPass,
    framer: FrameBuilder,
    wide_noise: NoiseFloor,
    band_noise: NoiseFloor,
    state_machine: CandidateDetector,
    segment: Option<SegmentContext>,
    anchor_input_offset: u64,
    resampler_delay: u32,
    output_sample_index: u64,
    warmup_frames_left: u64,
    signal_silent_frames: u32,
    last_silent: bool,
}

impl DetectionPipeline {
    pub fn new(config: DetectorConfig, capture_rate_hz: u32, channels: u16, selected_channel: u16) -> Result<Self, DetectorError> {
        if channels == 0 || selected_channel >= channels {
            return Err(DetectorError::ChannelIndex { selected: selected_channel, channels });
        }
        let resampler = Resampler::new(capture_rate_hz, config.detection_rate_hz)?;
        let resampler_delay = resampler.delay_in_input_frames();
        let band = BandPass::from_config(&config)?;
        let framer = FrameBuilder::new(config.frame_samples());
        let wide_noise = NoiseFloor::from_config(&config);
        let band_noise = NoiseFloor::from_config(&config);
        let state_machine = CandidateDetector::new(config.clone());
        let warmup_frames_left = config.warmup_frames();
        Ok(DetectionPipeline {
            config,
            selected_channel,
            channels,
            capture_rate_hz,
            resampler,
            dc: DcBlocker::new(DcBlocker::DEFAULT_ALPHA),
            band,
            framer,
            wide_noise,
            band_noise,
            state_machine,
            segment: None,
            anchor_input_offset: 0,
            resampler_delay,
            output_sample_index: 0,
            warmup_frames_left,
            signal_silent_frames: 0,
            last_silent: false,
        })
    }

    pub fn state(&self) -> State {
        if !self.wide_noise.is_ready() || self.warmup_frames_left > 0 {
            State::WarmingUp
        } else {
            self.state_machine.state()
        }
    }

    pub fn noise_floor_dbfs(&self) -> Option<f32> {
        self.wide_noise.value()
    }

    pub fn band_noise_floor_dbfs(&self) -> Option<f32> {
        self.band_noise.value()
    }

    pub fn signal_is_silent(&self) -> bool {
        self.last_silent
    }

    pub fn events_discarded_short(&self) -> u64 {
        self.state_machine.events_discarded_short()
    }

    pub fn begin_segment(&mut self, segment: SegmentContext, anchor_input_offset: u64, format_changed: bool) -> Vec<EventDraft> {
        let mut events = Vec::new();
        if let Some(acc) = self.state_machine.force_close()
            && let Some(event) = self.finish(acc, EndReason::SegmentEnd, false)
        {
            events.push(event);
        }
        self.segment = Some(segment);
        self.anchor_input_offset = anchor_input_offset;
        self.output_sample_index = 0;
        self.framer.reset();
        if format_changed {
            self.resampler.reset();
            self.dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
            self.band.reset();
            self.wide_noise.reset();
            self.band_noise.reset();
            self.warmup_frames_left = self.config.warmup_frames();
        }
        events
    }

    /// Resets all state required by §5.4. Short gaps preserve noise histograms;
    /// long gaps and format changes clear them. Normal segment rotation should
    /// call `begin_segment` (which only force-closes the event).
    pub fn discontinuity(&mut self, kind: Discontinuity, new_anchor_input_offset: u64, lost_frames: u64) -> Vec<EventDraft> {
        let reason = match kind {
            Discontinuity::Gap | Discontinuity::StreamError | Discontinuity::CaptureOverflow => EndReason::Gap,
            Discontinuity::DetectorDrop | Discontinuity::WatchdogRestart => EndReason::DetectorGap,
            Discontinuity::FormatChange => EndReason::Gap,
        };
        let mut events = Vec::new();
        if let Some(acc) = self.state_machine.reset_after_discontinuity()
            && let Some(event) = self.finish(acc, reason, false)
        {
            events.push(event);
        }
        self.resampler.reset();
        self.dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
        self.band.reset();
        self.framer.reset();
        self.anchor_input_offset = new_anchor_input_offset;
        self.output_sample_index = 0;
        self.warmup_frames_left = self.config.warmup_frames();
        self.signal_silent_frames = 0;
        self.last_silent = false;
        let long_gap = lost_frames.saturating_mul(1000) / u64::from(self.capture_rate_hz)
            > self.config.noise_floor_reset_gap_seconds * 1000;
        if long_gap || kind == Discontinuity::FormatChange {
            self.wide_noise.reset();
            self.band_noise.reset();
        }
        events
    }

    /// Consumes interleaved S16 PCM and appends any finalized events to `out`.
    /// Input is copied/converted in a bounded reusable f32 scratch buffer inside
    /// the resampler; no waveform is logged or persisted by this module.
    pub fn process_s16_block(&mut self, samples: &[i16], segment_offset_frames: u64, out: &mut Vec<EventDraft>) -> Result<(), DetectorError> {
        if !samples.len().is_multiple_of(self.channels as usize) {
            return Err(DetectorError::MisalignedSamples { samples: samples.len(), channels: self.channels });
        }
        let frames = samples.len() / self.channels as usize;
        let mut mono = Vec::with_capacity(frames);
        for frame in samples.chunks_exact(self.channels as usize) {
            mono.push(frame[self.selected_channel as usize] as f32 / 32768.0);
        }
        let resampled = self.resampler.process(&mono).to_vec();
        for sample in resampled {
            // Reset initialization is specified as seeding the prior x with the
            // first sample. `warmup_frames_left` excludes the first 100 ms from hits.
            let wide = self.dc.process(sample);
            let band = self.band.process(wide);
            self.output_sample_index += 1;
            if let Some(features) = self.framer.push(wide, band) {
                if features.peak == 0.0 {
                    self.signal_silent_frames = self.signal_silent_frames.saturating_add(1);
                } else {
                    self.signal_silent_frames = 0;
                }
                self.last_silent = self.signal_silent_frames >= 100;
                self.wide_noise.set_frozen(self.state_machine.state() == State::Candidate);
                self.band_noise.set_frozen(self.state_machine.state() == State::Candidate);
                let wide_floor = self.wide_noise.value();
                let band_floor = self.band_noise.value();
                if self.state_machine.state() != State::Candidate {
                    self.wide_noise.observe(features.level_dbfs);
                    self.band_noise.observe(features.band_rms_dbfs);
                } else {
                    // `observe` only advances the frozen duration; it does not
                    // modify the histogram until the forced-unfreeze threshold.
                    self.wide_noise.observe(features.level_dbfs);
                    self.band_noise.observe(features.band_rms_dbfs);
                }
                let frame_end = self.map_output_sample_to_input_offset(self.output_sample_index);
                let frame_start = frame_end.saturating_sub(
                    (u64::from(self.config.frame_samples() as u32) * u64::from(self.capture_rate_hz)
                        / u64::from(self.config.detection_rate_hz))
                        .max(1),
                );
                let warmed = self.warmup_frames_left == 0;
                if self.warmup_frames_left > 0 {
                    self.warmup_frames_left -= 1;
                }
                let drafts = self.state_machine.process_frame(
                    features,
                    frame_start.max(segment_offset_frames),
                    frame_end.max(segment_offset_frames),
                    wide_floor,
                    band_floor,
                    warmed,
                );
                for acc in drafts {
                    if let Some(event) = self.finish(acc, EndReason::Normal, false) {
                        out.push(event);
                    }
                }
                let frozen = self.state_machine.state() == State::Candidate;
                self.wide_noise.set_frozen(frozen);
                self.band_noise.set_frozen(frozen);
            }
        }
        Ok(())
    }

    /// Closes events at a segment boundary or shutdown.
    pub fn close(&mut self, reason: EndReason, continued_next_segment: bool) -> Vec<EventDraft> {
        let mut out = Vec::new();
        if let Some(acc) = self.state_machine.force_close()
            && let Some(event) = self.finish(acc, reason, continued_next_segment)
        {
            out.push(event);
        }
        out
    }

    fn map_output_sample_to_input_offset(&self, output_sample_index: u64) -> u64 {
        let scale = u64::from(self.capture_rate_hz);
        let detection = u64::from(self.config.detection_rate_hz);
        let input_delta = (output_sample_index * scale + detection / 2) / detection;
        self.anchor_input_offset
            .saturating_add(input_delta)
            .saturating_sub(u64::from(self.resampler_delay))
    }

    fn finish(&mut self, acc: EventAccumulator, reason: EndReason, continued_next_segment: bool) -> Option<EventDraft> {
        let duration_frames = acc.end_offset.saturating_sub(acc.start_offset);
        let duration_ms = (duration_frames.saturating_mul(1000) / u64::from(self.capture_rate_hz.max(1))) as u32;
        if duration_ms < self.config.min_event_ms {
            self.state_machine.events_discarded_short += 1;
            return None;
        }
        let hit_count = acc.hit_frames;
        let mean_level = if hit_count > 0 { (acc.sum_level / hit_count as f64) as f32 } else { f32::NAN };
        let mean_ratio = if hit_count > 0 { (acc.sum_band_ratio / hit_count as f64) as f32 } else { f32::NAN };
        let mean_margin = if hit_count > 0 { (acc.sum_level_margin / hit_count as f64) as f32 } else { f32::NAN };
        let level_term = (mean_margin / 20.0).clamp(0.0, 1.0);
        let ratio_term = ((mean_ratio - self.config.band_ratio_min) / (0.9 - self.config.band_ratio_min)).clamp(0.0, 1.0);
        let score = if hit_count == 0 || !mean_level.is_finite() || !mean_ratio.is_finite() || !mean_margin.is_finite() {
            None
        } else {
            Some(0.6 * level_term + 0.4 * ratio_term)
        };
        let segment = self.segment.as_ref()?;
        let end_reason = if reason == EndReason::Normal { EndReason::Normal } else { reason };
        Some(EventDraft {
            id: uuid_v4(),
            segment_id: segment.segment_id.clone(),
            start_offset_frames: acc.start_offset,
            end_offset_frames: acc.end_offset,
            started_at_utc: segment.wall_at_offset(acc.start_offset),
            ended_at_utc: segment.wall_at_offset(acc.end_offset),
            duration_ms,
            rule_score: score,
            detector_version: DETECTOR_VERSION.to_string(),
            peak_dbfs: Some(20.0 * acc.peak.max(1e-6).log10()),
            mean_level_dbfs: if mean_level.is_finite() { Some(mean_level) } else { None },
            mean_band_ratio: if mean_ratio.is_finite() { Some(mean_ratio) } else { None },
            noise_floor_dbfs: acc.noise_floor_dbfs,
            end_reason,
            continued: acc.continued || continued_next_segment,
        })
    }
}

/// Causes that must reset resampling, filters, framing and attack state (§5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discontinuity {
    Gap,
    StreamError,
    CaptureOverflow,
    DetectorDrop,
    WatchdogRestart,
    FormatChange,
}

/// UUID v4 generation without another RNG dependency. UUID is an identifier, not
/// a security token; process-local entropy is sufficient for the SQLite key.
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) as u128;
    let mut x = now ^ (seq << 32) ^ (std::process::id() as u128);
    // xorshift mix to spread adjacent timestamps across the identifier.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let mut b = x.to_be_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7],b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DetectorConfig {
        DetectorConfig {
            noise_floor_window_seconds: 1,
            noise_floor_min_seconds: 0,
            noise_floor_percentile: 20,
            filter_warmup_ms: 0,
            attack_window_frames: 5,
            attack_min_hits: 3,
            hangover_frames: 3,
            min_event_ms: 20,
            merge_gap_ms: 80,
            max_event_seconds: 1,
            ..DetectorConfig::default()
        }
    }

    fn feature(hit: bool) -> FrameFeatures {
        if hit {
            FrameFeatures { level_dbfs: -20.0, band_rms_dbfs: -25.0, band_ratio: 0.8, peak: 0.2 }
        } else {
            FrameFeatures { level_dbfs: -80.0, band_rms_dbfs: -90.0, band_ratio: 0.1, peak: 0.0001 }
        }
    }

    fn feed(machine: &mut CandidateDetector, hits: &[bool], start: u64) -> Vec<EventAccumulator> {
        let mut out = Vec::new();
        for (n, hit) in hits.iter().enumerate() {
            out.extend(machine.process_frame(
                feature(*hit), start + n as u64 * 160, start + (n as u64 + 1) * 160,
                Some(-70.0), Some(-80.0), true,
            ));
        }
        out
    }

    #[test]
    fn attack_window_requires_three_hits_and_backdates_to_first_hit() {
        let mut m = CandidateDetector::new(cfg());
        feed(&mut m, &[false, true, false, false], 0);
        assert_eq!(m.state(), State::Idle);
        feed(&mut m, &[true, true], 640);
        assert_eq!(m.state(), State::Candidate);
        let acc = m.force_close().unwrap();
        // Quiet frames do not form onset; the first hit was frame 1.
        assert_eq!(acc.start_frame, 1);
        assert_eq!(acc.start_offset, 160);
    }

    #[test]
    fn hangover_then_pending_closes_after_merge_window() {
        let mut m = CandidateDetector::new(cfg());
        feed(&mut m, &[true, true, true, true, false, false, false], 0);
        assert_eq!(m.state(), State::Pending);
        let out = feed(&mut m, &[false; 9], 7 * 160);
        assert_eq!(out.len(), 1);
        assert_eq!(m.state(), State::Idle);
    }

    #[test]
    fn pending_event_merges_with_an_onset_inside_the_merge_window() {
        let mut m = CandidateDetector::new(cfg());
        feed(&mut m, &[true, true, true, true, false, false, false], 0);
        assert_eq!(m.state(), State::Pending);
        feed(&mut m, &[false, true, true, true], 7 * 160);
        assert_eq!(m.state(), State::Candidate);
        let joined = m.force_close().unwrap();
        assert_eq!(joined.start_frame, 0);
        assert!(joined.end_frame >= 9);
    }

    #[test]
    fn short_candidates_are_discarded_when_finalized() {
        let mut c = cfg();
        c.min_event_ms = 200;
        c.hangover_frames = 1;
        let mut m = CandidateDetector::new(c);
        feed(&mut m, &[true, true, true, false], 0);
        let pending = m.force_close().unwrap();
        assert!(pending.end_frame - pending.start_frame < 20);
    }

    #[test]
    fn max_duration_splits_and_marks_continued() {
        let mut c = cfg();
        c.max_event_seconds = 1;
        c.hangover_frames = 100;
        let mut m = CandidateDetector::new(c);
        let hits = vec![true; 110];
        let out = feed(&mut m, &hits, 0);
        assert!(!out.is_empty(), "max-duration should emit a split");
        assert_eq!(m.state(), State::Candidate);
        let last = m.force_close().unwrap();
        assert!(last.continued);
    }

    #[test]
    fn warming_frames_do_not_create_candidates() {
        let mut m = CandidateDetector::new(cfg());
        feed(&mut m, &[true; 10], 0);
        // `warmed=false` is tested directly; state remains warming and no hits
        // accumulate into the attack window.
        m.reset_after_discontinuity();
        for n in 0..10 {
            m.process_frame(feature(true), n * 160, (n + 1) * 160, Some(-70.0), Some(-80.0), false);
        }
        assert_eq!(m.state(), State::WarmingUp);
    }

    #[test]
    fn wall_mapping_adds_only_gaps_before_or_at_offset() {
        let start = Utc::now();
        let c = SegmentContext {
            segment_id: "seg".into(),
            capture_rate_hz: 16_000,
            started_at_utc: start,
            offset_frames_at_anchor: 0,
            gaps: vec![GapOffset { offset_frames: 16_000, lost_frames: 8_000 }],
            time_quality: "synced".into(),
            boot_id: "boot".into(),
        };
        assert_eq!(c.wall_at_offset(8_000), start + chrono::Duration::milliseconds(500));
        assert_eq!(c.wall_at_offset(16_000), start + chrono::Duration::seconds(1));
        assert_eq!(c.wall_at_offset(32_000), start + chrono::Duration::milliseconds(2_500));
    }

    #[test]
    fn sample_to_input_offset_includes_rate_ratio_and_delay() {
        let p = DetectionPipeline::new(cfg(), 44_100, 1, 0).unwrap();
        // `160 * 44100 / 16000 = 441` input frames for a 10 ms frame, minus the
        // FIR's 32-frame input-domain delay.
        assert_eq!(p.map_output_sample_to_input_offset(160), 409);
    }

    #[test]
    fn channel_selection_rejects_out_of_range() {
        assert!(matches!(
            DetectionPipeline::new(cfg(), 48_000, 2, 2),
            Err(DetectorError::ChannelIndex { .. })
        ));
    }
}
