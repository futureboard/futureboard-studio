//! Hand-off point for an edition-provided score (notation) view.
//!
//! The MIDI editor offers a Score mode only while a provider is installed
//! here. The shared crate holds no notation code: a Professional build's app
//! layer installs the provider when a verified license allows it and removes
//! it when the license lapses, exactly like the edition provider in
//! [`crate::edition`]. A Community build installs nothing and the mode never
//! appears.
//!
//! The view edits the clip through the same [`Timeline`] the piano roll does,
//! so its note edits are ordinary undoable edit commands.

use std::sync::{Arc, OnceLock, RwLock};

use gpui::{AnyView, App, Entity, Window};

use crate::components::piano_roll::UiMidiPreviewCommand;
use crate::components::timeline::Timeline;

/// What the MIDI editor hands the provider when it first shows Score mode.
pub struct ScoreViewContext {
    pub timeline: Entity<Timeline>,
    /// Audition notes on the edited track's instrument, as the piano roll does.
    pub preview: Option<Arc<dyn Fn(UiMidiPreviewCommand, &mut App) + Send + Sync>>,
}

/// A created score view.
pub struct ScoreViewHandle {
    pub view: AnyView,
    /// Point the view at a clip, or at none. The MIDI editor calls it every
    /// render with the clip it is editing; it must be cheap when nothing
    /// changed.
    pub set_clip: Box<dyn Fn(Option<String>, &mut App)>,
}

pub type ScoreViewFactory =
    Arc<dyn Fn(ScoreViewContext, &mut Window, &mut App) -> ScoreViewHandle + Send + Sync + 'static>;

fn slot() -> &'static RwLock<Option<ScoreViewFactory>> {
    static SLOT: OnceLock<RwLock<Option<ScoreViewFactory>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Install (`Some`) or remove (`None`) the score view provider.
pub fn set_score_view_provider(provider: Option<ScoreViewFactory>) {
    if let Ok(mut guard) = slot().write() {
        *guard = provider;
    }
}

/// The installed provider, if this build and license offer one.
pub fn score_view_provider() -> Option<ScoreViewFactory> {
    slot().read().ok().and_then(|guard| guard.clone())
}
