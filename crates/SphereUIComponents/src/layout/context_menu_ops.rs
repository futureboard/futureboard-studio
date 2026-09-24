use gpui::{Context, Window, WindowId};

use super::studio_state::{ContextTarget, OpenPopover};
use super::StudioLayout;

/// Stable identifier for a context-menu hit target. Callbacks store only these
/// IDs — never entity handles or timeline borrows.
#[derive(Debug, Clone)]
pub enum ContextMenuTarget {
    EmptyTimeline,
    TrackHeader(String),
    Clip(String),
    MixerStrip(String),
    PluginSlot {
        track_id: String,
        slot_index: usize,
    },
    /// Tempo, browser, automation, ruler, and other existing targets.
    Extended(ContextTarget),
}

/// Request to open a context menu at a screen position.
#[derive(Debug, Clone)]
pub struct ContextMenuRequest {
    pub window_id: WindowId,
    pub x: f32,
    pub y: f32,
    pub target: ContextMenuTarget,
}

impl ContextMenuRequest {
    pub fn new(window_id: WindowId, x: f32, y: f32, target: ContextMenuTarget) -> Self {
        Self {
            window_id,
            x,
            y,
            target,
        }
    }

    pub fn from_window(window: &Window, x: f32, y: f32, target: ContextMenuTarget) -> Self {
        Self::new(window.window_handle().window_id(), x, y, target)
    }
}

impl ContextMenuTarget {
    pub fn from_context_target(target: ContextTarget) -> Self {
        match target {
            ContextTarget::TimelineEmpty => Self::EmptyTimeline,
            ContextTarget::Track(id) => Self::TrackHeader(id),
            ContextTarget::Clip(id) => Self::Clip(id),
            ContextTarget::Mixer(id) => Self::MixerStrip(id),
            other => Self::Extended(other),
        }
    }

    pub fn to_context_target(&self) -> ContextTarget {
        match self {
            Self::EmptyTimeline => ContextTarget::TimelineEmpty,
            Self::TrackHeader(id) => ContextTarget::Track(id.clone()),
            Self::Clip(id) => ContextTarget::Clip(id.clone()),
            Self::MixerStrip(id) => ContextTarget::Mixer(id.clone()),
            Self::PluginSlot { track_id, .. } => ContextTarget::Mixer(track_id.clone()),
            Self::Extended(target) => target.clone(),
        }
    }

    pub fn log_label(&self) -> String {
        match self {
            Self::EmptyTimeline => "EmptyTimeline".to_string(),
            Self::TrackHeader(id) => format!("TrackHeader({id})"),
            Self::Clip(id) => format!("Clip({id})"),
            Self::MixerStrip(id) => format!("MixerStrip({id})"),
            Self::PluginSlot {
                track_id,
                slot_index,
            } => format!("PluginSlot({track_id},{slot_index})"),
            Self::Extended(target) => format!("Extended({target:?})"),
        }
    }
}

fn context_menu_log(message: &str) {
    eprintln!("[ContextMenu] {message}");
}

impl StudioLayout {
    /// Open a context menu when the session is ready and the target still exists.
    pub(super) fn try_open_context_menu(
        &mut self,
        request: ContextMenuRequest,
        cx: &mut Context<Self>,
    ) {
        context_menu_log(&format!("request target={}", request.target.log_label()));
        if !self.session_install_status.is_ready() {
            context_menu_log("invalid target ignored reason=session-not-ready");
            return;
        }
        if !self.validate_context_menu_target(&request.target, cx) {
            context_menu_log("invalid target ignored");
            return;
        }
        // Track and clip menus list the ARA-capable plug-ins, which come from
        // the scanned catalog. That catalog is loaded lazily and used to be
        // armed only by the plug-in picker, so a context menu opened before the
        // picker had ever run reported "no ARA plug-ins" no matter what was
        // installed. Arming it here is a no-op once it is loaded.
        self.arm_catalog_load(cx);

        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.project_switcher.is_open = false;
        self.overlay.open_popover = Some(OpenPopover::Context { request });
        context_menu_log("opened");
        // The detached mixer draws the menus opened from its own window, so it
        // has to hear about one immediately — the meter cadence is too slow to
        // be a menu's response time.
        self.push_mixer_snapshot_to_window(cx);
        cx.notify();
    }

    pub(super) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.overlay.open_popover.take().is_some() {
            context_menu_log("closed");
            // Same reason as opening: a menu the pop-out mixer is drawing has to
            // come down when the Studio decides it is closed.
            self.push_mixer_snapshot_to_window(cx);
            cx.notify();
        }
    }

    pub(super) fn validate_context_menu_target(
        &self,
        target: &ContextMenuTarget,
        cx: &Context<Self>,
    ) -> bool {
        let state = &self.timeline.read(cx).state;
        match target {
            ContextMenuTarget::EmptyTimeline => true,
            ContextMenuTarget::TrackHeader(track_id) => state.find_track(track_id).is_some(),
            ContextMenuTarget::Clip(clip_id) => state.find_clip(clip_id).is_some(),
            ContextMenuTarget::MixerStrip(track_id) => state.find_track(track_id).is_some(),
            ContextMenuTarget::PluginSlot {
                track_id,
                slot_index,
            } => state
                .find_track(track_id)
                .is_some_and(|track| *slot_index < track.inserts.len()),
            ContextMenuTarget::Extended(extended) => match extended {
                ContextTarget::TimelineEmpty => true,
                ContextTarget::TrackLane { track_id, .. } => state.find_track(track_id).is_some(),
                ContextTarget::Track(track_id) => state.find_track(track_id).is_some(),
                ContextTarget::Clip(clip_id) => state.find_clip(clip_id).is_some(),
                ContextTarget::TimelineMarker { .. } => true,
                ContextTarget::SongTextMarker { event_id, .. } => {
                    state.song_text_event(event_id).is_some()
                }
                ContextTarget::AutomationLane { track_id, .. } => {
                    state.find_track(track_id).is_some()
                }
                ContextTarget::Browser(_) => true,
                ContextTarget::ChordTrack { .. } | ContextTarget::ChordLane => true,
                ContextTarget::Mixer(track_id) => state.find_track(track_id).is_some(),
                ContextTarget::SendPicker { track_id } => state.find_track(track_id).is_some(),
                ContextTarget::OutputPicker { track_id } => state.find_track(track_id).is_some(),
                // The pinned Master/Monitor strips always exist.
                ContextTarget::MasterOutputPicker | ContextTarget::MonitorOutputPicker => true,
                ContextTarget::AutomationTargetPicker { track_id } => {
                    state.find_track(track_id).is_some()
                }
                ContextTarget::Tempo
                | ContextTarget::TapTempo
                | ContextTarget::CountIn
                | ContextTarget::Metronome
                | ContextTarget::TimeSignature
                | ContextTarget::TimelineRuler { .. } => true,
                ContextTarget::TimeSignaturePoint { .. }
                | ContextTarget::TimeSignatureTrack { .. } => true,
                ContextTarget::TempoTrack { .. } => true,
                ContextTarget::MarkerLane | ContextTarget::RegionLane => true,
                ContextTarget::MarkerTrack { marker_id, .. } => marker_id
                    .as_deref()
                    .is_none_or(|id| state.marker(id).is_some()),
                ContextTarget::RegionTrack { region_id, .. } => region_id
                    .as_deref()
                    .is_none_or(|id| state.region(id).is_some()),
            },
        }
    }

    pub(super) fn context_target_for_open_menu(&self) -> Option<ContextTarget> {
        match self.overlay.open_popover.as_ref()? {
            OpenPopover::Context { request } => Some(request.target.to_context_target()),
            OpenPopover::AutomationTargetPicker { .. } => None,
        }
    }

    pub(super) fn validate_open_context_menu_action(&self, cx: &Context<Self>) -> bool {
        let Some(OpenPopover::Context { request }) = self.overlay.open_popover.as_ref() else {
            return true;
        };
        self.validate_context_menu_target(&request.target, cx)
    }
}
