//! Measurements TECH_SPEC §5.1 requires from the detection-branch resampler.
//!
//! §5.1 sets two numbers: passband 0-4 kHz flat within ±0.5 dB, and at least
//! 60 dB of attenuation for input energy that would fold back into the snore band
//! (14.5-15.9 kHz lands at 100-1500 Hz after decimation). Both are measured here
//! by driving the resampler with a steady sine and comparing output RMS against
//! the input RMS, which is what the real pipeline will see.

use snore_monitor::config::DETECTION_RATE_HZ;
use snore_monitor::resampler::Resampler;
use std::f64::consts::PI;

const CAPTURE_RATES: [u32; 5] = [16_000, 32_000, 44_100, 48_000, 96_000];

/// Feeds a sine of `freq` Hz at `amplitude` through a fresh resampler and
/// returns the gain in dB measured over the settled part of the output.
fn measured_gain_db(capture_rate: u32, freq: f64, amplitude: f64) -> f64 {
    let mut resampler = Resampler::new(capture_rate, DETECTION_RATE_HZ).expect("supported rate");
    let total = capture_rate as usize * 2;
    let step = 2.0 * PI * freq / capture_rate as f64;
    let mut phase = 0.0f64;
    let mut output: Vec<f32> = Vec::with_capacity(total);

    let mut fed = 0usize;
    while fed < total {
        let take = 1024.min(total - fed);
        let mut chunk = Vec::with_capacity(take);
        for _ in 0..take {
            chunk.push((amplitude * phase.sin()) as f32);
            phase += step;
        }
        output.extend_from_slice(resampler.process(&chunk));
        fed += take;
    }

    // Skip the filter transient: the second half is fully settled.
    let settled = &output[output.len() / 2..];
    let out_rms = (settled.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>()
        / settled.len() as f64)
        .sqrt();
    let in_rms = amplitude / 2f64.sqrt();
    if out_rms <= 0.0 {
        return f64::NEG_INFINITY;
    }
    20.0 * (out_rms / in_rms).log10()
}

#[test]
fn passband_is_flat_within_half_a_db() {
    for rate in CAPTURE_RATES {
        for freq in [100.0, 250.0, 500.0, 1_000.0, 2_000.0, 3_000.0, 4_000.0] {
            let gain = measured_gain_db(rate, freq, 0.5);
            assert!(
                gain.abs() <= 0.5,
                "{rate} Hz capture, {freq} Hz tone: passband gain {gain:.3} dB is outside ±0.5 dB"
            );
        }
    }
}

#[test]
fn alias_band_is_attenuated_by_at_least_60_db() {
    for rate in CAPTURE_RATES {
        for freq in [14_500.0, 15_000.0, 15_500.0, 15_900.0] {
            if freq >= rate as f64 / 2.0 {
                continue;
            }
            let gain = measured_gain_db(rate, freq, 0.5);
            assert!(
                gain <= -60.0,
                "{rate} Hz capture, {freq} Hz tone folds into the snore band at {gain:.1} dB; \
                 TECH_SPEC §5.1 requires <= -60 dB"
            );
        }
    }
}

#[test]
fn snore_band_content_survives_the_whole_chain() {
    // 300 Hz sits in the middle of the snore band and must come through cleanly
    // at every capture rate, including the awkward 44100 Hz case.
    for rate in CAPTURE_RATES {
        let gain = measured_gain_db(rate, 300.0, 0.5);
        assert!(
            gain.abs() <= 0.5,
            "{rate} Hz capture: 300 Hz snore-band tone measured {gain:.3} dB"
        );
    }
}
