//! CPAL device discovery and capture callback (TECH_SPEC §4.1, tasks T3.1/T3.2).
//!
//! The callback performs bounded copies into preallocated SPSC rings only. It
//! does not run DSP, storage, logging, HTTP, or blocking work. The Pi hardware
//! path remains subject to the T0.2/T8.1 spike; this module compiles on the
//! developer host so the rest of the application can be tested without a Pi.

use crate::bounded_queue::SpscRing;
use crate::config::{AudioConfig, SampleFormat, SUPPORTED_CAPTURE_RATES};
use crate::metrics::Metrics;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, Stream, SupportedStreamConfig};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const META_PADDING: usize = 16;

/// Callback block metadata paired with raw interleaved S16 samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureMeta {
    pub frames: u32,
    pub channels: u16,
    pub rate_hz: u32,
    pub capture_seq: u64,
    pub mono_start_ns: u64,
    pub stream_generation: u32,
}

/// Two-ring capture buffer: one ring for sample frames and one for callback
/// boundaries/timestamps. Both are allocated before the stream starts.
pub struct CaptureRing {
    samples: SpscRing<i16>,
    blocks: SpscRing<CaptureMeta>,
    max_block_frames: usize,
    overflows: AtomicU64,
    max_depth_frames: AtomicU64,
}

impl CaptureRing {
    pub fn new(rate_hz: u32, channels: u16, capture_ring_ms: u32, period_ms: u32, buffer_ms: u32) -> Self {
        let duration_frames = (u64::from(rate_hz) * u64::from(capture_ring_ms) / 1000) as usize;
        let requested_buffer_frames = (u64::from(rate_hz) * u64::from(buffer_ms) / 1000) as usize;
        let max_block_frames = requested_buffer_frames.max(1);
        let sample_capacity = duration_frames.max(max_block_frames * 2).next_power_of_two();
        let blocks_needed = (capture_ring_ms / period_ms.max(1)) as usize + META_PADDING;
        CaptureRing {
            samples: SpscRing::with_capacity(sample_capacity.saturating_mul(channels.max(1) as usize)),
            blocks: SpscRing::with_capacity(blocks_needed),
            max_block_frames,
            overflows: AtomicU64::new(0),
            max_depth_frames: AtomicU64::new(0),
        }
    }

    /// Called by the single CPAL producer. Returns false for a whole block when
    /// either ring cannot accept it, preserving the metadata/sample boundary.
    pub fn callback_push(&self, samples: &[i16], meta: CaptureMeta) -> bool {
        if meta.frames as usize > self.max_block_frames
            || samples.len() > self.samples.free_space()
            || self.blocks.free_space() == 0
        {
            self.overflows.fetch_add(u64::from(meta.frames), Ordering::Relaxed);
            return false;
        }
        for sample in samples {
            if !self.samples.try_push_quiet(*sample) {
                // The preflight plus single producer guarantees this cannot occur
                // unless the ring's SPSC contract was violated.
                self.overflows.fetch_add(u64::from(meta.frames), Ordering::Relaxed);
                return false;
            }
        }
        if !self.blocks.try_push_quiet(meta) {
            // Capacity was checked above; don't publish an unpaired block.
            self.overflows.fetch_add(u64::from(meta.frames), Ordering::Relaxed);
            return false;
        }
        let depth = self.samples.len() / usize::from(meta.channels.max(1));
        self.max_depth_frames.fetch_max(depth as u64, Ordering::Relaxed);
        true
    }

    /// Dispatcher side: obtains metadata and copies exactly that block into the
    /// caller's reusable buffer.
    pub fn pop_into(&self, out: &mut Vec<i16>) -> Option<CaptureMeta> {
        let meta = self.blocks.try_pop()?;
        let sample_count = meta.frames as usize * meta.channels as usize;
        out.clear();
        out.reserve(sample_count.saturating_sub(out.capacity()));
        for _ in 0..sample_count {
            let Some(sample) = self.samples.try_pop() else {
                // Internal invariant failure: samples and metadata were published
                // together. Count a loss and return no partially valid block.
                self.overflows.fetch_add(u64::from(meta.frames), Ordering::Relaxed);
                out.clear();
                return None;
            };
            out.push(sample);
        }
        Some(meta)
    }

    pub fn overflow_frames(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    pub fn queued_frames(&self, channels: u16) -> usize {
        self.samples.len() / usize::from(channels.max(1))
    }

    pub fn high_water_frames(&self) -> u64 {
        self.max_depth_frames.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedFormat {
    pub device_name: String,
    pub rate_hz: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

#[derive(Debug)]
pub struct CaptureSelection {
    pub device: Device,
    pub supported: SupportedStreamConfig,
    pub info: NegotiatedFormat,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no CPAL input device found")]
    NoDevice,
    #[error("multiple input devices found; specify audio.alsa_device (found {0:?})")]
    MultipleDevices(Vec<String>),
    #[error("requested input device {0:?} was not found")]
    DeviceNotFound(String),
    #[error("no supported S16 input configuration for device {device}: {details}")]
    NoSupportedConfig { device: String, details: String },
    #[error("CPAL error: {0}")]
    Cpal(String),
}

/// Discovers eligible capture devices and negotiates S16_LE in documented order:
/// configured rate, then 48 kHz, then 44.1 kHz; channel count is configured, then
/// mono if the device exposes it. Other formats/rates fail explicitly.
pub fn select_device(host: &Host, config: &AudioConfig) -> Result<CaptureSelection, CaptureError> {
    let mut devices = host.input_devices().map_err(|e| CaptureError::Cpal(e.to_string()))?;
    let all: Vec<Device> = devices.by_ref().collect();
    if all.is_empty() {
        return Err(CaptureError::NoDevice);
    }
    let names: Vec<(String, Device)> = all
        .into_iter()
        .map(|d| (d.to_string(), d))
        .collect();
    let candidates: Vec<(String, Device)> = if config.alsa_device.trim().is_empty() {
        if names.len() != 1 {
            return Err(CaptureError::MultipleDevices(names.iter().map(|(name, _)| name.clone()).collect()));
        }
        names
    } else {
        names
            .into_iter()
            .filter(|(name, _)| name == &config.alsa_device)
            .collect()
    };
    let Some((device_name, device)) = candidates.into_iter().next() else {
        return Err(CaptureError::DeviceNotFound(config.alsa_device.clone()));
    };

    let ranges = device
        .supported_input_configs()
        .map_err(|e| CaptureError::Cpal(e.to_string()))?
        .collect::<Vec<_>>();
    let attempts = [config.preferred_rate_hz, 48_000, 44_100];
    for desired_rate in attempts {
        if !SUPPORTED_CAPTURE_RATES.contains(&desired_rate) {
            continue;
        }
        for channels in [config.channels, 1].into_iter().filter(|c| *c > 0).collect::<Vec<_>>() {
            for range in &ranges {
                if range.channels() != channels || range.sample_format() != cpal::SampleFormat::I16 {
                    continue;
                }
                if desired_rate < range.min_sample_rate() || desired_rate > range.max_sample_rate() {
                    continue;
                }
                let supported = (*range).with_sample_rate(desired_rate);
                return Ok(CaptureSelection {
                    info: NegotiatedFormat {
                        device_name: device_name.clone(),
                        rate_hz: desired_rate,
                        channels,
                        format: SampleFormat::S16Le,
                    },
                    device,
                    supported,
                });
            }
        }
    }
    let details = ranges
        .iter()
        .map(|r| format!("{}ch {:?} {}-{} Hz", r.channels(), r.sample_format(), r.min_sample_rate(), r.max_sample_rate()))
        .collect::<Vec<_>>()
        .join(", ");
    Err(CaptureError::NoSupportedConfig { device: device_name, details })
}

/// Opens the selected CPAL stream and starts it. The callback copies only to the
/// bounded capture rings. Stream errors set an atomic flag and are later turned
/// into ordered gap/control messages by the dispatcher.
pub fn start_stream(
    selection: CaptureSelection,
    ring: Arc<CaptureRing>,
    metrics: Arc<Metrics>,
    shutting_down: Arc<AtomicBool>,
    stream_generation: u32,
) -> Result<Stream, CaptureError> {
    let channels = selection.info.channels;
    let rate_hz = selection.info.rate_hz;
    let seq = Arc::new(AtomicU64::new(0));
    let stream_origin = Instant::now();
    let data_ring = Arc::clone(&ring);
    let data_metrics = Arc::clone(&metrics);
    let data_seq = Arc::clone(&seq);
    let error_metrics = Arc::clone(&metrics);
    let error_shutdown = Arc::clone(&shutting_down);
    let stream_config = selection.supported.config();
    let err_fn = move |err: cpal::Error| {
        error_metrics.xrun_count.fetch_add(1, Ordering::Relaxed);
        error_metrics.set_last_error(format!("CPAL stream error: {err}"));
        if !error_shutdown.load(Ordering::Relaxed) {
            error_metrics.degraded.store(true, Ordering::Relaxed);
        }
    };
    let stream = selection
        .device
        .build_input_stream::<i16, _, _>(
            stream_config,
            move |data: &[i16], _info: &cpal::InputCallbackInfo| {
                let now_ns = stream_origin.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                let frame_count = data.len() / channels as usize;
                let start_seq = data_seq.fetch_add(frame_count as u64, Ordering::Relaxed);
                let meta = CaptureMeta {
                    frames: frame_count.min(u32::MAX as usize) as u32,
                    channels,
                    rate_hz,
                    capture_seq: start_seq,
                    mono_start_ns: now_ns,
                    stream_generation,
                };
                if !data_ring.callback_push(data, meta) {
                    data_metrics.capture_ring_overflow_frames.fetch_add(frame_count as u64, Ordering::Relaxed);
                }
                data_metrics.capture_ring_high_water_frames.fetch_max(data_ring.high_water_frames(), Ordering::Relaxed);
            },
            err_fn,
            None,
        )
        .map_err(|e| CaptureError::Cpal(e.to_string()))?;
    stream.play().map_err(|e| CaptureError::Cpal(e.to_string()))?;
    Ok(stream)
}

/// Backoff schedule for stream rebuild attempts: 1,2,4,8,16,30,30... seconds.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    next_seconds: u64,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        ReconnectBackoff { next_seconds: 1 }
    }
}

impl ReconnectBackoff {
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next_seconds.min(30);
        self.next_seconds = (self.next_seconds * 2).min(30);
        Duration::from_secs(delay)
    }

    pub fn reset(&mut self) {
        self.next_seconds = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_ring_keeps_block_boundaries_and_exact_samples() {
        let ring = CaptureRing::new(16_000, 1, 500, 10, 200);
        let data = [1i16, -2, 3, -4];
        let meta = CaptureMeta { frames: 4, channels: 1, rate_hz: 16_000, capture_seq: 0, mono_start_ns: 123, stream_generation: 1 };
        assert!(ring.callback_push(&data, meta));
        let mut out = Vec::new();
        assert_eq!(ring.pop_into(&mut out), Some(meta));
        assert_eq!(out, data);
        assert_eq!(ring.pop_into(&mut out), None);
    }

    #[test]
    fn ring_overflow_drops_entire_block_and_counts_frames() {
        let ring = CaptureRing::new(1_000, 1, 10, 10, 10);
        let samples = vec![7i16; 20]; // larger than allocated max block
        let meta = CaptureMeta { frames: 20, channels: 1, rate_hz: 1_000, capture_seq: 0, mono_start_ns: 0, stream_generation: 0 };
        assert!(!ring.callback_push(&samples, meta));
        assert_eq!(ring.overflow_frames(), 20);
        assert!(ring.pop_into(&mut Vec::new()).is_none());
    }

    #[test]
    fn reconnect_backoff_caps_at_thirty_seconds_and_resets() {
        let mut b = ReconnectBackoff::default();
        let delays: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }
}
