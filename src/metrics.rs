//! Runtime counters and the volatile status snapshot (TECH_SPEC §12, task T1.3).
//!
//! Counters live in atomics because they are updated from the capture callback,
//! the dispatcher, the recorder, the detector and the DB writer. They are read by
//! the HTTP `/status` handler. Nothing here allocates on the audio path.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// Number of DSP frame timings kept for the p99 estimate.
const DSP_TIMING_WINDOW: usize = 1024;

/// Which component a status string describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Degraded,
    Unknown,
    Silent,
    Disconnected,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Degraded => "degraded",
            Health::Unknown => "unknown",
            Health::Silent => "silent",
            Health::Disconnected => "disconnected",
        }
    }
}

/// Detector state as reported by `/status` (TECH_SPEC §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorState {
    Unknown,
    WarmingUp,
    Idle,
    Candidate,
    Pending,
}

impl DetectorState {
    pub fn as_str(self) -> &'static str {
        match self {
            DetectorState::Unknown => "unknown",
            DetectorState::WarmingUp => "warming_up",
            DetectorState::Idle => "idle",
            DetectorState::Candidate => "candidate",
            DetectorState::Pending => "pending",
        }
    }
}

/// Fixed-window timing statistics for one repeated operation.
#[derive(Debug, Default)]
pub struct TimingStats {
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
    window: Mutex<TimingWindow>,
}

#[derive(Debug)]
struct TimingWindow {
    samples: Vec<u64>,
    next: usize,
    /// How many of `samples` have been written at least once.
    filled: usize,
}

impl Default for TimingWindow {
    fn default() -> Self {
        TimingWindow {
            samples: vec![0; DSP_TIMING_WINDOW],
            next: 0,
            filled: 0,
        }
    }
}

/// Summary of a [`TimingStats`] window.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimingSummary {
    pub count: u64,
    pub avg_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

impl TimingStats {
    pub fn record(&self, elapsed_ns: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        self.max_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
        if let Ok(mut window) = self.window.lock() {
            let slot = window.next;
            window.samples[slot] = elapsed_ns;
            window.next = (slot + 1) % window.samples.len();
            window.filled = (window.filled + 1).min(window.samples.len());
        }
    }

    pub fn summary(&self) -> TimingSummary {
        let count = self.count.load(Ordering::Relaxed);
        let sum = self.sum_ns.load(Ordering::Relaxed);
        let max = self.max_ns.load(Ordering::Relaxed);
        let p99 = self
            .window
            .lock()
            .ok()
            .map(|w| {
                if w.filled == 0 {
                    return 0;
                }
                // Only the populated part of the ring carries real timings; the
                // untouched tail is zero-filled and would drag the estimate down.
                let mut sorted = w.samples[..w.filled].to_vec();
                sorted.sort_unstable();
                let idx = ((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1);
                sorted[idx]
            })
            .unwrap_or(0);
        TimingSummary {
            count,
            avg_ns: sum.checked_div(count).unwrap_or(0),
            p99_ns: p99,
            max_ns: max,
        }
    }
}

/// All counters and status flags shared across threads.
#[derive(Debug, Default)]
pub struct Metrics {
    // --- capture / timeline (TECH_SPEC §4.1, §4.3) ---
    pub capture_ring_overflow_frames: AtomicU64,
    pub capture_ring_high_water_frames: AtomicU64,
    pub xrun_count: AtomicU64,
    pub lost_frames_total: AtomicU64,
    pub stream_rebuild_count: AtomicU64,
    pub gap_detection_limited: AtomicBool,

    // --- queues (TECH_SPEC §4.3) ---
    pub dropped_detection_blocks: AtomicU64,
    pub recording_queue_high_water: AtomicU64,
    pub detection_queue_high_water: AtomicU64,
    pub recording_queue_control_slots: AtomicU64,
    pub detection_queue_control_slots: AtomicU64,
    pub queue_overflow_gaps: AtomicU64,

    // --- dispatcher / recording ---
    pub clipped_samples_total: AtomicU64,
    pub segments_completed: AtomicU64,
    pub degraded: AtomicBool,

    // --- detector (TECH_SPEC §5.3, §12) ---
    pub events_total: AtomicU64,
    pub events_discarded_short: AtomicU64,
    pub event_fk_retries: AtomicU64,
    pub detector_state: AtomicU32,
    pub noise_floor_mdb: AtomicU32,
    pub band_noise_floor_mdb: AtomicU32,
    /// Set while `noise_floor_mdb` holds a measured value. Without it, the
    /// atomic's default `0` would read back as a plausible 0 dBFS reading.
    pub noise_floor_known: AtomicBool,
    pub band_noise_floor_known: AtomicBool,
    pub signal_silent: AtomicBool,

    // --- storage ---
    pub storage_pressure: AtomicBool,
    pub recording_stopped_low_disk: AtomicBool,
    pub deleted_segments: AtomicU64,
    pub missing_segments: AtomicU64,

    /// Whether the system wall clock is currently trusted. The Pi 3B has no RTC,
    /// so until NTP has settled `Utc::now()` is meaningless and segments recorded
    /// in that window must not claim `time_quality=synced` (§4.3). Defaults to
    /// `false`: an unprobed clock is reported as untrusted, never assumed good.
    pub time_synced: AtomicBool,

    // --- dsp cost ---
    pub dsp_block_time: TimingStats,

    // --- last error, for the status page ---
    last_error: Mutex<Option<String>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_last_error(&self, msg: impl Into<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(msg.into());
        }
    }

    pub fn clear_last_error(&self) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = None;
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn set_time_synced(&self, synced: bool) {
        self.time_synced.store(synced, Ordering::Relaxed);
    }

    pub fn time_synced(&self) -> bool {
        self.time_synced.load(Ordering::Relaxed)
    }

    /// Probes whether the host wall clock is trustworthy right now (§4.3).
    ///
    /// On Linux this asks the kernel via `adjtimex` whether the clock is
    /// synchronised. Anywhere else, and on any probe failure, the answer is
    /// `false`: a clock we cannot vouch for is reported as untrusted rather than
    /// optimistically assumed correct. Callers store the result with
    /// [`Metrics::set_time_synced`].
    pub fn probe_time_synced() -> bool {
        #[cfg(target_os = "linux")]
        {
            const STA_UNSYNC: libc::c_int = 0x0040;
            const STA_NANO: libc::c_int = 0x2000;
            let mut buf: libc::timex = unsafe { std::mem::zeroed() };
            buf.modes = STA_NANO;
            // SAFETY: `adjtimex` only reads/writes the `timex` we pass by pointer.
            let rc = unsafe { libc::adjtimex(&mut buf) };
            if rc < 0 {
                return false;
            }
            // `STA_UNSYNC` set means the kernel is not disciplined to a reference.
            buf.status & STA_UNSYNC == 0
        }
        #[cfg(not(target_os = "linux"))]
        {
            // No equivalent portable probe; report untrusted so nothing claims a
            // calibrated timestamp on a platform we have not validated.
            false
        }
    }

    pub fn set_detector_state(&self, state: DetectorState) {
        self.detector_state.store(state as u32, Ordering::Relaxed);
    }

    pub fn detector_state(&self) -> DetectorState {
        match self.detector_state.load(Ordering::Relaxed) {
            1 => DetectorState::WarmingUp,
            2 => DetectorState::Idle,
            3 => DetectorState::Candidate,
            4 => DetectorState::Pending,
            _ => DetectorState::Unknown,
        }
    }

    /// Stores a dBFS value as milli-dBFS in an atomic (f32 has no atomic form).
    fn store_dbfs(value: &AtomicU32, known: &AtomicBool, dbfs: Option<f32>) {
        match dbfs {
            Some(v) => {
                let mdb = (v.clamp(-200.0, 100.0) * 1000.0).round() as i32;
                value.store(mdb as u32, Ordering::Relaxed);
                known.store(true, Ordering::Relaxed);
            }
            None => known.store(false, Ordering::Relaxed),
        }
    }

    fn load_dbfs(value: &AtomicU32, known: &AtomicBool) -> Option<f32> {
        if known.load(Ordering::Relaxed) {
            Some(value.load(Ordering::Relaxed) as i32 as f32 / 1000.0)
        } else {
            None
        }
    }

    pub fn set_noise_floor_dbfs(&self, dbfs: Option<f32>) {
        Self::store_dbfs(&self.noise_floor_mdb, &self.noise_floor_known, dbfs);
    }

    pub fn noise_floor_dbfs(&self) -> Option<f32> {
        Self::load_dbfs(&self.noise_floor_mdb, &self.noise_floor_known)
    }

    pub fn set_band_noise_floor_dbfs(&self, dbfs: Option<f32>) {
        Self::store_dbfs(&self.band_noise_floor_mdb, &self.band_noise_floor_known, dbfs);
    }

    pub fn band_noise_floor_dbfs(&self) -> Option<f32> {
        Self::load_dbfs(&self.band_noise_floor_mdb, &self.band_noise_floor_known)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_floor_roundtrip_handles_none() {
        let m = Metrics::new();
        assert_eq!(m.noise_floor_dbfs(), None);
        m.set_noise_floor_dbfs(Some(-63.25));
        assert_eq!(m.noise_floor_dbfs(), Some(-63.25));
        m.set_noise_floor_dbfs(None);
        assert_eq!(m.noise_floor_dbfs(), None);
    }

    #[test]
    fn timing_summary_reports_max_and_avg() {
        let t = TimingStats::default();
        t.record(100);
        t.record(300);
        let s = t.summary();
        assert_eq!(s.count, 2);
        assert_eq!(s.avg_ns, 200);
        assert_eq!(s.max_ns, 300);
        assert_eq!(s.p99_ns, 300);
    }
}
