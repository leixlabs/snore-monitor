//! `detect-wav`: offline threshold calibration for the live detector (tasks.md T2.7).
//!
//! A PCM WAV file is replayed through the *same* [`DetectionPipeline`] the capture
//! path uses — the same resampler, DC blocker, band-pass, noise floors and
//! candidate state machine — so a recording can be re-run with overridden
//! thresholds and the resulting events compared against what the service would
//! store. Nothing here reimplements detection, and the tool needs neither a
//! microphone nor a database nor a running service.
//!
//! Accepted input is 16/32/44.1/48/96 kHz, 1-2 channel `WAVE_FORMAT_PCM` S16_LE:
//! both arbitrary recordings and the segments written by the recorder (whose
//! header is the canonical 44-byte PCM form). Anything else fails with an
//! explicit message rather than being converted silently.
//!
//! Events are written as JSON (default) or CSV. `--frames` and
//! `--frames-per-second` additionally dump the per-frame decision inputs on
//! stderr, so thresholds can be chosen from the actual signal.

use clap::{Parser, ValueEnum};
use serde::Serialize;
use snore_monitor::config::{Config, ConfigLoadError, DetectorConfig, SUPPORTED_CAPTURE_RATES};
use snore_monitor::detector::{
    DETECTOR_VERSION, DetectionPipeline, Discontinuity, EndReason, EventDraft, SegmentContext,
    State,
};
use snore_monitor::dsp::{BandPass, DcBlocker, FrameBuilder, FrameFeatures, NoiseFloor};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Stored frames read per I/O call. Large enough that loop overhead disappears
/// into the DSP, small enough that memory stays flat on long recordings.
const READ_CHUNK_FRAMES: usize = 4_800;

#[derive(Debug, Parser)]
#[command(
    name = "detect-wav",
    version,
    about = "Run the live snore-detection pipeline over a PCM WAV file and emit its events.",
    long_about = "Replays a 16-96 kHz, 1-2 channel S16_LE WAV through the exact detection pipeline the \
service uses, so detection parameters can be calibrated offline against real recordings. Needs no \
microphone, no database and no running service; with no arguments beyond the file it uses the \
built-in detector defaults.",
    after_help = "EXAMPLES:\n  detect-wav night.wav\n  detect-wav night.wav --format csv -o events.csv\n  detect-wav night.wav --band-ratio-min 0.45 --noise-margin-db 10 --frames\n  detect-wav night.wav --channels 2 --mono-channel 1 --band-high-hz 1200\n  detect-wav night.wav --config /etc/snore-monitor/config.toml\n  detect-wav night.wav --start-time-utc 2025-01-02T23:10:00Z --mode segments"
)]
struct Cli {
    /// Input WAV file (16/32/44.1/48/96 kHz, 1-2 channels, WAVE_FORMAT_PCM S16_LE).
    #[arg(value_name = "WAV")]
    wav: PathBuf,

    /// Take detection defaults from a service configuration file; explicit flags still win.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Event output encoding.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    format: OutputFormat,

    /// Write events to this file instead of stdout.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Dump one CSV row per 10 ms frame on stderr. Bare `--frames` uses the default level.
    #[arg(long, value_name = "DBFS", num_args = 0..=1, default_missing_value = "-45.0")]
    frames: Option<f32>,

    /// Dump one CSV row per second on stderr. Bare `--frames-per-second` uses the default level.
    #[arg(long, value_name = "DBFS", num_args = 0..=1, default_missing_value = "-45.0")]
    frames_per_second: Option<f32>,

    #[command(flatten)]
    overrides: DetectionOverrides,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// One JSON document: metadata plus the event array.
    Json,
    /// One header row and one row per event.
    Csv,
}

/// Every `[detector]` key TECH_SPEC §5.3 exposes for calibration. Unset values
/// come from the configuration file or the built-in default; nothing is derived
/// or rescaled from the live value.
#[derive(Debug, Default, clap::Args)]
struct DetectionOverrides {
    /// Override detector.frame_ms (10 ms at 16 kHz).
    #[arg(long, value_name = "MS")]
    frame_ms: Option<u32>,
    /// Override detector.band_low_hz.
    #[arg(long, value_name = "HZ")]
    band_low_hz: Option<f32>,
    /// Override detector.band_high_hz.
    #[arg(long, value_name = "HZ")]
    band_high_hz: Option<f32>,
    /// Override detector.absolute_floor_dbfs.
    #[arg(long, value_name = "DBFS")]
    absolute_floor_dbfs: Option<f32>,
    /// Override detector.noise_margin_db.
    #[arg(long, value_name = "DB")]
    noise_margin_db: Option<f32>,
    /// Override detector.band_margin_db.
    #[arg(long, value_name = "DB")]
    band_margin_db: Option<f32>,
    /// Override detector.band_ratio_min.
    #[arg(long, value_name = "RATIO")]
    band_ratio_min: Option<f32>,
    /// Override detector.noise_floor_window_seconds.
    #[arg(long, value_name = "SECONDS")]
    noise_floor_window_seconds: Option<u32>,
    /// Override detector.noise_floor_percentile.
    #[arg(long, value_name = "PERCENT")]
    noise_floor_percentile: Option<u32>,
    /// Override detector.noise_floor_min_seconds.
    #[arg(long, value_name = "SECONDS")]
    noise_floor_min_seconds: Option<u32>,
    /// Override detector.noise_floor_reset_gap_seconds.
    #[arg(long, value_name = "SECONDS")]
    noise_floor_reset_gap_seconds: Option<u64>,
    /// Override detector.noise_freeze_max_seconds.
    #[arg(long, value_name = "SECONDS")]
    noise_freeze_max_seconds: Option<u32>,
    /// Override detector.filter_warmup_ms.
    #[arg(long, value_name = "MS")]
    filter_warmup_ms: Option<u32>,
    /// Override detector.attack_window_frames.
    #[arg(long, value_name = "FRAMES")]
    attack_window_frames: Option<u32>,
    /// Override detector.attack_min_hits.
    #[arg(long, value_name = "FRAMES")]
    attack_min_hits: Option<u32>,
    /// Override detector.hangover_frames.
    #[arg(long, value_name = "FRAMES")]
    hangover_frames: Option<u32>,
    /// Override detector.min_event_ms.
    #[arg(long, value_name = "MS")]
    min_event_ms: Option<u32>,
    /// Override detector.merge_gap_ms.
    #[arg(long, value_name = "MS")]
    merge_gap_ms: Option<u32>,
    /// Override detector.max_event_seconds.
    #[arg(long, value_name = "SECONDS")]
    max_event_seconds: Option<u32>,

    /// Restart detection every --chunk-seconds to reproduce a service behaviour a single
    /// continuous file cannot show: `segments` mimics the hourly rotation (which keeps DSP
    /// state), `chunks` mimics a gap (which resets it, §5.4).
    #[arg(long, value_enum, value_name = "MODE")]
    mode: Option<RunMode>,

    /// Expected input channel count. The WAV header is authoritative; this only checks it.
    #[arg(long, value_name = "1|2")]
    channels: Option<u16>,

    /// Index of the channel the detector listens to. Defaults to audio.mono_channel_index
    /// from --config, else 0.
    #[arg(long, value_name = "INDEX")]
    mono_channel: Option<u16>,

    /// Boundary spacing for --mode, in detection-seconds (3600 matches
    /// audio.segment_duration_seconds).
    #[arg(long, value_name = "SECONDS", default_value_t = 3_600)]
    chunk_seconds: u64,

    /// `end_reason` recorded for events closed by a --mode boundary.
    #[arg(long, value_enum, default_value_t = BoundaryEndReason::SegmentEnd)]
    boundary_end_reason: BoundaryEndReason,

    /// UTC origin written into the event time columns (RFC 3339).
    #[arg(long, value_name = "RFC3339", default_value = "1970-01-01T00:00:00Z")]
    start_time_utc: String,

    /// `segment_id` written into every event.
    #[arg(long, value_name = "ID", default_value = "detect-wav")]
    segment_id: String,

    /// Include per-event diagnostics (`segment_id`, `ended_at_utc`, `detector_version`).
    #[arg(long, action = clap::ArgAction::SetTrue)]
    verbose: bool,
}

impl DetectionOverrides {
    /// Copies every explicitly given flag onto `config` and returns the ones that
    /// differ, for the output metadata.
    fn apply(&self, config: &mut DetectorConfig) -> Vec<String> {
        let mut applied = Vec::new();
        macro_rules! apply {
            ($field:ident, $target:ident) => {
                if let Some(value) = self.$field {
                    config.$target = value;
                    applied.push(format!("{}={}", stringify!($target), value));
                }
            };
        }
        apply!(frame_ms, frame_ms);
        apply!(band_low_hz, band_low_hz);
        apply!(band_high_hz, band_high_hz);
        apply!(absolute_floor_dbfs, absolute_floor_dbfs);
        apply!(noise_margin_db, noise_margin_db);
        apply!(band_margin_db, band_margin_db);
        apply!(band_ratio_min, band_ratio_min);
        apply!(noise_floor_window_seconds, noise_floor_window_seconds);
        apply!(noise_floor_percentile, noise_floor_percentile);
        apply!(noise_floor_min_seconds, noise_floor_min_seconds);
        apply!(noise_floor_reset_gap_seconds, noise_floor_reset_gap_seconds);
        apply!(noise_freeze_max_seconds, noise_freeze_max_seconds);
        apply!(filter_warmup_ms, filter_warmup_ms);
        apply!(attack_window_frames, attack_window_frames);
        apply!(attack_min_hits, attack_min_hits);
        apply!(hangover_frames, hangover_frames);
        apply!(min_event_ms, min_event_ms);
        apply!(merge_gap_ms, merge_gap_ms);
        apply!(max_event_seconds, max_event_seconds);
        applied
    }
}

/// How the file is cut before it reaches the pipeline. Both cut modes feed the
/// same samples in the same order; only the announced boundary differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RunMode {
    /// One continuous stream: the file is a single segment.
    Continuous,
    /// Cut every `--chunk-seconds` as the service rotates segments, which closes
    /// the open candidate but keeps DSP state (§5.4).
    Segments,
    /// Cut every `--chunk-seconds` as a gap would: close the open candidate and
    /// reset resampler, filters and framing (§5.4).
    Chunks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
enum BoundaryEndReason {
    #[default]
    SegmentEnd,
    Shutdown,
}

impl BoundaryEndReason {
    fn to_end_reason(self) -> EndReason {
        match self {
            BoundaryEndReason::SegmentEnd => EndReason::SegmentEnd,
            BoundaryEndReason::Shutdown => EndReason::Shutdown,
        }
    }
}

#[derive(Debug)]
enum ToolError {
    Message(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::Message(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for ToolError {}

type ToolResult<T> = Result<T, ToolError>;

fn fail<T>(message: impl Into<String>) -> ToolResult<T> {
    Err(ToolError::Message(message.into()))
}

fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(ToolError::Message(message)) => {
            eprintln!("detect-wav: error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> ToolResult<()> {
    // Detection parameters: built-in defaults, then the service configuration
    // file, then the explicit command-line overrides.
    let mut detector = DetectorConfig::default();
    let mut configured_channel = 0u16;
    if let Some(path) = &cli.config {
        let config = Config::load(path).map_err(|error| {
            ToolError::Message(format!("cannot use {}: {error}", path.display()))
        })?;
        configured_channel = config.audio.mono_channel_index;
        detector = config.detector.clone();
    }
    let overridden = cli.overrides.apply(&mut detector);
    validate_detector(&detector).map_err(ToolError::Message)?;
    let mut input = WavReader::open(&cli.wav, cli.overrides.channels)?;
    let selected_channel = cli.overrides.mono_channel.unwrap_or(configured_channel);
    if selected_channel >= input.channels {
        return fail(format!(
            "channel {selected_channel} does not exist in {} ({} channel(s); use --mono-channel to select another)",
            cli.wav.display(),
            input.channels
        ));
    }

    let start_utc = chrono::DateTime::parse_from_rfc3339(cli.overrides.start_time_utc.trim())
        .map_err(|error| {
            ToolError::Message(format!(
                "--start-time-utc {:?} is not an RFC 3339 timestamp: {error}",
                cli.overrides.start_time_utc
            ))
        })?
        .to_utc();

    let mut session = Session::new(SessionParams {
        detector: detector.clone(),
        rate_hz: input.rate_hz,
        channels: input.channels,
        selected_channel,
        mode: cli.overrides.mode.unwrap_or(RunMode::Continuous),
        chunk_seconds: cli.overrides.chunk_seconds,
        boundary_end_reason: cli.overrides.boundary_end_reason.to_end_reason(),
        start_utc,
        segment_id: cli.overrides.segment_id.clone(),
    })?
    .with_streams(
        cli.frames
            .map(FeatureStream::frames)
            .unwrap_or_else(FeatureStream::off),
        cli.frames_per_second
            .map(FeatureStream::per_second)
            .unwrap_or_else(FeatureStream::off),
    );

    let mut buffer = vec![0i16; READ_CHUNK_FRAMES * usize::from(input.channels)];
    loop {
        let read = input.read_block(&mut buffer)?;
        if read == 0 {
            break;
        }
        let block = buffer[..read].to_vec();
        session.push_interleaved(&block)?;
    }

    let summary = session.finish(input.truncated())?;
    write_events(cli, &summary)?;
    eprintln!(
        "detect-wav: {} event(s) from {:.3} s of {} Hz / {} channel audio (channel {}, {} short candidate(s) discarded)",
        summary.event_count,
        summary.seconds,
        summary.rate_hz,
        summary.channels,
        summary.selected_channel,
        summary.events_discarded_short
    );
    if !overridden.is_empty() {
        eprintln!(
            "detect-wav: overridden parameters: {}",
            overridden.join(" ")
        );
    }
    Ok(())
}

/// Runs the service's own `[detector]` validation instead of a second copy that
/// could drift: the effective values are serialized into the smallest TOML
/// document `Config::from_toml_str` accepts and parsed back through it.
fn validate_detector(detector: &DetectorConfig) -> Result<(), String> {
    let document = format!(
        "[detector]\ndetection_rate_hz = {}\nframe_ms = {}\nband_low_hz = {}\nband_high_hz = {}\nabsolute_floor_dbfs = {}\nnoise_margin_db = {}\nband_margin_db = {}\nband_ratio_min = {}\nnoise_floor_window_seconds = {}\nnoise_floor_percentile = {}\nnoise_floor_min_seconds = {}\nnoise_floor_reset_gap_seconds = {}\nnoise_freeze_max_seconds = {}\nfilter_warmup_ms = {}\nattack_window_frames = {}\nattack_min_hits = {}\nhangover_frames = {}\nmin_event_ms = {}\nmerge_gap_ms = {}\nmax_event_seconds = {}\n[storage]\nrecording_dir = \"/tmp/snore-monitor-detect-wav\"\ndatabase_path = \"/tmp/snore-monitor-detect-wav/app.db\"\n",
        detector.detection_rate_hz,
        detector.frame_ms,
        detector.band_low_hz,
        detector.band_high_hz,
        detector.absolute_floor_dbfs,
        detector.noise_margin_db,
        detector.band_margin_db,
        detector.band_ratio_min,
        detector.noise_floor_window_seconds,
        detector.noise_floor_percentile,
        detector.noise_floor_min_seconds,
        detector.noise_floor_reset_gap_seconds,
        detector.noise_freeze_max_seconds,
        detector.filter_warmup_ms,
        detector.attack_window_frames,
        detector.attack_min_hits,
        detector.hangover_frames,
        detector.min_event_ms,
        detector.merge_gap_ms,
        detector.max_event_seconds,
    );
    Config::from_toml_str(&document)
        .map(|_| ())
        .map_err(|error| match error {
            ConfigLoadError::Invalid(invalid) => format!(
                "detection parameters are not usable:\n  {}\n(startup runs the same validation, so this would fail there too)",
                invalid.0.join("\n  ")
            ),
            other => format!("detection parameters are not usable: {other}"),
        })
}

/// A WAV whose header has already been checked against TECH_SPEC §5.1, so
/// unsupported input fails before any sample is read.
struct WavReader {
    samples: hound::WavIntoSamples<std::io::BufReader<File>, i16>,
    rate_hz: u32,
    channels: u16,
    /// Samples the data chunk declares, as frames times channels.
    declared_samples: u64,
    samples_read: u64,
}

impl WavReader {
    fn open(path: &Path, declared_channels: Option<u16>) -> ToolResult<Self> {
        let reader = hound::WavReader::open(path).map_err(|error| {
            ToolError::Message(format!("cannot read {} as WAV: {error}", path.display()))
        })?;
        let spec = reader.spec();
        Self::check_declared_channels(spec.channels, declared_channels)?;
        Self::check_supported(spec.sample_rate, spec.channels, spec.bits_per_sample)?;
        let declared_samples = reader.duration() as u64 * u64::from(spec.channels);
        let samples = reader.into_samples::<i16>();
        Ok(WavReader {
            samples,
            rate_hz: spec.sample_rate,
            channels: spec.channels,
            declared_samples,
            samples_read: 0,
        })
    }

    /// The WAV header is the source of truth; `--channels` only cross-checks it.
    fn check_declared_channels(header: u16, declared: Option<u16>) -> ToolResult<()> {
        match declared {
            Some(value) if value != header => fail(format!(
                "--channels {value} does not match the WAV header, which declares {header} channel(s)"
            )),
            _ => Ok(()),
        }
    }

    fn check_supported(rate_hz: u32, channels: u16, bits_per_sample: u16) -> ToolResult<()> {
        if bits_per_sample != 16 {
            return fail(format!(
                "{bits_per_sample}-bit samples are not supported; TECH_SPEC §5.1 stores 16-bit PCM only"
            ));
        }
        if !(1..=2).contains(&channels) {
            return fail(format!(
                "{channels} channels are not supported; TECH_SPEC §5.1 allows 1-2"
            ));
        }
        if !SUPPORTED_CAPTURE_RATES.contains(&rate_hz) {
            return fail(format!(
                "{rate_hz} Hz is not a supported capture rate; TECH_SPEC §5.1 allows {SUPPORTED_CAPTURE_RATES:?}"
            ));
        }
        Ok(())
    }

    /// Fills `buffer` with interleaved S16 samples and returns how many were
    /// read. A data chunk cut short (a `kill -9`, or a storage device pulled
    /// mid-write) is reported to the caller as `truncated`; the tail is never
    /// invented or zero-filled, and the samples that are present are analysed.
    fn read_block(&mut self, buffer: &mut [i16]) -> ToolResult<usize> {
        let mut read = 0usize;
        for slot in buffer.iter_mut() {
            match self.samples.next() {
                Some(Ok(sample)) => {
                    *slot = sample;
                    read += 1;
                }
                Some(Err(error)) if is_short_read(&error) => {
                    self.samples_read += read as u64;
                    return Ok(read);
                }
                Some(Err(error)) => {
                    self.samples_read += read as u64;
                    return fail(format!("cannot read sample {}: {error}", self.samples_read));
                }
                None => break,
            }
        }
        self.samples_read += read as u64;
        Ok(read)
    }

    /// Whether the data chunk ended before its declared length.
    fn truncated(&self) -> bool {
        self.samples_read < self.declared_samples
    }
}

/// `hound` reports a data chunk that ends early as an `io::Error` with the
/// generic `Other` kind and the message "Failed to read enough bytes.", because
/// its internal `read_into` helper cannot tell a truncated tail from a broken
/// stream. That truncation is exactly the case a calibration run must tolerate,
/// so it is recognised here; genuine I/O failures keep their own `ErrorKind` and
/// are still reported as errors.
fn is_short_read(error: &hound::Error) -> bool {
    match error {
        hound::Error::IoError(io) => {
            io.kind() == std::io::ErrorKind::UnexpectedEof
                || (io.kind() == std::io::ErrorKind::Other
                    && io.to_string().contains("Failed to read enough bytes"))
        }
        _ => false,
    }
}

struct SessionParams {
    detector: DetectorConfig,
    rate_hz: u32,
    channels: u16,
    selected_channel: u16,
    mode: RunMode,
    chunk_seconds: u64,
    boundary_end_reason: EndReason,
    start_utc: chrono::DateTime<chrono::Utc>,
    segment_id: String,
}

/// Replays one file through [`DetectionPipeline`] and reconstructs its events.
///
/// Detection itself is entirely the pipeline's: samples go to
/// [`DetectionPipeline::process_s16_block`] in the same interleaved form the
/// capture path produces, and boundaries go to `begin_segment`/
/// `discontinuity`/`close`. The feature observers below only *mirror* the
/// per-frame signal chain so `--frames` can dump the decision inputs; they never
/// influence an event. The one thing the tool owns is the wall clock, which is a
/// single UTC origin plus the capture rate instead of a monotonic anchor.
struct Session {
    detector: DetectorConfig,
    pipeline: DetectionPipeline,
    rate_hz: u32,
    channels: u16,
    selected_channel: u16,
    mode: RunMode,
    boundary_end_reason: EndReason,
    chunk_frames: u64,
    start_utc: chrono::DateTime<chrono::Utc>,
    segment_id: String,
    dc: DcBlocker,
    band: BandPass,
    framer: FrameBuilder,
    wide_noise: NoiseFloor,
    band_noise: NoiseFloor,
    samples_into_reset: u64,
    warmup_frames_left: u64,
    input_frames: u64,
    in_chunk_frames: u64,
    boundaries: u64,
    segment_anchor: u64,
    events: Vec<EventDraft>,
    frames: FeatureStream,
    seconds: FeatureStream,
    silent_frames: u64,
    clipped_samples: u64,
}

impl Session {
    fn new(params: SessionParams) -> ToolResult<Self> {
        let frame_samples = params.detector.frame_samples();
        if frame_samples == 0 {
            return fail("detector.frame_ms and detector.detection_rate_hz yield an empty frame");
        }
        let band = BandPass::from_config(&params.detector).map_err(|error| {
            ToolError::Message(format!("cannot design the detector band-pass: {error}"))
        })?;
        let pipeline = DetectionPipeline::new(
            params.detector.clone(),
            params.rate_hz,
            params.channels,
            params.selected_channel,
        )
        .map_err(|error| {
            ToolError::Message(format!(
                "cannot build the detection pipeline for {} Hz / {} channel input: {error}",
                params.rate_hz, params.channels
            ))
        })?;
        let chunk_frames = params
            .chunk_seconds
            .saturating_mul(u64::from(params.detector.detection_rate_hz))
            .max(1);
        let warmup_frames_left = params.detector.warmup_frames();
        let mut session = Session {
            dc: DcBlocker::new(DcBlocker::DEFAULT_ALPHA),
            band,
            framer: FrameBuilder::new(frame_samples),
            wide_noise: NoiseFloor::from_config(&params.detector),
            band_noise: NoiseFloor::from_config(&params.detector),
            pipeline,
            rate_hz: params.rate_hz,
            channels: params.channels,
            selected_channel: params.selected_channel,
            mode: params.mode,
            boundary_end_reason: params.boundary_end_reason,
            chunk_frames,
            start_utc: params.start_utc,
            segment_id: params.segment_id,
            samples_into_reset: 0,
            warmup_frames_left,
            input_frames: 0,
            in_chunk_frames: 0,
            boundaries: 0,
            segment_anchor: 0,
            events: Vec::new(),
            frames: FeatureStream::off(),
            seconds: FeatureStream::off(),
            silent_frames: 0,
            clipped_samples: 0,
            detector: params.detector,
        };
        let context = session.segment_context();
        let events = session.pipeline.begin_segment(context, 0, true);
        session.events.extend(events);
        Ok(session)
    }

    fn with_streams(mut self, frames: FeatureStream, seconds: FeatureStream) -> Self {
        self.frames = frames;
        self.seconds = seconds;
        self
    }

    fn segment_context(&self) -> SegmentContext {
        SegmentContext {
            segment_id: self.segment_id.clone(),
            capture_rate_hz: self.rate_hz,
            started_at_utc: self.segment_start_utc(),
            offset_frames_at_anchor: self.segment_anchor,
            // A single file has no registered gaps: wall time is a plain scale.
            gaps: Vec::new(),
            time_quality: "synced".to_string(),
            boot_id: "detect-wav".to_string(),
        }
    }

    /// Wall time of the anchor the next segment starts at.
    fn segment_start_utc(&self) -> chrono::DateTime<chrono::Utc> {
        let ns =
            (u128::from(self.segment_anchor) * 1_000_000_000u128 / u128::from(self.rate_hz)) as i64;
        self.start_utc + chrono::Duration::nanoseconds(ns)
    }

    fn push_interleaved(&mut self, samples: &[i16]) -> ToolResult<()> {
        let channels = usize::from(self.channels);
        if !samples.len().is_multiple_of(channels) {
            return fail(format!(
                "read {} sample(s), which is not a whole number of {channels}-channel frames",
                samples.len()
            ));
        }
        for sample in samples {
            if sample.unsigned_abs() >= 32_767 {
                self.clipped_samples += 1;
            }
        }

        // The pipeline receives the block in exactly the form the dispatcher
        // hands it to the live detector, so an event boundary in the file and an
        // event boundary in the service are produced by the same code.
        let offset = self.input_frames;
        self.pipeline
            .process_s16_block(samples, offset, &mut self.events)
            .map_err(|error| {
                ToolError::Message(format!(
                    "detector rejected the audio block at frame {offset}: {error}"
                ))
            })?;
        self.input_frames += (samples.len() / channels) as u64;
        self.in_chunk_frames += (samples.len() / channels) as u64;

        if self.frames.enabled() || self.seconds.enabled() {
            self.observe_frames(samples);
        }

        if self.mode != RunMode::Continuous {
            while self.in_chunk_frames >= self.chunk_frames {
                let excess = self.in_chunk_frames - self.chunk_frames;
                self.cross_boundary()?;
                if excess == 0 {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Mirrors the detection chain over one block purely to produce the feature
    /// dump. This is a second traversal of the same samples; it exists only when
    /// a dump was requested.
    fn observe_frames(&mut self, samples: &[i16]) {
        let channels = usize::from(self.channels);
        for frame in samples.chunks_exact(channels) {
            let selected = f32::from(frame[self.selected_channel as usize]) / 32768.0;
            let wide = self.dc.process(selected);
            let in_band = self.band.process(wide);
            self.samples_into_reset += 1;
            let Some(features) = self.framer.push(wide, in_band) else {
                continue;
            };
            self.silent_frames = if features.peak == 0.0 {
                self.silent_frames + 1
            } else {
                0
            };

            let frozen = self.pipeline.state() == State::Candidate;
            self.wide_noise.set_frozen(frozen);
            self.band_noise.set_frozen(frozen);
            let wide_floor = self.wide_noise.value();
            let band_floor = self.band_noise.value();
            self.wide_noise.observe(features.level_dbfs);
            self.band_noise.observe(features.band_rms_dbfs);

            let frame_samples = u64::from(self.detector.frame_samples() as u32);
            let detection = u64::from(self.detector.detection_rate_hz);
            let input_delta =
                (self.samples_into_reset * u64::from(self.rate_hz) + detection / 2) / detection;
            let frame_end = self.segment_anchor.saturating_add(input_delta);
            let frame_span = (frame_samples * u64::from(self.rate_hz) / detection).max(1);
            let frame_start = frame_end.saturating_sub(frame_span);

            if self.frames.enabled() {
                let row = FrameRow {
                    frame_index: self.samples_into_reset / frame_samples,
                    offset_start: frame_start,
                    offset_end: frame_end,
                    features,
                    wide_noise_floor: wide_floor,
                    band_noise_floor: band_floor,
                };
                self.frames.push_frame(row, &self.detector);
            }
            if self.seconds.enabled() {
                self.seconds.push_frame_into_second(
                    features,
                    frame_end,
                    self.rate_hz,
                    self.detector.frame_ms,
                );
            }
        }
    }

    /// A `--mode` boundary: either the service's segment rotation (no DSP reset,
    /// §5.4 last paragraph) or an injected gap (full reset, `end_reason=gap`).
    fn cross_boundary(&mut self) -> ToolResult<()> {
        self.boundaries += 1;
        match self.mode {
            RunMode::Continuous => {}
            RunMode::Segments => {
                // Close the open candidate, then re-anchor on the same continuous
                // stream: the noise floor and filter state carry over.
                let events = self.pipeline.close(self.boundary_end_reason, false);
                self.events.extend(events);
                self.segment_anchor = self.input_frames;
                let context = self.segment_context();
                let events = self
                    .pipeline
                    .begin_segment(context, self.segment_anchor, false);
                self.events.extend(events);
                self.framer.reset();
                self.samples_into_reset = 0;
                self.in_chunk_frames = 0;
            }
            RunMode::Chunks => {
                self.segment_anchor = self.input_frames;
                let events = self.pipeline.discontinuity(
                    Discontinuity::Gap,
                    self.segment_anchor,
                    self.chunk_frames,
                );
                self.events.extend(events);
                self.dc = DcBlocker::new(DcBlocker::DEFAULT_ALPHA);
                self.band.reset();
                self.framer.reset();
                self.samples_into_reset = 0;
                self.in_chunk_frames = 0;
                self.warmup_frames_left = self.detector.warmup_frames();
                self.silent_frames = 0;
                let long_gap = self.chunk_frames.saturating_mul(1000) / u64::from(self.rate_hz)
                    > self.detector.noise_floor_reset_gap_seconds * 1000;
                if long_gap {
                    self.wide_noise.reset();
                    self.band_noise.reset();
                }
            }
        }
        Ok(())
    }

    /// Flushes the pipeline and returns everything the writers need.
    fn finish(mut self, truncated: bool) -> ToolResult<Summary> {
        let mut events = std::mem::take(&mut self.events);
        events.extend(self.pipeline.close(EndReason::Shutdown, false));
        let wide_noise_floor = self.pipeline.noise_floor_dbfs();
        let band_noise_floor = self.pipeline.band_noise_floor_dbfs();
        let events_discarded_short = self.pipeline.events_discarded_short();
        let silent_run_seconds =
            self.silent_frames as f64 * f64::from(self.detector.frame_ms) / 1000.0;
        self.frames.finish();
        self.seconds.finish();
        Ok(Summary {
            rate_hz: self.rate_hz,
            channels: self.channels,
            selected_channel: self.selected_channel,
            frames: self.input_frames,
            seconds: self.input_frames as f64 / f64::from(self.rate_hz),
            truncated,
            silent: silent_run_seconds >= 1.0 && self.input_frames > 0,
            clipped_samples: self.clipped_samples,
            wide_noise_floor,
            band_noise_floor,
            events_discarded_short,
            event_count: events.len(),
            boundaries: self.boundaries,
            events,
        })
    }
}

/// Everything the writers need, produced after the input is fully consumed.
#[derive(Debug, Serialize)]
struct Summary {
    rate_hz: u32,
    channels: u16,
    selected_channel: u16,
    frames: u64,
    seconds: f64,
    truncated: bool,
    silent: bool,
    clipped_samples: u64,
    wide_noise_floor: Option<f32>,
    band_noise_floor: Option<f32>,
    events_discarded_short: u64,
    event_count: usize,
    boundaries: u64,
    events: Vec<EventDraft>,
}

/// One CSV row of the per-frame feature dump.
struct FrameRow {
    frame_index: u64,
    offset_start: u64,
    offset_end: u64,
    features: FrameFeatures,
    wide_noise_floor: Option<f32>,
    band_noise_floor: Option<f32>,
}

impl FrameRow {
    /// The per-frame decision inputs of §5.3, recomputed here so the dump
    /// explains why a frame did or did not become a hit. `hit` is left empty
    /// while a noise floor is still unknown, because no decision is made then.
    fn csv_line(&self, detector: &DetectorConfig) -> String {
        match self.wide_noise_floor.zip(self.band_noise_floor) {
            Some((wide_floor, band_floor)) => {
                let active_threshold = detector
                    .absolute_floor_dbfs
                    .max(wide_floor + detector.noise_margin_db);
                let active = self.features.level_dbfs > active_threshold;
                let snore_like = self.features.band_ratio >= detector.band_ratio_min
                    && self.features.band_rms_dbfs >= band_floor + detector.band_margin_db;
                format!(
                    "{},{},{},{:.3},{:.3},{:.4},{:.5},{:.3},{:.3},{:.3},{}",
                    self.frame_index,
                    self.offset_start,
                    self.offset_end,
                    self.features.level_dbfs,
                    self.features.band_rms_dbfs,
                    self.features.band_ratio,
                    self.features.peak,
                    wide_floor,
                    band_floor,
                    active_threshold,
                    u8::from(active && snore_like),
                )
            }
            None => format!(
                "{},{},{},{:.3},{:.3},{:.4},{:.5},,,,",
                self.frame_index,
                self.offset_start,
                self.offset_end,
                self.features.level_dbfs,
                self.features.band_rms_dbfs,
                self.features.band_ratio,
                self.features.peak,
            ),
        }
    }
}

/// Feature dump on stderr, either one row per frame or one row per second. Rows
/// are written during replay so a long recording never buffers its curve.
struct FeatureStream {
    writer: Option<BufWriter<std::io::Stderr>>,
    per_second: bool,
    level_dbfs: f32,
    pending: Option<PendingSecond>,
}

struct PendingSecond {
    second: u64,
    frames: u64,
    wide_sum: f64,
    band_sum: f64,
    ratio_sum: f64,
    peak: f32,
    over_level: u64,
}

impl FeatureStream {
    fn off() -> Self {
        FeatureStream {
            writer: None,
            per_second: false,
            level_dbfs: 0.0,
            pending: None,
        }
    }

    fn enabled(&self) -> bool {
        self.writer.is_some()
    }

    fn frames(level_dbfs: f32) -> Self {
        FeatureStream {
            writer: Some(BufWriter::new(std::io::stderr())),
            per_second: false,
            level_dbfs,
            pending: None,
        }
    }

    fn per_second(level_dbfs: f32) -> Self {
        FeatureStream {
            writer: Some(BufWriter::new(std::io::stderr())),
            per_second: true,
            level_dbfs,
            pending: None,
        }
    }

    fn push_frame(&mut self, row: FrameRow, detector: &DetectorConfig) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writeln!(writer, "{}", row.csv_line(detector));
        }
        let _ = row;
    }

    /// Rolls the per-second accumulator, emitting the previous second when the
    /// first frame of a new one arrives.
    fn push_frame_into_second(
        &mut self,
        features: FrameFeatures,
        offset_end: u64,
        rate_hz: u32,
        frame_ms: u32,
    ) {
        if self.writer.is_none() || !self.per_second {
            return;
        }
        let frames_per_second = (1000 / frame_ms.max(1)).max(1) as u64;
        let second = offset_end / u64::from(rate_hz);
        if self
            .pending
            .as_ref()
            .is_some_and(|slot| slot.second != second)
        {
            let finished = self.pending.take().expect("checked above");
            self.emit_second(finished, frames_per_second);
        }
        let slot = self.pending.get_or_insert(PendingSecond {
            second,
            frames: 0,
            wide_sum: 0.0,
            band_sum: 0.0,
            ratio_sum: 0.0,
            peak: 0.0,
            over_level: 0,
        });
        let gain = 10f32.powf(self.level_dbfs / 20.0);
        slot.frames += 1;
        slot.wide_sum += f64::from(features.level_dbfs);
        slot.band_sum += f64::from(features.band_rms_dbfs);
        slot.ratio_sum += f64::from(features.band_ratio);
        slot.peak = slot.peak.max(features.peak);
        if features.peak > gain {
            slot.over_level += 1;
        }
    }

    fn emit_second(&mut self, second: PendingSecond, frames_per_second: u64) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        let frames = second.frames as f64;
        let _ = writeln!(
            writer,
            "{},{},{},{:.3},{:.3},{:.4},{:.5},{}",
            second.second,
            second.second * frames_per_second,
            (second.second + 1) * frames_per_second,
            second.wide_sum / frames,
            second.band_sum / frames,
            second.ratio_sum / frames,
            second.peak,
            second.over_level,
        );
    }

    fn finish(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.emit_second(pending, 100);
        }
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
    }
}

/// Writes the event list as JSON (default) or CSV, to stdout or `--output`.
fn write_events(cli: &Cli, summary: &Summary) -> ToolResult<()> {
    if summary.truncated {
        eprintln!(
            "detect-wav: warning: the data chunk ends before its declared length; \
             analysing the {} frame(s) that are actually present",
            summary.frames
        );
    }
    let mut sink: Box<dyn Write> = match &cli.output {
        Some(path) => Box::new(BufWriter::new(File::create(path).map_err(|error| {
            ToolError::Message(format!("cannot create {}: {error}", path.display()))
        })?)),
        None => Box::new(BufWriter::new(std::io::stdout())),
    };
    let result = match cli.format {
        OutputFormat::Json => write_json(&mut sink, cli, summary),
        OutputFormat::Csv => write_csv(&mut sink, summary),
    };
    result.and_then(|()| {
        sink.flush()
            .map_err(|error| ToolError::Message(format!("cannot write event output: {error}")))
    })
}

fn write_json(sink: &mut dyn Write, cli: &Cli, summary: &Summary) -> ToolResult<()> {
    let events: Vec<serde_json::Value> = summary
        .events
        .iter()
        .enumerate()
        .map(|(index, event)| event_json(index as u64, event, cli.overrides.verbose))
        .collect();
    let document = serde_json::json!({
        "tool": "detect-wav",
        "wav": cli.wav.display().to_string(),
        "detector_version": DETECTOR_VERSION,
        "capture_rate_hz": summary.rate_hz,
        "channels": summary.channels,
        "selected_channel": summary.selected_channel,
        "frames": summary.frames,
        "duration_seconds": summary.seconds,
        "truncated": summary.truncated,
        "signal_state": if summary.silent { "silent" } else { "ok" },
        "clipped_samples": summary.clipped_samples,
        "wide_noise_floor_dbfs": summary.wide_noise_floor,
        "band_noise_floor_dbfs": summary.band_noise_floor,
        "events_discarded_short": summary.events_discarded_short,
        "boundaries": summary.boundaries,
        "mode": format!("{:?}", cli.overrides.mode.unwrap_or(RunMode::Continuous)).to_lowercase(),
        "overrides": cli.overrides.apply(&mut DetectorConfig::default()),
        "event_count": summary.event_count,
        "events": events,
    });
    let text = serde_json::to_string_pretty(&document)
        .map_err(|error| ToolError::Message(format!("cannot encode events as JSON: {error}")))?;
    writeln!(sink, "{text}")
        .map_err(|error| ToolError::Message(format!("cannot write JSON events: {error}")))
}

/// JSON for one event. `null` stays `null` (never 0); the database-only fields
/// `id`, `segment_id` and `ended_at_utc` are only added by `--verbose`.
fn event_json(index: u64, event: &EventDraft, verbose: bool) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert("index".into(), index.into());
    object.insert(
        "start_offset_frames".into(),
        event.start_offset_frames.into(),
    );
    object.insert("end_offset_frames".into(), event.end_offset_frames.into());
    object.insert("duration_ms".into(), event.duration_ms.into());
    object.insert(
        "started_at_utc".into(),
        event.started_at_utc.to_rfc3339().into(),
    );
    for (key, value) in [
        ("rule_score", event.rule_score),
        ("peak_dbfs", event.peak_dbfs),
        ("mean_level_dbfs", event.mean_level_dbfs),
        ("mean_band_ratio", event.mean_band_ratio),
        ("noise_floor_dbfs", event.noise_floor_dbfs),
    ] {
        let json = match value {
            Some(value) if value.is_finite() => serde_json::json!(value),
            _ => serde_json::Value::Null,
        };
        object.insert(key.into(), json);
    }
    object.insert("end_reason".into(), event.end_reason.as_str().into());
    object.insert("continued".into(), event.continued.into());
    if verbose {
        object.insert("segment_id".into(), event.segment_id.clone().into());
        object.insert(
            "ended_at_utc".into(),
            event.ended_at_utc.to_rfc3339().into(),
        );
        object.insert(
            "detector_version".into(),
            event.detector_version.clone().into(),
        );
    }
    serde_json::Value::Object(object)
}

fn write_csv(sink: &mut dyn Write, summary: &Summary) -> ToolResult<()> {
    writeln!(
        sink,
        "index,start_offset_frames,end_offset_frames,duration_ms,started_at_utc,rule_score,peak_dbfs,mean_level_dbfs,mean_band_ratio,noise_floor_dbfs,end_reason,continued"
    )
    .map_err(csv_error)?;
    for (index, event) in summary.events.iter().enumerate() {
        writeln!(
            sink,
            "{index},{},{},{},{},{},{},{},{},{},{},{}",
            event.start_offset_frames,
            event.end_offset_frames,
            event.duration_ms,
            event.started_at_utc.to_rfc3339(),
            optional(event.rule_score),
            optional(event.peak_dbfs),
            optional(event.mean_level_dbfs),
            optional(event.mean_band_ratio),
            optional(event.noise_floor_dbfs),
            event.end_reason.as_str(),
            event.continued,
        )
        .map_err(csv_error)?;
    }
    Ok(())
}

fn csv_error(error: std::io::Error) -> ToolError {
    ToolError::Message(format!("cannot write CSV events: {error}"))
}

/// Empty cell for a `null` value, matching JSON's explicit null.
fn optional(value: Option<f32>) -> String {
    match value {
        Some(value) if value.is_finite() => format!("{value:.3}"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_are_applied_field_by_field() {
        let overrides = DetectionOverrides {
            band_ratio_min: Some(0.4),
            noise_margin_db: Some(12.0),
            attack_min_hits: Some(2),
            ..DetectionOverrides::default()
        };
        let mut detector = DetectorConfig::default();
        let applied = overrides.apply(&mut detector);
        assert_eq!(detector.band_ratio_min, 0.4);
        assert_eq!(detector.noise_margin_db, 12.0);
        assert_eq!(detector.attack_min_hits, 2);
        assert_eq!(
            detector.hangover_frames,
            DetectorConfig::default().hangover_frames,
            "unset flags must not change anything"
        );
        assert_eq!(applied.len(), 3);
    }

    #[test]
    fn effective_detector_config_is_validated_by_the_service_validator() {
        assert!(validate_detector(&DetectorConfig::default()).is_ok());
        let broken = DetectorConfig {
            band_high_hz: 9_000.0,
            ..DetectorConfig::default()
        };
        let message = validate_detector(&broken).unwrap_err();
        assert!(message.contains("detector.band_high_hz"), "{message}");
    }

    #[test]
    fn unsupported_rates_widths_and_formats_are_named_in_the_error() {
        let error = WavReader::check_supported(22_050, 1, 16)
            .unwrap_err()
            .to_string();
        assert!(error.contains("22050"), "{error}");
        assert!(
            error.contains("[16000, 32000, 44100, 48000, 96000]"),
            "{error}"
        );
        let error = WavReader::check_supported(16_000, 3, 16)
            .unwrap_err()
            .to_string();
        assert!(error.contains("3 channels"), "{error}");
        let error = WavReader::check_supported(16_000, 1, 24)
            .unwrap_err()
            .to_string();
        assert!(error.contains("24-bit"), "{error}");
        assert!(WavReader::check_supported(44_100, 2, 16).is_ok());
    }

    #[test]
    fn a_declared_channel_count_is_checked_against_the_header() {
        let error = WavReader::check_declared_channels(1, Some(2))
            .unwrap_err()
            .to_string();
        assert!(error.contains("declares 1 channel"), "{error}");
        assert!(error.contains("--channels 2"), "{error}");
        assert!(WavReader::check_declared_channels(2, Some(2)).is_ok());
        assert!(WavReader::check_declared_channels(2, None).is_ok());
    }

    /// Writes a canonical 44-byte PCM header plus `frames` of a deterministic
    /// 320 Hz tone and returns the path.
    fn write_fixture(dir: &Path, name: &str, rate: u32, channels: u16, frames: u32) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for n in 0..frames {
            let value = ((n as f32 * 0.1).sin() * 8_000.0) as i16;
            for _ in 0..channels {
                writer.write_sample(value).unwrap();
            }
        }
        writer.finalize().unwrap();
        path
    }

    fn session_for(reader: &WavReader) -> Session {
        Session::new(SessionParams {
            detector: DetectorConfig::default(),
            rate_hz: reader.rate_hz,
            channels: reader.channels,
            selected_channel: 0,
            mode: RunMode::Continuous,
            chunk_seconds: 3_600,
            boundary_end_reason: EndReason::SegmentEnd,
            start_utc: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            segment_id: "test".to_string(),
        })
        .unwrap()
    }

    #[test]
    fn a_header_only_file_reads_as_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "empty.wav", 16_000, 1, 0);
        let mut reader = WavReader::open(&path, None).unwrap();
        let mut buffer = vec![0i16; 64];
        assert_eq!(reader.read_block(&mut buffer).unwrap(), 0);
        assert!(!reader.truncated());
    }

    #[test]
    fn a_short_data_chunk_is_reported_as_truncated_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "cut.wav", 16_000, 1, 4_000);
        // Cut the file mid-data while the header still declares every frame.
        let full = std::fs::read(&path).unwrap();
        std::fs::write(&path, &full[..3_000]).unwrap();
        let mut reader = WavReader::open(&path, None).unwrap();
        let mut buffer = vec![0i16; 4_000];
        let read = reader
            .read_block(&mut buffer)
            .expect("a cut tail must not be fatal");
        assert!(read > 0 && read < 4_000, "read {read}");
        assert!(reader.truncated());
    }

    #[test]
    fn every_supported_rate_replays_through_the_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        for rate in SUPPORTED_CAPTURE_RATES {
            let path = write_fixture(dir.path(), &format!("rate{rate}.wav"), rate, 2, rate);
            let mut reader = WavReader::open(&path, None).unwrap();
            let mut session = session_for(&reader);
            let mut buffer = vec![0i16; 4_096 * 2];
            let mut frames = 0u64;
            loop {
                let read = reader.read_block(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                let block = buffer[..read].to_vec();
                session.push_interleaved(&block).unwrap();
            }
            frames += session.input_frames;
            assert_eq!(frames, u64::from(rate), "input rate {rate}");
        }
    }

    #[test]
    fn events_carry_the_configured_origin_and_offsets_stay_inside_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "origin.wav", 16_000, 1, 3_200);
        let mut reader = WavReader::open(&path, None).unwrap();
        let start = chrono::DateTime::parse_from_rfc3339("2025-01-02T23:00:00Z")
            .unwrap()
            .to_utc();
        let mut session = Session::new(SessionParams {
            detector: DetectorConfig::default(),
            rate_hz: reader.rate_hz,
            channels: reader.channels,
            selected_channel: 0,
            mode: RunMode::Continuous,
            chunk_seconds: 3_600,
            boundary_end_reason: EndReason::SegmentEnd,
            start_utc: start,
            segment_id: "night-1".to_string(),
        })
        .unwrap();
        let mut buffer = vec![0i16; 1_600];
        while let Ok(read) = reader.read_block(&mut buffer) {
            if read == 0 {
                break;
            }
            let block = buffer[..read].to_vec();
            session.push_interleaved(&block).unwrap();
        }
        let summary = session.finish(false).unwrap();
        assert_eq!(summary.frames, 3_200);
        for event in &summary.events {
            assert!(event.started_at_utc >= start, "{:?}", event.started_at_utc);
            assert!(event.ended_at_utc <= start + chrono::Duration::milliseconds(200));
            assert!(event.end_offset_frames <= 3_200);
            assert_eq!(event.segment_id, "night-1");
        }
    }

    #[test]
    fn frame_rows_carry_the_decision_inputs() {
        let detector = DetectorConfig::default();
        let row = FrameRow {
            frame_index: 3,
            offset_start: 480,
            offset_end: 640,
            features: FrameFeatures {
                level_dbfs: -30.0,
                band_rms_dbfs: -34.0,
                band_ratio: 0.5,
                peak: 0.05,
            },
            wide_noise_floor: Some(-70.0),
            band_noise_floor: Some(-80.0),
        };
        let line = row.csv_line(&detector);
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields.len(), 11);
        assert_eq!(fields[0], "3");
        assert_eq!(fields[1], "480");
        assert_eq!(fields[2], "640");
        // level -30 is above max(-50, -70 + 8) and the band level clears its floor.
        assert_eq!(fields[10], "1");
        // While a floor is unknown no decision is made, so `hit` stays empty.
        let unknown = FrameRow {
            wide_noise_floor: None,
            band_noise_floor: None,
            ..row
        };
        let line = unknown.csv_line(&detector);
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields.len(), 11);
        assert_eq!(fields[10], "");
    }
}
