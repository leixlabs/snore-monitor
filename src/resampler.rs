//! Exact-rational polyphase resampling (TECH_SPEC §5.1, task T2.2).
//!
//! TECH_SPEC §5.1 requires an *exact* rational ratio, preallocated buffers and
//! state that survives across variable-length capture blocks; §10 suggests
//! `rubato` for the job. `rubato`'s preallocated API is chunk based and takes the
//! ratio as an `f64`, which cannot express 44100 -> 16000 exactly (441:160), so
//! the resampler is implemented here instead: a windowed-sinc polyphase FIR whose
//! ratio is the reduced integer pair `L:M`.
//!
//! Upsample by `L`, low pass at `0.5 / max(L, M)` of the upsampled Nyquist, then
//! keep every `M`-th sample. Only the `L` non-zero phases are ever evaluated, so
//! the cost is `taps` multiply-accumulates per output sample. The filter is
//! linear phase, so its group delay is exactly `(L * taps - 1) / (2 * L)` input
//! samples — what [`RationalResampler::delay_in_input_frames`] reports.

use crate::config::{DETECTION_RATE_HZ, SUPPORTED_CAPTURE_RATES};

/// Taps per polyphase branch. Chosen from the Kaiser design rule for the
/// transition band §5.1 requires (4 kHz passband, 14.5 kHz stopband);
/// `tests/resampler_metrics.rs` measures the resulting response.
pub const DEFAULT_TAPS: usize = 64;

/// Resampling failures that abort startup rather than silently falling back.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResamplerError {
    #[error("unsupported input rate {0} Hz; TECH_SPEC §5.1 allows {1:?}")]
    UnsupportedInputRate(u32, [u32; 5]),
    #[error("output rate {got} Hz must equal the detection rate {expected} Hz")]
    UnsupportedOutputRate { got: u32, expected: u32 },
    #[error("taps per phase must be >= 2, got {0}")]
    InvalidTaps(usize),
}

/// Resampler from one capture rate onto the fixed detection rate.
#[derive(Debug)]
pub enum Resampler {
    /// The capture rate already equals the detection rate.
    Pass,
    Rational(RationalResampler),
}

impl Resampler {
    /// Builds the resampler for `input_rate` -> `output_rate`.
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self, ResamplerError> {
        Self::with_taps(input_rate, output_rate, DEFAULT_TAPS)
    }

    pub fn with_taps(
        input_rate: u32,
        output_rate: u32,
        taps: usize,
    ) -> Result<Self, ResamplerError> {
        if !SUPPORTED_CAPTURE_RATES.contains(&input_rate) {
            return Err(ResamplerError::UnsupportedInputRate(
                input_rate,
                SUPPORTED_CAPTURE_RATES,
            ));
        }
        if output_rate != DETECTION_RATE_HZ {
            return Err(ResamplerError::UnsupportedOutputRate {
                got: output_rate,
                expected: DETECTION_RATE_HZ,
            });
        }
        if input_rate == output_rate {
            return Ok(Resampler::Pass);
        }
        Ok(Resampler::Rational(RationalResampler::new(
            input_rate,
            output_rate,
            taps,
        )?))
    }

    /// Resamples `input` and returns the samples produced by this call.
    pub fn process<'a>(&'a mut self, input: &'a [f32]) -> &'a [f32] {
        match self {
            Resampler::Pass => input,
            Resampler::Rational(r) => r.process(input),
        }
    }

    /// Group delay in input (capture) frames, for §5.1's offset map.
    pub fn delay_in_input_frames(&self) -> u32 {
        match self {
            Resampler::Pass => 0,
            Resampler::Rational(r) => r.delay_in_input_frames(),
        }
    }

    /// Drops all state, as §5.4 requires after a gap or a format change.
    pub fn reset(&mut self) {
        if let Resampler::Rational(r) = self {
            r.reset();
        }
    }

    /// Exact output length for `input_frames` inputs, independent of chunking.
    pub fn output_len_for(&self, input_frames: u64) -> u64 {
        match self {
            Resampler::Pass => input_frames,
            Resampler::Rational(r) => (input_frames * r.l as u64).div_ceil(r.m as u64),
        }
    }
}

/// Polyphase FIR implementing the exact ratio `L:M` (output:input).
#[derive(Debug)]
pub struct RationalResampler {
    /// Upsampling factor of the reduced ratio.
    pub(crate) l: usize,
    /// Downsampling factor of the reduced ratio.
    pub(crate) m: usize,
    taps: usize,
    /// `l * taps` coefficients, phase major: phase `p` occupies
    /// `phases[p * taps .. (p + 1) * taps]`.
    phases: Vec<f32>,
    /// The `taps` input samples that precede the next output, oldest first.
    hist: Vec<f32>,
    /// Input samples consumed so far.
    in_count: u64,
    /// Output samples produced so far.
    out_count: u64,
    /// Scratch holding `hist` followed by the current input chunk.
    work: Vec<f32>,
    out_buf: Vec<f32>,
}

impl RationalResampler {
    pub fn new(input_rate: u32, output_rate: u32, taps: usize) -> Result<Self, ResamplerError> {
        if taps < 2 {
            return Err(ResamplerError::InvalidTaps(taps));
        }
        let divisor = gcd(input_rate, output_rate);
        let l = (output_rate / divisor) as usize;
        let m = (input_rate / divisor) as usize;
        let phases = design_phases(l, m, taps);
        Ok(RationalResampler {
            l,
            m,
            taps,
            phases,
            hist: vec![0.0; taps],
            in_count: 0,
            out_count: 0,
            work: Vec::with_capacity(taps * 2),
            out_buf: Vec::new(),
        })
    }

    /// Exact ratio as `(output, input)`.
    pub fn ratio(&self) -> (usize, usize) {
        (self.l, self.m)
    }

    /// Group delay in input frames, rounded to the nearest whole frame.
    pub fn delay_in_input_frames(&self) -> u32 {
        let centre = (self.l * self.taps - 1) as f64 / 2.0;
        (centre / self.l as f64).round() as u32
    }

    /// Clears filter history and restarts the sample counters.
    pub fn reset(&mut self) {
        self.hist.iter_mut().for_each(|s| *s = 0.0);
        self.in_count = 0;
        self.out_count = 0;
        self.out_buf.clear();
    }

    /// Total input samples consumed since the last reset.
    pub fn input_frames_consumed(&self) -> u64 {
        self.in_count
    }

    /// Total output samples produced since the last reset.
    pub fn output_frames_produced(&self) -> u64 {
        self.out_count
    }

    /// Consumes `input` and returns every output sample that is now determined.
    ///
    /// The return value is only valid until the next call. Output count depends
    /// solely on the number of input frames consumed, never on chunking, so the
    /// same signal split differently yields byte-identical output.
    pub fn process(&mut self, input: &[f32]) -> &[f32] {
        self.work.clear();
        self.work.extend_from_slice(&self.hist);
        self.work.extend_from_slice(input);

        let base = self.in_count;
        let total_in = base + input.len() as u64;
        self.out_buf.clear();

        // Output n is determined once input sample floor(n * M / L) exists, i.e.
        // while n * M < total_in * L.
        while self.out_count * (self.m as u64) < total_in * (self.l as u64) {
            let nm = self.out_count * self.m as u64;
            let q = nm / self.l as u64;
            let p = (nm % self.l as u64) as usize;
            // Index of x[q] inside `work`: `hist` covers everything before `base`.
            let start = (q - base) as usize + self.taps;
            let phase = &self.phases[p * self.taps..(p + 1) * self.taps];
            let mut acc = 0.0f32;
            for (i, coefficient) in phase.iter().enumerate() {
                acc += coefficient * self.work[start - i];
            }
            self.out_buf.push(acc);
            self.out_count += 1;
        }

        // `work` is always at least `taps` long, so the history window is exact.
        let keep_from = self.work.len() - self.taps;
        self.hist.copy_from_slice(&self.work[keep_from..]);
        self.in_count = total_in;
        &self.out_buf
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

/// Builds `l * taps` polyphase coefficients for the reduced ratio `l:m`.
///
/// The prototype low pass is a windowed sinc with cutoff `0.5 / max(l, m)` of the
/// upsampled Nyquist and a Kaiser window (`beta = 8`), then scaled so the mean
/// phase sum is exactly 1 — that is the unity-gain condition for this structure,
/// because the upsampled signal carries each input sample once every `l` samples.
fn design_phases(l: usize, m: usize, taps: usize) -> Vec<f32> {
    let n = l * taps;
    let fc = 0.5 / l.max(m) as f64;
    let centre = (n - 1) as f64 / 2.0;
    let beta = 8.0;
    let window_norm = bessel_i0(beta);

    let mut prototype = vec![0.0f64; n];
    for (i, slot) in prototype.iter_mut().enumerate() {
        let t = if n > 1 {
            2.0 * i as f64 / (n - 1) as f64 - 1.0
        } else {
            0.0
        };
        let window = bessel_i0(beta * (1.0 - t * t).max(0.0).sqrt()) / window_norm;
        *slot = 2.0 * fc * sinc(2.0 * fc * (i as f64 - centre)) * window;
    }

    let total: f64 = prototype.iter().sum();
    let scale = if total.abs() > 1e-12 {
        l as f64 / total
    } else {
        1.0
    };

    let mut phases = vec![0.0f32; n];
    for p in 0..l {
        for k in 0..taps {
            phases[p * taps + k] = (prototype[p + k * l] * scale) as f32;
        }
    }
    phases
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

/// Modified Bessel function of the first kind, order 0, by its power series.
fn bessel_i0(x: f64) -> f64 {
    let half = x / 2.0;
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    for k in 1..=48 {
        let step = half / k as f64;
        term *= step * step;
        sum += term;
        if term < 1e-17 * sum {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratios_reduce_exactly() {
        for (input, expected) in [
            (16_000u32, (1usize, 1usize)),
            (32_000, (1, 2)),
            (48_000, (1, 3)),
            (44_100, (160, 441)),
            (96_000, (1, 6)),
        ] {
            match Resampler::new(input, DETECTION_RATE_HZ).unwrap() {
                Resampler::Pass => assert_eq!(expected, (1, 1), "input {input}"),
                Resampler::Rational(r) => assert_eq!(r.ratio(), expected, "input {input}"),
            }
        }
    }

    #[test]
    fn output_length_matches_the_exact_ratio() {
        // One second of input at any supported rate is exactly one second at
        // the detection rate: 16000 output frames, whatever the chunking.
        for (input_rate, expected_out) in [
            (32_000u32, 16_000usize),
            (48_000, 16_000),
            (44_100, 16_000),
            (96_000, 16_000),
        ] {
            let mut r = Resampler::new(input_rate, DETECTION_RATE_HZ).unwrap();
            // One second of input, delivered in ragged chunks.
            let total_in = input_rate as usize;
            let mut produced = 0usize;
            let mut fed = 0usize;
            for chunk in [1usize, 7, 480, 1000, 3333, 9999, 100_000] {
                let take = chunk.min(total_in - fed);
                if take == 0 {
                    continue;
                }
                let block = vec![0.0f32; take];
                produced += r.process(&block).len();
                fed += take;
            }
            assert_eq!(fed, total_in);
            assert_eq!(produced, expected_out, "input rate {input_rate}");
            assert_eq!(
                r.output_len_for(total_in as u64) as usize,
                expected_out,
                "input rate {input_rate}"
            );
        }
    }

    #[test]
    fn chunking_never_changes_the_output() {
        let input_rate = 44_100u32;
        let signal: Vec<f32> = (0..44_100)
            .map(|i| (i as f32 * 0.01).sin() * 0.5)
            .collect();

        let mut one_shot = Resampler::new(input_rate, DETECTION_RATE_HZ).unwrap();
        let reference = one_shot.process(&signal).to_vec();

        for chunk in [1usize, 13, 160, 441, 4096] {
            let mut chunked = Resampler::new(input_rate, DETECTION_RATE_HZ).unwrap();
            let mut out = Vec::new();
            for part in signal.chunks(chunk) {
                out.extend_from_slice(chunked.process(part));
            }
            assert_eq!(out.len(), reference.len(), "chunk {chunk}");
            assert!(
                out.iter().zip(&reference).all(|(a, b)| (a - b).abs() < 1e-6),
                "chunk size {chunk} changed the output"
            );
        }
    }

    #[test]
    fn dc_passes_at_unity_gain() {
        for input_rate in [16_000u32, 32_000, 44_100, 48_000, 96_000] {
            let mut r = Resampler::new(input_rate, DETECTION_RATE_HZ).unwrap();
            let block = vec![1.0f32; input_rate as usize];
            let out = r.process(&block);
            // Ignore the startup transient, then require unity gain.
            let tail = &out[out.len() / 2..];
            let mean: f32 = tail.iter().sum::<f32>() / tail.len() as f32;
            assert!(
                (mean - 1.0).abs() < 1e-3,
                "input rate {input_rate} DC gain was {mean}"
            );
        }
    }

    #[test]
    fn reset_restores_the_initial_state() {
        let mut r = Resampler::new(48_000, DETECTION_RATE_HZ).unwrap();
        let a = r.process(&vec![0.25f32; 480]).to_vec();
        r.reset();
        let b = r.process(&vec![0.25f32; 480]).to_vec();
        assert_eq!(a, b);
    }

    #[test]
    fn rejects_rates_outside_the_spec_table() {
        assert!(matches!(
            Resampler::new(22_050, DETECTION_RATE_HZ),
            Err(ResamplerError::UnsupportedInputRate(22_050, _))
        ));
        assert!(matches!(
            Resampler::new(48_000, 8_000),
            Err(ResamplerError::UnsupportedOutputRate { got: 8_000, .. })
        ));
    }
}
