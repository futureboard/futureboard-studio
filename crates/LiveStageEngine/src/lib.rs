//! LiveStage engine: a live mixer and effects rack without a GUI.
//!
//! ```txt
//! interface inputs ──► channel strips ──► buses ──► master
//!                     (trim, inserts,    (inserts,  (inserts,
//!                      fader, pan,        fader)     fader)
//!                      sends)
//!        every strip ──► output patch ──► interface outputs
//!   armed strips ──► recorder ──► one WAV/FLAC per strip
//! ```
//!
//! [`Session`] is the setup as data, [`LiveEngine`] runs it: it opens the
//! interface ([`device`]), compiles the session into a realtime [`graph`],
//! runs the built-in effects in process ([`builtin_fx`]) and third-party ones
//! in Futureboard's plug-in host (`external`, behind the `external-plugins`
//! feature), and records ([`recorder`]).

pub mod builtin_fx;
pub mod device;
pub mod engine;
#[cfg(feature = "external-plugins")]
pub mod external;
pub mod graph;
pub mod recorder;
pub mod ring;
pub mod session;

pub use engine::{Command, EngineStatus, InsertState, LiveEngine, StripLevels};
pub use session::*;
