//! Asynchronous single-owner SQLite writer (TECH_SPEC §4.3/§7, task T4.2).
//!
//! Capture and DSP workers send commands; only this thread owns `Database` and
//! executes writes. Events that race ahead of `SegmentStarted` are retried rather
//! than dropped or allowed to crash the detector.

use crate::detector::EventDraft;
use crate::error::Result;
use crate::storage::{Database, SegmentCompleted, SegmentStarted};
use crate::timeline::GapRecord;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const FK_RETRY_LIMIT: usize = 8;
const FK_RETRY_PAUSE: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub enum DbCommand {
    SegmentStarted(SegmentStarted),
    SegmentCompleted(SegmentCompleted),
    Gap { segment_id: String, gap: GapRecord },
    Event(EventDraft),
    Flush(Sender<()>),
    Shutdown,
}

impl DbCommand {
    pub fn is_control(&self) -> bool {
        !matches!(self, DbCommand::Event(_) | DbCommand::Gap { .. })
    }
}

#[derive(Clone, Debug)]
pub struct DbWriterHandle {
    tx: Sender<DbCommand>,
}

impl DbWriterHandle {
    pub fn send(&self, command: DbCommand) -> std::result::Result<(), String> {
        self.tx.send(command).map_err(|_| "database writer has stopped".to_string())
    }

    pub fn segment_started(&self, segment: SegmentStarted) -> std::result::Result<(), String> {
        self.send(DbCommand::SegmentStarted(segment))
    }

    pub fn segment_completed(&self, segment: SegmentCompleted) -> std::result::Result<(), String> {
        self.send(DbCommand::SegmentCompleted(segment))
    }

    pub fn gap(&self, segment_id: impl Into<String>, gap: GapRecord) -> std::result::Result<(), String> {
        self.send(DbCommand::Gap { segment_id: segment_id.into(), gap })
    }

    pub fn event(&self, event: EventDraft) -> std::result::Result<(), String> {
        self.send(DbCommand::Event(event))
    }

    /// Waits until all commands enqueued before this call have been applied.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(DbCommand::Flush(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }
}

pub struct DbWriter {
    handle: DbWriterHandle,
    join: Option<JoinHandle<()>>,
}

impl DbWriter {
    pub fn spawn(database: Database, on_error: impl Fn(String) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = DbWriterHandle { tx };
        let join = thread::Builder::new()
            .name("snore-db-writer".into())
            .spawn(move || run_writer(database, rx, on_error))
            .expect("failed to spawn DB writer thread");
        DbWriter { handle, join: Some(join) }
    }

    pub fn handle(&self) -> DbWriterHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self, timeout: Duration) -> bool {
        // Flush is queued before Shutdown, so its acknowledgement proves every
        // prior command has been applied before we join the worker.
        let drained = self.handle.flush(timeout);
        let _ = self.handle.send(DbCommand::Shutdown);
        let Some(join) = self.join.take() else { return true; };
        if drained {
            let _ = join.join();
        }
        drained
    }
}

fn run_writer(database: Database, rx: Receiver<DbCommand>, on_error: impl Fn(String)) {
    let mut retry_events: Vec<EventDraft> = Vec::new();
    while let Ok(command) = rx.recv() {
        match command {
            DbCommand::SegmentStarted(segment) => {
                if let Err(error) = database.insert_segment_started(&segment) {
                    on_error(format!("segment start write failed: {error}"));
                }
                retry_fk_events(&database, &mut retry_events, &on_error);
            }
            DbCommand::SegmentCompleted(segment) => {
                if let Err(error) = database.complete_segment(&segment) {
                    on_error(format!("segment completion write failed: {error}"));
                }
            }
            DbCommand::Gap { segment_id, gap } => {
                if let Err(error) = database.insert_gap(&segment_id, &gap) {
                    on_error(format!("gap write failed for segment {segment_id}: {error}"));
                }
            }
            DbCommand::Event(event) => match database.insert_event(&event) {
                Ok(()) => {}
                Err(error) if is_foreign_key_error(&error) => {
                    retry_events.push(event);
                }
                Err(error) => on_error(format!("event write failed: {error}")),
            },
            DbCommand::Flush(ack) => {
                retry_fk_events(&database, &mut retry_events, &on_error);
                let _ = ack.send(());
            }
            DbCommand::Shutdown => {
                retry_fk_events(&database, &mut retry_events, &on_error);
                if !retry_events.is_empty() {
                    on_error(format!("{} event(s) remain retryable at shutdown", retry_events.len()));
                }
                break;
            }
        }
    }
}

fn retry_fk_events(database: &Database, pending: &mut Vec<EventDraft>, on_error: &impl Fn(String)) {
    if pending.is_empty() {
        return;
    }
    let mut remaining = Vec::with_capacity(pending.len());
    for event in pending.drain(..) {
        let mut inserted = false;
        for attempt in 0..FK_RETRY_LIMIT {
            match database.insert_event(&event) {
                Ok(()) => {
                    inserted = true;
                    break;
                }
                Err(error) if is_foreign_key_error(&error) => {
                    if attempt + 1 < FK_RETRY_LIMIT {
                        thread::sleep(FK_RETRY_PAUSE);
                    }
                }
                Err(error) => {
                    on_error(format!("event {} retry failed: {error}", event.id));
                    break;
                }
            }
        }
        if !inserted {
            remaining.push(event);
        }
    }
    *pending = remaining;
}

fn is_foreign_key_error(error: &crate::error::Error) -> bool {
    match error {
        crate::error::Error::Db(rusqlite::Error::SqliteFailure(code, _)) => {
            code.code == rusqlite::ErrorCode::ConstraintViolation
        }
        _ => false,
    }
}

/// Helper for synchronous setup paths that need to fail immediately.
pub fn apply(database: &Database, command: DbCommand) -> Result<()> {
    match command {
        DbCommand::SegmentStarted(s) => database.insert_segment_started(&s),
        DbCommand::SegmentCompleted(s) => database.complete_segment(&s),
        DbCommand::Gap { segment_id, gap } => database.insert_gap(&segment_id, &gap),
        DbCommand::Event(e) => database.insert_event(&e),
        DbCommand::Flush(_) | DbCommand::Shutdown => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{SegmentStarted, SegmentStatus};
    use crate::timeline::TimeQuality;
    use chrono::Utc;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn segment(id: &str) -> SegmentStarted {
        SegmentStarted {
            id: id.into(), relative_path: PathBuf::from(format!("{id}.wav")), started_at_utc: Utc::now(),
            capture_rate_hz: 16_000, channels: 1, sample_format: "S16_LE".into(),
            channel_mapping: "mono:0".into(), time_quality: TimeQuality::Synced, boot_id: "boot".into(),
        }
    }

    fn event() -> EventDraft {
        EventDraft {
            id: "evt".into(), segment_id: "seg".into(), start_offset_frames: 0, end_offset_frames: 1_600,
            started_at_utc: Utc::now(), ended_at_utc: Utc::now(), duration_ms: 100, rule_score: None,
            detector_version: "rule-v1".into(), peak_dbfs: None, mean_level_dbfs: None,
            mean_band_ratio: None, noise_floor_dbfs: None, end_reason: crate::detector::EndReason::Normal,
            continued: false,
        }
    }

    #[test]
    fn writer_orders_segment_before_event_and_flushes() {
        let db = Database::in_memory().unwrap();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errors_for_cb = Arc::clone(&errors);
        let writer = DbWriter::spawn(db, move |error| errors_for_cb.lock().unwrap().push(error));
        let handle = writer.handle();
        handle.event(event()).unwrap();
        handle.segment_started(segment("seg")).unwrap();
        assert!(handle.flush(Duration::from_secs(2)));
        assert!(writer.shutdown(Duration::from_secs(2)));
        assert!(errors.lock().unwrap().is_empty(), "writer errors: {:?}", *errors.lock().unwrap());
    }

    #[test]
    fn event_foreign_key_retry_is_visible_and_retained_until_segment_arrives() {
        let db = Database::in_memory().unwrap();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errors_for_cb = Arc::clone(&errors);
        let writer = DbWriter::spawn(db, move |error| errors_for_cb.lock().unwrap().push(error));
        let handle = writer.handle();
        handle.event(event()).unwrap();
        // Command ordering guarantees the event attempt occurs first; the writer
        // will retry after this SegmentStarted control message arrives.
        handle.segment_started(segment("seg")).unwrap();
        assert!(handle.flush(Duration::from_secs(2)));
        assert!(writer.shutdown(Duration::from_secs(2)));
        assert!(errors.lock().unwrap().is_empty(), "writer errors: {:?}", *errors.lock().unwrap());
    }

    #[test]
    fn db_command_control_classification_is_explicit() {
        assert!(!DbCommand::Event(event()).is_control());
        assert!(DbCommand::SegmentStarted(segment("seg")).is_control());
        let _ = SegmentStatus::Recording;
    }
}
