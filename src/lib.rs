//! Raspberry Pi snore monitor.
//!
//! The crate is split so that every stage of the pipeline can be tested without
//! a microphone: [`config`] and [`bounded_queue`] carry the plumbing, [`detector`]
//! and [`resampler`] are pure DSP, [`timeline`] is pure time arithmetic, and
//! [`storage`], [`recorder`] and [`retention`] only need a directory. The threads
//! that join them live in [`dispatcher`], [`audio_capture`] and [`http_server`].

pub mod bounded_queue;
pub mod config;
pub mod dsp;
pub mod error;
pub mod metrics;
pub mod resampler;
pub mod timeline;
pub mod storage;
pub mod recorder;
pub mod retention;
pub mod detector;
pub mod db_writer;
pub mod audio_capture;
pub mod dispatcher;
pub mod http_server;
pub mod recovery;
