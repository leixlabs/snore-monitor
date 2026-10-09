//! Per-frame signal chain and noise-floor estimation (TECH_SPEC §5.2, tasks T2.3
//! and T2.4).
//!
//! Everything here is allocation free once constructed and carries state across
//! capture blocks, because CPAL block lengths vary (TECH_SPEC §4.1). A *frame* is
//! 10 ms of the 16 kHz detection stream; the pipeline feeds samples one at a time
//! and this module turns them into [`FrameFeatures`].

use crate::config::DetectorConfig;
use biquad::{Biquad, Coefficients, DirectForm2Transposed, ToHertz, Type, Q_BUTTERWORTH_F32};

/// Lowest dBFS value the feature stage reports (TECH_SPEC §5.2).
pub const DBFS_FLOOR: f32 = -120.0;
/// `max(rms, 1e-6)` in TECH_SPEC §5.2 is exactly the -120 dBFS floor.
const RMS_FLOOR: f32 = 1e-6;
/// The `ε` added to wideband energy in the band-ratio denominator (§5.2).
const BAND_RATIO_EPSILON: f32 = 1e-10;

#[derive(Debug, thiserror::Error)]
pub enum DspError {
    #[error("cannot design {kind} biquad at {frequency_hz} Hz for a {rate_hz} Hz stream: {biquad_error:?}")]
    FilterDesign {
        kind: &'static str,
        frequency_hz: f32,
        rate_hz: f32,
        biquad_error: biquad::Errors,
    },
}

/// First-order DC blocker: `y[n] = x[n] - x[n-1] + alpha * y[n-1]`.
///
/// `alpha = 0.995` puts the corner near 12.7 Hz at 16 kHz (TECH_SPEC §5.2).
#[derive(Debug, Clone)]
pub struct DcBlocker {
    alpha: f32,
    x_prev: f32,
    y_prev: f32,
}

impl DcBlocker {
    pub const DEFAULT_ALPHA: f32 = 0.995;

    pub fn new(alpha: f32) -> Self {
        DcBlocker {
            alpha,
            x_prev: 0.0,
            y_prev: 0.0,
        }
    }

    /// §5.2: on reset `x[n-1]` takes the first sample of the new stream and
    /// `y[n-1]` is zero, so a DC step at the gap does not ring through the filter.
    pub fn reset(&mut self, first_sample: f32) {
        self.x_prev = first_sample;
        self.y_prev = 0.0;
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = x - self.x_prev + self.alpha * self.y_prev;
        self.x_prev = x;
        self.y_prev = y;
        y
    }
}

/// Second-order Butterworth high pass followed by a second-order Butterworth low
/// pass — the 80-1500 Hz snore band of TECH_SPEC §5.2.
///
/// Coefficients come from the `biquad` crate, which generates them with the RBJ
/// cookbook formulas the spec names. `DirectForm2Transposed` is used because the
/// coefficients are static.
#[derive(Debug)]
pub struct BandPass {
    low_hz: f32,
    high_hz: f32,
    high_coeffs: Coefficients<f32>,
    low_coeffs: Coefficients<f32>,
    high: DirectForm2Transposed<f32>,
    low: DirectForm2Transposed<f32>,
}

impl BandPass {
    pub fn new(rate_hz: f32, low_hz: f32, high_hz: f32) -> Result<Self, DspError> {
        let high_coeffs = Coefficients::<f32>::from_params(
            Type::HighPass,
            rate_hz.hz(),
            low_hz.hz(),
            Q_BUTTERWORTH_F32,
        )
        .map_err(|biquad_error| DspError::FilterDesign {
            kind: "high-pass",
            frequency_hz: low_hz,
            rate_hz,
            biquad_error,
        })?;
        let low_coeffs = Coefficients::<f32>::from_params(
            Type::LowPass,
            rate_hz.hz(),
            high_hz.hz(),
            Q_BUTTERWORTH_F32,
        )
        .map_err(|biquad_error| DspError::FilterDesign {
            kind: "low-pass",
            frequency_hz: high_hz,
            rate_hz,
            biquad_error,
        })?;
        Ok(BandPass {
            low_hz,
            high_hz,
            high: DirectForm2Transposed::<f32>::new(high_coeffs),
            low: DirectForm2Transposed::<f32>::new(low_coeffs),
            high_coeffs,
            low_coeffs,
        })
    }

    pub fn from_config(config: &DetectorConfig) -> Result<Self, DspError> {
        BandPass::new(
            config.detection_rate_hz as f32,
            config.band_low_hz,
            config.band_high_hz,
        )
    }

    /// Zeroes both filter states (§5.4 step 3).
    pub fn reset(&mut self) {
        self.high = DirectForm2Transposed::<f32>::new(self.high_coeffs);
        self.low = DirectForm2Transposed::<f32>::new(self.low_coeffs);
    }

    pub fn process(&mut self, x: f32) -> f32 {
        self.low.run(self.high.run(x))
    }

    pub fn band(&self) -> (f32, f32) {
        (self.low_hz, self.high_hz)
    }
}

/// Everything the state machine needs from one frame (TECH_SPEC §5.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameFeatures {
    /// Wideband level after the DC blocker, in dBFS, floored at -120.
    pub level_dbfs: f32,
    /// Level of the 80-1500 Hz band, in dBFS, floored at -120.
    pub band_rms_dbfs: f32,
    /// `band_energy / (wideband_energy + ε)`.
    pub band_ratio: f32,
    /// Largest absolute DC-blocked sample in the frame.
    pub peak: f32,
}

impl FrameFeatures {
    /// A frame of digital silence.
    pub const SILENT: FrameFeatures = FrameFeatures {
        level_dbfs: DBFS_FLOOR,
        band_rms_dbfs: DBFS_FLOOR,
        band_ratio: 0.0,
        peak: 0.0,
    };
}

/// Accumulates samples into fixed-length frames.
///
/// A partial frame survives across capture blocks but is discarded on reset, so
/// frames are never stitched across a gap (TECH_SPEC §5.4 step 5).
#[derive(Debug)]
pub struct FrameBuilder {
    frame_samples: usize,
    filled: usize,
    wide_sq_sum: f32,
    band_sq_sum: f32,
    peak: f32,
}

impl FrameBuilder {
    pub fn new(frame_samples: usize) -> Self {
        FrameBuilder {
            frame_samples: frame_samples.max(1),
            filled: 0,
            wide_sq_sum: 0.0,
            band_sq_sum: 0.0,
            peak: 0.0,
        }
    }

    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Drops the partial frame without emitting it.
    pub fn reset(&mut self) {
        self.filled = 0;
        self.wide_sq_sum = 0.0;
        self.band_sq_sum = 0.0;
        self.peak = 0.0;
    }

    /// Adds one sample pair; returns features when a frame completes.
    pub fn push(&mut self, wideband: f32, band: f32) -> Option<FrameFeatures> {
        self.wide_sq_sum += wideband * wideband;
        self.band_sq_sum += band * band;
        let magnitude = wideband.abs();
        if magnitude > self.peak {
            self.peak = magnitude;
        }
        self.filled += 1;
        if self.filled < self.frame_samples {
            return None;
        }
        let features = FrameFeatures {
            level_dbfs: dbfs_from_energy(self.wide_sq_sum, self.frame_samples),
            band_rms_dbfs: dbfs_from_energy(self.band_sq_sum, self.frame_samples),
            band_ratio: self.band_sq_sum / (self.wide_sq_sum + BAND_RATIO_EPSILON),
            peak: self.peak,
        };
        self.reset();
        Some(features)
    }
}

/// `20 * log10(max(rms, 1e-6))`, i.e. a -120 dBFS floor (TECH_SPEC §5.2).
fn dbfs_from_energy(energy: f32, samples: usize) -> f32 {
    let rms = (energy / samples as f32).sqrt();
    let bounded = if rms.is_finite() { rms.max(RMS_FLOOR) } else { RMS_FLOOR };
    (20.0 * bounded.log10()).max(DBFS_FLOOR)
}

/// Number of histogram buckets: 1 dB each over -100..0 dBFS (TECH_SPEC §5.2).
const BUCKETS: usize = 100;
/// How many frames between percentile recomputations (100 ms at 10 ms frames).
const ESTIMATE_INTERVAL_FRAMES: u32 = 10;

/// One 20th-percentile noise floor over a sliding window of frames.
///
/// TECH_SPEC §5.2 asks for two of these — one on the wideband level and one on
/// the band level — because comparing a wideband floor against a band level is
/// dimensionally wrong.
#[derive(Debug)]
pub struct NoiseFloor {
    counts: [u32; BUCKETS],
    /// Bucket index of each frame in the window, oldest at `head`.
    window: Vec<u8>,
    head: usize,
    filled: usize,
    percentile: u32,
    min_frames: usize,
    frozen: bool,
    frozen_frames: u64,
    freeze_max_frames: u64,
    frames_since_estimate: u32,
    cached: Option<f32>,
}

impl NoiseFloor {
    pub fn new(
        window_frames: usize,
        percentile: u32,
        min_frames: usize,
        freeze_max_frames: u64,
    ) -> Self {
        let window_frames = window_frames.max(1);
        NoiseFloor {
            counts: [0; BUCKETS],
            window: vec![0; window_frames],
            head: 0,
            filled: 0,
            percentile,
            min_frames,
            frozen: false,
            frozen_frames: 0,
            freeze_max_frames,
            frames_since_estimate: ESTIMATE_INTERVAL_FRAMES,
            cached: None,
        }
    }

    pub fn from_config(config: &DetectorConfig) -> Self {
        NoiseFloor::new(
            config.noise_floor_window_frames(),
            config.noise_floor_percentile,
            config.noise_floor_min_frames(),
            u64::from(config.noise_freeze_max_seconds) * 1000 / u64::from(config.frame_ms.max(1)),
        )
    }

    /// Number of frames currently in the window.
    pub fn window_len(&self) -> usize {
        self.filled
    }

    /// Clears the window; used after a long gap or a format change (§5.2).
    pub fn reset(&mut self) {
        self.counts = [0; BUCKETS];
        self.window.iter_mut().for_each(|b| *b = 0);
        self.head = 0;
        self.filled = 0;
        self.frozen = false;
        self.frozen_frames = 0;
        self.frames_since_estimate = ESTIMATE_INTERVAL_FRAMES;
        self.cached = None;
    }

    /// Freezes or resumes window movement. The detector freezes while a candidate
    /// is open so a sustained noise source cannot pull the floor up mid-event.
    pub fn set_frozen(&mut self, frozen: bool) {
        if frozen == self.frozen {
            return;
        }
        self.frozen = frozen;
        self.frozen_frames = 0;
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Adds one frame. While frozen the window does not move, except that a
    /// freeze longer than `noise_freeze_max_seconds` force-resumes it (§5.2).
    pub fn observe(&mut self, level_dbfs: f32) {
        if self.frozen {
            self.frozen_frames += 1;
            if self.frozen_frames < self.freeze_max_frames {
                return;
            }
            self.frozen = false;
            self.frozen_frames = 0;
        }
        self.insert(bucket_of(level_dbfs));
        self.frames_since_estimate += 1;
        if self.frames_since_estimate >= ESTIMATE_INTERVAL_FRAMES {
            self.frames_since_estimate = 0;
            self.cached = self.compute();
        }
    }

    fn insert(&mut self, bucket: u8) {
        if self.filled == self.window.len() {
            let evicted = self.window[self.head];
            self.counts[evicted as usize] = self.counts[evicted as usize].saturating_sub(1);
        }
        self.window[self.head] = bucket;
        self.counts[bucket as usize] += 1;
        self.head = (self.head + 1) % self.window.len();
        if self.filled < self.window.len() {
            self.filled += 1;
        }
    }

    /// The current estimate, or `None` while warming up.
    pub fn value(&self) -> Option<f32> {
        self.cached
    }

    /// Whether the window holds enough frames to trust an estimate.
    pub fn is_ready(&self) -> bool {
        self.filled >= self.min_frames
    }

    fn compute(&self) -> Option<f32> {
        let total = self.filled;
        if total < self.min_frames {
            return None;
        }
        let target = (u64::from(self.percentile) * total as u64).div_ceil(100).max(1);
        let mut cumulative = 0u64;
        for (index, count) in self.counts.iter().enumerate() {
            cumulative += u64::from(*count);
            if cumulative >= target {
                return Some(-100.0 + index as f32 + 0.5);
            }
        }
        Some(-100.0 + BUCKETS as f32 - 0.5)
    }
}

/// Bucket index for a dBFS value: bucket `i` covers `[-100 + i, -99 + i)`, and
/// values outside the range clamp to the first or last bucket (§5.2).
pub fn bucket_of(level_dbfs: f32) -> u8 {
    let raw = if level_dbfs.is_finite() { level_dbfs } else { -100.0 };
    let index = (raw + 100.0).floor();
    index.clamp(0.0, (BUCKETS - 1) as f32) as u8
}

/// Centre of a bucket, which is what the percentile estimate reports.
pub fn bucket_centre(bucket: u8) -> f32 {
    -100.0 + bucket as f32 + 0.5
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Drives `count` samples of a sine through the DC blocker and returns the
    /// settled RMS.
    fn dc_blocked_sine_rms(frequency_hz: f32, amplitude: f32, rate_hz: f32, count: usize) -> f32 {
        let mut dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
        dc.reset(0.0);
        let mut tail = Vec::new();
        for n in 0..count {
            let x = amplitude * (2.0 * PI * frequency_hz * n as f32 / rate_hz).sin();
            let y = dc.process(x);
            if n >= count / 2 {
                tail.push(y);
            }
        }
        (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt()
    }

    #[test]
    fn dbfs_matches_a_known_amplitude() {
        // A 0.5 amplitude sine has RMS 0.35355, i.e. -9.03 dBFS.
        let rms = dc_blocked_sine_rms(1_000.0, 0.5, 16_000.0, 16_000);
        let dbfs = 20.0 * rms.log10();
        assert!(
            (dbfs - -9.0309).abs() < 0.1,
            "expected about -9.03 dBFS, measured {dbfs}"
        );
    }

    #[test]
    fn frame_features_report_expected_levels() {
        let mut builder = FrameBuilder::new(160);
        let mut dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
        let mut band = BandPass::new(16_000.0, 80.0, 1_500.0).unwrap();
        dc.reset(0.0);
        let mut last = None;
        for n in 0..16_000 {
            let x = 0.5 * (2.0 * PI * 300.0 * n as f32 / 16_000.0).sin();
            let wide = dc.process(x);
            let in_band = band.process(wide);
            if let Some(features) = builder.push(wide, in_band) {
                last = Some(features);
            }
        }
        let features = last.expect("frames were produced");
        assert!(
            (features.level_dbfs - -9.03).abs() < 0.3,
            "level {} dBFS",
            features.level_dbfs
        );
        assert!(
            features.band_ratio > 0.9,
            "a 300 Hz tone is in band, ratio was {}",
            features.band_ratio
        );
        assert!(features.peak <= 0.51 && features.peak > 0.4);
    }

    #[test]
    fn band_ratio_rejects_out_of_band_energy() {
        let mut builder = FrameBuilder::new(160);
        let mut dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
        let mut band = BandPass::new(16_000.0, 80.0, 1_500.0).unwrap();
        dc.reset(0.0);
        let mut last = None;
        for n in 0..16_000 {
            let x = 0.5 * (2.0 * PI * 6_000.0 * n as f32 / 16_000.0).sin();
            let wide = dc.process(x);
            let in_band = band.process(wide);
            if let Some(features) = builder.push(wide, in_band) {
                last = Some(features);
            }
        }
        let features = last.expect("frames were produced");
        assert!(
            features.band_ratio < 0.02,
            "6 kHz is far outside the band, ratio was {}",
            features.band_ratio
        );
        // The wideband level is unaffected by the band filter.
        assert!((features.level_dbfs - -9.03).abs() < 0.3);
    }

    #[test]
    fn bandpass_hits_minus_three_db_at_its_corners() {
        for (frequency, expected_db) in [(80.0f32, -3.0f32), (1_500.0, -3.0)] {
            let gain_db = bandpass_gain_db(frequency);
            assert!(
                (gain_db - expected_db).abs() <= 0.5,
                "{frequency} Hz corner measured {gain_db} dB, expected {expected_db} +/- 0.5 dB"
            );
        }
    }

    #[test]
    fn bandpass_rejects_forty_hertz_by_at_least_eleven_db() {
        let gain_db = bandpass_gain_db(40.0);
        assert!(
            gain_db <= -11.0,
            "40 Hz measured {gain_db} dB, TECH_SPEC §5.2 requires <= -11 dB"
        );
    }

    fn bandpass_gain_db(frequency_hz: f32) -> f32 {
        let rate = 16_000.0f32;
        let mut band = BandPass::new(rate, 80.0, 1_500.0).unwrap();
        let count = 32_000usize;
        let mut tail = Vec::new();
        for n in 0..count {
            let x = 0.5 * (2.0 * PI * frequency_hz * n as f32 / rate).sin();
            let y = band.process(x);
            if n >= count / 2 {
                tail.push(y);
            }
        }
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
        20.0 * rms.log10() - 20.0 * (0.5 / 2f32.sqrt()).log10()
    }

    #[test]
    fn silence_never_produces_nan() {
        let mut builder = FrameBuilder::new(160);
        let mut dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
        let mut band = BandPass::new(16_000.0, 80.0, 1_500.0).unwrap();
        dc.reset(0.0);
        for _ in 0..1_600 {
            let wide = dc.process(0.0);
            let in_band = band.process(wide);
            if let Some(f) = builder.push(wide, in_band) {
                assert!(f.level_dbfs.is_finite() && f.band_rms_dbfs.is_finite());
                assert!(f.band_ratio.is_finite());
                assert_eq!(f.peak, 0.0);
                assert_eq!(f.level_dbfs, DBFS_FLOOR);
            }
        }
    }

    #[test]
    fn dc_blocker_reset_uses_the_first_sample() {
        let mut dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
        dc.reset(0.7);
        // With x[n-1] seeded to the first sample and y[n-1] = 0, a constant input
        // produces no step transient: the first output is exactly zero.
        assert_eq!(dc.process(0.7), 0.0);
    }

    #[test]
    fn frame_boundaries_do_not_depend_on_chunking() {
        let samples: Vec<f32> = (0..1_600)
            .map(|n| 0.3 * (2.0 * PI * 220.0 * n as f32 / 16_000.0).sin())
            .collect();
        let run = |chunk: usize| {
            let mut builder = FrameBuilder::new(160);
            let mut dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
            let mut band = BandPass::new(16_000.0, 80.0, 1_500.0).unwrap();
            dc.reset(0.0);
            let mut out = Vec::new();
            for part in samples.chunks(chunk) {
                for x in part {
                    let wide = dc.process(*x);
                    let in_band = band.process(wide);
                    if let Some(f) = builder.push(wide, in_band) {
                        out.push(f);
                    }
                }
            }
            out
        };
        let reference = run(samples.len());
        for chunk in [1usize, 7, 160, 333] {
            let other = run(chunk);
            assert_eq!(other.len(), reference.len(), "chunk {chunk}");
            for (a, b) in other.iter().zip(&reference) {
                assert!((a.level_dbfs - b.level_dbfs).abs() < 1e-4, "chunk {chunk}");
                assert!((a.band_ratio - b.band_ratio).abs() < 1e-4, "chunk {chunk}");
            }
        }
    }

    #[test]
    fn buckets_clamp_out_of_range_values() {
        assert_eq!(bucket_of(-140.0), 0);
        assert_eq!(bucket_of(-100.0), 0);
        assert_eq!(bucket_of(-99.5), 0);
        assert_eq!(bucket_of(-50.0), 50);
        assert_eq!(bucket_of(0.0), 99);
        assert_eq!(bucket_of(20.0), 99);
        assert_eq!(bucket_of(f32::NAN), 0);
        assert_eq!(bucket_centre(0), -99.5);
        assert_eq!(bucket_centre(99), -0.5);
    }

    #[test]
    fn noise_floor_matches_a_hand_computed_percentile() {
        // 100 frames at -60 dBFS and 100 at -40 dBFS: the 20th percentile lands in
        // the -60 bucket, whose centre is -59.5.
        let mut floor = NoiseFloor::new(3_000, 20, 100, 30_000);
        for _ in 0..100 {
            floor.observe(-60.0);
        }
        for _ in 0..100 {
            floor.observe(-40.0);
        }
        assert_eq!(floor.value(), Some(-59.5));
        assert_eq!(floor.window_len(), 200);
    }

    #[test]
    fn noise_floor_is_unknown_while_warming_up() {
        let mut floor = NoiseFloor::new(3_000, 20, 1_000, 30_000);
        for _ in 0..999 {
            floor.observe(-70.0);
        }
        assert!(!floor.is_ready());
        assert_eq!(floor.value(), None);
        for _ in 0..11 {
            floor.observe(-70.0);
        }
        assert!(floor.is_ready());
        assert_eq!(floor.value(), Some(-69.5));
    }

    #[test]
    fn noise_floor_freezes_and_then_force_resumes() {
        // freeze_max_frames is given in frames: 30 s of 10 ms frames is 3000.
        let mut floor = NoiseFloor::new(3_000, 20, 100, 3_000);
        for _ in 0..3_000 {
            floor.observe(-70.0);
        }
        assert_eq!(floor.value(), Some(-69.5));
        floor.set_frozen(true);
        // While frozen the window does not move.
        for _ in 0..2_999 {
            floor.observe(-20.0);
        }
        assert_eq!(floor.value(), Some(-69.5), "window moved while frozen");
        // The 3000th frozen frame force-resumes and the loud frames take over.
        floor.observe(-20.0);
        assert!(!floor.is_frozen());
        for _ in 0..3_000 {
            floor.observe(-20.0);
        }
        assert_eq!(
            floor.value(),
            Some(-19.5),
            "the floor should rise once the freeze is released"
        );
    }

    #[test]
    fn noise_floor_reset_clears_the_window() {
        let mut floor = NoiseFloor::new(3_000, 20, 100, 3_000);
        for _ in 0..500 {
            floor.observe(-70.0);
        }
        floor.reset();
        assert_eq!(floor.window_len(), 0);
        assert_eq!(floor.value(), None);
    }

    #[test]
    fn noise_floor_survives_a_short_gap() {
        // A short gap must not clear the histogram (§5.2); this is expressed by
        // simply not calling `reset`.
        let mut floor = NoiseFloor::new(3_000, 20, 100, 3_000);
        for _ in 0..2_000 {
            floor.observe(-65.0);
        }
        let before = floor.value();
        for _ in 0..10 {
            floor.observe(-65.0);
        }
        assert_eq!(floor.value(), before);
        assert_eq!(floor.window_len(), 2_010);
    }
}
