//! Offline detection over WAV files (TECH_SPEC §5.5, T2.7).
//!
//! This is the threshold-calibration tool: it runs the *same* [`DetectionPipeline`]
//! the live service uses, so a result here is what the service would have stored
//! for the same PCM. No detection logic is duplicated.
//!
//! The per-frame feature curve (`--frames`) is built from the public primitives in
//! [`crate::dsp`] and [`crate::resampler`]; the correspondence to the pipeline is
//! one-to-one and is spelled out in [`FeatureTrace`].

use crate::config::DetectorConfig;
use crate::detector::{DetectionPipeline, EndReason, EventDraft, SegmentContext};
use crate::error::{Error, Result};
use crate::resampler::Resampler;
use crate::dsp::{BandPass, DcBlocker, FrameBuilder};
use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Fixed feed size. `DetectionPipeline::process_s16_block` accepts variable
/// lengths, so this only bounds the tool's memory, not the algorithm.
pub const BLOCK_FRAMES: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum DetectWavError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: hound::Error,
    },
    #[error("{path}: {message}")]
    Unsupported { path: PathBuf, message: String },
    #[error(transparent)]
    Detector(#[from] crate::detector::DetectorError),
    #[error(transparent)]
    Dsp(#[from] crate::dsp::DspError),
    #[error(transparent)]
    Resampler(#[from] crate::resampler::ResamplerError),
    #[error(transparent)]
    Io(#[from] Error),
}

/// One row of the `--frames` feature curve.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FrameTrace {
    pub index: u64,
    pub level_dbfs: f32,
    pub band_rms_dbfs: f32,
    pub band_ratio: f32,
    pub peak: f32,
    /// Wall-clock time of the frame start, derived from the file's anchor.
    pub at_utc: DateTime<Utc>,
}

/// Result of running detection over one file.
#[derive(Debug, Serialize)]
pub struct DetectionResult {
    pub file: String,
    pub capture_rate_hz: u32,
    pub channels: u16,
    pub mono_channel_index: u16,
    pub frame_count: u64,
    pub detector_version: String,
    pub events: Vec<EventDraft>,
    /// Seconds of audio processed, for the §5.5/T2.8 cost reference.
    pub duration_seconds: f64,
    /// Wall-clock seconds spent in the DSP path.
    pub processing_seconds: f64,
    /// Frames per second of detection throughput (audio seconds per wall second).
    pub realtime_factor: f64,
}

/// A per-frame feature chain mirroring `DetectionPipeline` sample-for-sample.
///
/// The pipeline is, in order: channel select + S16→f32 (`/32768.0`), resample to
/// the detection rate, DC block, band pass, 10 ms framing. This struct reproduces
/// exactly that chain from public primitives so the `--frames` curve matches what
/// the state machine saw, without widening `DetectionPipeline`'s interface.
pub struct FeatureTrace {
    resampler: Resampler,
    dc: DcBlocker,
    band: BandPass,
    framer: FrameBuilder,
    frame_index: u64,
    started_at: DateTime<Utc>,
    capture_rate_hz: u32,
}

impl FeatureTrace {
    pub fn new(config: &DetectorConfig, capture_rate_hz: u32, started_at: DateTime<Utc>) -> Result<Self, DetectWavError> {
        let resampler = Resampler::new(capture_rate_hz, config.detection_rate_hz)?;
        let band = BandPass::from_config(config)?;
        Ok(FeatureTrace {
            resampler,
            dc: DcBlocker::new(DcBlocker::DEFAULT_ALPHA),
            band,
            framer: FrameBuilder::new(config.frame_samples()),
            frame_index: 0,
            started_at,
            capture_rate_hz,
        })
    }

    /// Feeds interleaved S16 PCM; returns one trace row per completed frame.
    pub fn process_s16_block(&mut self, samples: &[i16], channels: u16, channel: u16, out: &mut Vec<FrameTrace>) -> Result<(), DetectWavError> {
        let mono: Vec<f32> = samples
            .chunks_exact(usize::from(channels))
            .map(|frame| frame[usize::from(channel)] as f32 / 32768.0)
            .collect();
        let detection_rate = self.resampler_output_rate();
        let resampled = self.resampler.process(&mono).to_vec();
        for sample in resampled {
            let wide = self.dc.process(sample);
            let in_band = self.band.process(wide);
            if let Some(features) = self.framer.push(wide, in_band) {
                let index = self.frame_index;
                self.frame_index += 1;
                // Frame k starts at detection sample `frame_samples * k`, which
                // maps back to capture frames by the exact ratio.
                let frame_samples = self.framer.frame_samples() as u64;
                let input_offset = (index * frame_samples).saturating_mul(u64::from(self.capture_rate_hz))
                    / u64::from(detection_rate);
                out.push(FrameTrace {
                    index,
                    level_dbfs: features.level_dbfs,
                    band_rms_dbfs: features.band_rms_dbfs,
                    band_ratio: features.band_ratio,
                    peak: features.peak,
                    at_utc: self.started_at
                        + chrono::Duration::nanoseconds(
                            (input_offset.saturating_mul(1_000_000_000) / u64::from(self.capture_rate_hz))
                                .min(i64::MAX as u64) as i64,
                        ),
                });
            }
        }
        Ok(())
    }

    fn resampler_output_rate(&self) -> u32 {
        // The resampler is constructed for the fixed detection rate; `Pass` keeps
        // the capture rate, which is what the pipeline frames at too.
        match &self.resampler {
            Resampler::Pass => self.capture_rate_hz,
            Resampler::Rational(_) => crate::config::DETECTION_RATE_HZ,
        }
    }
}

/// Reads a WAV and runs the shared detection pipeline over it.
///
/// `started_at` anchors `wall(o)`; offline callers pass the file's modification
/// time or an explicit value, never a fabricated "now".
pub fn detect_file(
    path: &Path,
    config: DetectorConfig,
    mono_channel_index: u16,
    started_at: DateTime<Utc>,
) -> Result<DetectionResult, DetectWavError> {
    let mut reader = hound::WavReader::open(path).map_err(|source| DetectWavError::Read { path: path.to_path_buf(), source })?;
    let spec = reader.spec();
    if spec.sample_rate == 0 {
        return Err(DetectWavError::Unsupported {
            path: path.to_path_buf(),
            message: "sample rate is zero".into(),
        });
    }
    if !crate::config::SUPPORTED_CAPTURE_RATES.contains(&spec.sample_rate) {
        return Err(DetectWavError::Unsupported {
            path: path.to_path_buf(),
            message: format!(
                "{} Hz is outside TECH_SPEC §5.1's supported capture rates {:?}",
                spec.sample_rate,
                crate::config::SUPPORTED_CAPTURE_RATES
            ),
        });
    }
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(DetectWavError::Unsupported {
            path: path.to_path_buf(),
            message: format!("only 16-bit integer PCM is supported, found {:?}", spec.sample_format),
        });
    }
    if !(1..=2).contains(&spec.channels) {
        return Err(DetectWavError::Unsupported {
            path: path.to_path_buf(),
            message: format!("only 1-2 channels are supported (WAVE_FORMAT_PCM), found {}", spec.channels),
        });
    }
    if mono_channel_index >= spec.channels {
        return Err(DetectWavError::Unsupported {
            path: path.to_path_buf(),
            message: format!(
                "mono channel index {} is out of range for {} channels",
                mono_channel_index, spec.channels
            ),
        });
    }

    let channels = spec.channels;
    let capture_rate = spec.sample_rate;
    let segment_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("segment").to_string();
    let context = SegmentContext {
        segment_id,
        capture_rate_hz: capture_rate,
        started_at_utc: started_at,
        offset_frames_at_anchor: 0,
        gaps: Vec::new(),
        time_quality: "synced".into(),
        boot_id: "offline".into(),
    };

    let mut pipeline = DetectionPipeline::new(config.clone(), capture_rate, channels, mono_channel_index)?;
    let mut events = pipeline.begin_segment(context, 0, false);
    let started = std::time::Instant::now();
    let mut frame_count: u64 = 0;
    let mut interleaved: Vec<i16> = Vec::with_capacity(BLOCK_FRAMES * channels as usize);
    for sample in reader.samples::<i16>() {
        let value = sample.map_err(|source| DetectWavError::Read { path: path.to_path_buf(), source })?;
        interleaved.push(value);
        if interleaved.len() == BLOCK_FRAMES * channels as usize {
            frame_count += (interleaved.len() / channels as usize) as u64;
            pipeline.process_s16_block(&interleaved, 0, &mut events)?;
            interleaved.clear();
        }
    }
    if !interleaved.is_empty() {
        frame_count += (interleaved.len() / channels as usize) as u64;
        pipeline.process_s16_block(&interleaved, 0, &mut events)?;
    }
    events.extend(pipeline.close(EndReason::SegmentEnd, false));
    let processing_seconds = started.elapsed().as_secs_f64();
    let duration_seconds = frame_count as f64 / capture_rate as f64;

    Ok(DetectionResult {
        file: path.display().to_string(),
        capture_rate_hz: capture_rate,
        channels,
        mono_channel_index,
        frame_count,
        detector_version: crate::detector::DETECTOR_VERSION.to_string(),
        events,
        duration_seconds,
        processing_seconds,
        realtime_factor: if processing_seconds > 0.0 {
            duration_seconds / processing_seconds
        } else {
            f64::INFINITY
        },
    })
}

/// Recursively collects `*.wav` files under `root`, sorted for stable output.
pub fn collect_wav_files(root: &Path) -> Result<Vec<PathBuf>, DetectWavError> {
    let mut out = Vec::new();
    if root.is_file() {
        out.push(root.to_path_buf());
        return Ok(out);
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?;
        for entry in entries {
            let entry = entry.map_err(Error::from)?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| Error::io(&path, e))?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// File modification time, or a zero epoch when unavailable. Offline tooling has
/// no wall-clock guarantee to offer, so the anchor is metadata, not invention.
pub fn file_anchor(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| {
            let unix = t
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Utc.timestamp_opt(unix.as_secs() as i64, unix.subsec_nanos())
                .single()
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        })
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

/// CSV header for `--csv` output.
pub const CSV_HEADER: &str = "file,start_offset_frames,end_offset_frames,started_at_utc,ended_at_utc,duration_ms,rule_score,peak_dbfs,mean_level_dbfs,mean_band_ratio,noise_floor_dbfs,end_reason,continued";

/// Renders one result as CSV rows (no header).
pub fn result_to_csv(result: &DetectionResult) -> String {
    let mut rows = Vec::with_capacity(result.events.len());
    for event in &result.events {
        rows.push(format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            result.file,
            event.start_offset_frames,
            event.end_offset_frames,
            event.started_at_utc.to_rfc3339(),
            event.ended_at_utc.to_rfc3339(),
            event.duration_ms,
            event.rule_score.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            event.peak_dbfs.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            event.mean_level_dbfs.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            event.mean_band_ratio.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            event.noise_floor_dbfs.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            event.end_reason.as_str(),
            u8::from(event.continued),
        ));
    }
    rows.join("\n")
}
