//! Realtime spectrum analysis for a bridged insert's audio.
//!
//! The analyser lives in `builtin_dsp_core` so LiveStage's engine, which runs
//! the built-ins in-process, measures exactly what this host does.

pub use builtin_dsp_core::spectrum::*;
