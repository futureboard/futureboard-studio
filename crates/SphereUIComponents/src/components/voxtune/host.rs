//! What the Voxtune editor needs from whoever embeds it.
//!
//! The editor is the same in Studio's tuner panel, the desktop app and the
//! VST3/ARA plug-in; only the transport and persistence differ:
//!
//! * the desktop app plays the engine itself ([`EngineTransport`]);
//! * the plug-in follows the DAW's transport and can't start it;
//! * Studio's panel follows Studio's transport, in clip time.

use std::sync::Arc;

use gpui::App;
use sphere_voxtune::engine::CantoAnalysisResult;
use sphere_voxtune::session::VoxDocument;
use sphere_voxtune::shared::Shared;

/// Transport as the editor shows it, in seconds of the take.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VoxTransport {
    pub playing: bool,
    pub position: f64,
}

pub trait VoxtuneHost: 'static {
    /// Current transport.
    fn transport(&self, cx: &App) -> VoxTransport;

    /// Whether the editor's play button may start and stop playback. A
    /// plug-in follows its host and can't.
    fn controls_transport(&self) -> bool;

    fn set_playing(&mut self, playing: bool, cx: &mut App);

    /// Moves the playhead to `seconds` of the take.
    fn seek(&mut self, seconds: f64, cx: &mut App);

    /// Whether pressing a note may preview it through the engine (needs the
    /// engine to be rendering to an output).
    fn can_audition(&self) -> bool;

    /// Loop playback over the whole take, if the host supports it.
    fn looping(&self, _cx: &App) -> Option<bool> {
        None
    }

    fn set_looping(&mut self, _on: bool, _cx: &mut App) {}

    /// Called after every committed edit (and undo / redo), with the
    /// document as it now is, so the host can persist or re-render it.
    fn document_changed(&mut self, doc: &VoxDocument, cx: &mut App);

    /// A load finished with `result` (fresh or from the cache), so the host
    /// can cache it.
    fn analysis_ready(&mut self, _result: &CantoAnalysisResult, _cx: &mut App) {}

    /// Status bar text the host adds: a note at the right (the output
    /// device) and a notice before it (an export result, a failure; `true`
    /// shows it as a warning).
    fn status(&self, _cx: &App) -> (Option<String>, Option<(String, bool)>) {
        (None, None)
    }
}

/// Transport for a host where the engine drives its own output (the
/// desktop app): play, stop, seek and loop act on the engine directly, and
/// the audio callback publishes the playhead through [`Shared`].
pub struct EngineTransport {
    pub shared: Arc<Shared>,
    /// Persists the document; None when nothing needs to.
    pub on_change: Option<Box<dyn FnMut(&VoxDocument, &mut App)>>,
}

impl VoxtuneHost for EngineTransport {
    fn transport(&self, _cx: &App) -> VoxTransport {
        VoxTransport {
            playing: self.shared.is_playing(),
            position: self.shared.playhead(),
        }
    }

    fn controls_transport(&self) -> bool {
        true
    }

    fn set_playing(&mut self, playing: bool, _cx: &mut App) {
        let mut engine = self.shared.lock();
        engine.set_playing(playing);
        self.shared.publish(&engine);
    }

    fn seek(&mut self, seconds: f64, _cx: &mut App) {
        let mut engine = self.shared.lock();
        engine.seek(seconds.max(0.0));
        self.shared.publish(&engine);
    }

    fn can_audition(&self) -> bool {
        true
    }

    fn looping(&self, _cx: &App) -> Option<bool> {
        Some(self.shared.lock().loop_enabled)
    }

    fn set_looping(&mut self, on: bool, _cx: &mut App) {
        self.shared.lock().loop_enabled = on;
    }

    fn document_changed(&mut self, doc: &VoxDocument, cx: &mut App) {
        if let Some(cb) = self.on_change.as_mut() {
            cb(doc, cx);
        }
    }
}
