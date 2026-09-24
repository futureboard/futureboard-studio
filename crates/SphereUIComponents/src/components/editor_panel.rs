//! Routes the bottom editor panel between AudioEditor, MidiEditor, and empty state.

use gpui::{
    div, px, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use sphere_audio_editor::{editor_kind_for_clip, ClipEditorKind};

use crate::components::ara_editor_host::AraEditorHost;
use crate::components::audio_editor::{clip_type_hint_for_selection, AudioEditorHost};
use crate::components::piano_roll::PianoRoll;
use crate::components::solfege_editor::SolfegeEditorPanel;
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::timeline_state::ClipType;
use crate::theme::Colors;

/// The two audio-facing surfaces hosted by the bottom Editor tab.
///
/// MIDI keeps its existing routing below; these tabs are shown when the
/// current target is an audio clip or an ARA-capable track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum EditorSurfaceTab {
    #[default]
    AudioEditor,
    Ara,
}

impl EditorSurfaceTab {
    const ALL: [Self; 2] = [Self::Ara, Self::AudioEditor];

    fn label(self) -> &'static str {
        match self {
            Self::Ara => "ARA",
            Self::AudioEditor => "Audio Editor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorSelectionTarget {
    Clip(String),
    Track(String),
}

pub struct ClipEditorPanel {
    timeline: Entity<Timeline>,
    piano_roll: Entity<PianoRoll>,
    solfege_editor: Entity<SolfegeEditorPanel>,
    audio_editor: Entity<AudioEditorHost>,
    ara_editor: Entity<AraEditorHost>,
    active_surface_tab: EditorSurfaceTab,
    last_selection_target: Option<EditorSelectionTarget>,
    /// Last branch reported by the trace, so the choice is logged on change
    /// rather than every frame.
    traced: Option<&'static str>,
    _timeline_observer: Subscription,
}

impl ClipEditorPanel {
    pub fn new(
        timeline: Entity<Timeline>,
        piano_roll: Entity<PianoRoll>,
        solfege_editor: Entity<SolfegeEditorPanel>,
        audio_editor: Entity<AudioEditorHost>,
        ara_editor: Entity<AraEditorHost>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _timeline_observer = cx.observe(&timeline, |_, _, cx| cx.notify());
        Self {
            timeline,
            piano_roll,
            solfege_editor,
            audio_editor,
            ara_editor,
            active_surface_tab: EditorSurfaceTab::default(),
            last_selection_target: None,
            traced: None,
            _timeline_observer,
        }
    }

    /// Used by the dock shell so the ARA pop-out affordance only appears when
    /// the ARA sub-tab is actually in front.
    pub(crate) fn ara_tab_active(&self) -> bool {
        self.active_surface_tab == EditorSurfaceTab::Ara
    }

    fn set_surface_tab(&mut self, tab: EditorSurfaceTab, cx: &mut Context<Self>) {
        if self.active_surface_tab == tab {
            return;
        }
        self.active_surface_tab = tab;
        if tab != EditorSurfaceTab::Ara {
            // The ARA view owns a native child window. Release it after the
            // click update unwinds so switching to Audio Editor never leaves
            // the plug-in surface parked over the new tab.
            let ara_editor = self.ara_editor.clone();
            cx.defer(move |cx| {
                let _ = ara_editor.update(cx, |host, cx| host.request_detach(cx));
            });
        }
        cx.notify();
    }

    /// Reports which editor the tab resolved to, once per change.
    fn trace(&mut self, branch: &'static str) {
        if self.traced == Some(branch) {
            return;
        }
        if std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some() {
            eprintln!("[ara-panel] editor tab -> {branch}");
        }
        self.traced = Some(branch);
    }

    /// Whether the current selection can show the ARA/Audio Editor tabs.
    fn editor_surface_tabs_visible(&self, cx: &Context<Self>) -> bool {
        let state = &self.timeline.read(cx).state;
        // Keep the sub-tab gate aligned with the authoritative editor route.
        let audio_clip_selected =
            editor_kind_for_clip(clip_type_hint_for_selection(state)) == ClipEditorKind::Audio;
        let ara_track_selected = state
            .selection
            .selected_clip_ids
            .first()
            .and_then(|clip_id| state.find_clip(clip_id).map(|(track, _)| track))
            .or_else(|| {
                state
                    .selection
                    .selected_track_id
                    .as_deref()
                    .and_then(|track_id| state.tracks.iter().find(|track| track.id == track_id))
            })
            .is_some_and(|track| track.ara.is_some());

        audio_clip_selected || ara_track_selected
    }

    /// Make a new clip/track selection choose the useful surface by default,
    /// while preserving an explicit sub-tab click until the target changes.
    fn sync_surface_tab_to_selection(&mut self, cx: &mut Context<Self>) {
        let state = &self.timeline.read(cx).state;
        let selected_clip = state.selection.selected_clip_ids.first();
        let target = selected_clip
            .cloned()
            .map(EditorSelectionTarget::Clip)
            .or_else(|| {
                state
                    .selection
                    .selected_track_id
                    .clone()
                    .map(EditorSelectionTarget::Track)
            });
        if target == self.last_selection_target {
            return;
        }
        self.last_selection_target = target.clone();

        let audio_clip_selected =
            editor_kind_for_clip(clip_type_hint_for_selection(state)) == ClipEditorKind::Audio;
        let ara_track_selected = selected_clip
            .and_then(|clip_id| state.find_clip(clip_id).map(|(track, _)| track))
            .or_else(|| {
                state
                    .selection
                    .selected_track_id
                    .as_deref()
                    .and_then(|track_id| state.tracks.iter().find(|track| track.id == track_id))
            })
            .is_some_and(|track| track.ara.is_some());
        let default_tab = if audio_clip_selected {
            EditorSurfaceTab::AudioEditor
        } else if ara_track_selected {
            EditorSurfaceTab::Ara
        } else {
            EditorSurfaceTab::AudioEditor
        };
        if self.active_surface_tab != default_tab {
            self.active_surface_tab = default_tab;
            if default_tab != EditorSurfaceTab::Ara {
                let ara_editor = self.ara_editor.clone();
                cx.defer(move |cx| {
                    let _ = ara_editor.update(cx, |host, cx| host.request_detach(cx));
                });
            }
        }
    }

    fn current_kind(&self, cx: &Context<Self>) -> ClipEditorKind {
        let hint = clip_type_hint_for_selection(&self.timeline.read(cx).state);
        editor_kind_for_clip(hint)
    }

    fn solfege_selected(&self, cx: &Context<Self>) -> bool {
        let state = &self.timeline.read(cx).state;
        let Some(clip_id) = state.selection.selected_clip_ids.first() else {
            return false;
        };
        state.find_clip(clip_id).is_some_and(|(track, clip)| {
            track.solfege.is_some() && matches!(&clip.clip_type, ClipType::Midi { .. })
        })
    }

    fn surface_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div()
            .id("editor-surface-tabs")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(3.0))
            .px(px(6.0))
            .h(px(26.0))
            .flex_none()
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_titlebar());

        for (index, tab) in EditorSurfaceTab::ALL.into_iter().enumerate() {
            let active = self.active_surface_tab == tab;
            row = row.child(
                div()
                    .id(("editor-surface-tab", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(18.0))
                    .px(px(10.0))
                    .rounded(px(crate::theme::radius::CONTROL))
                    .bg(if active {
                        Colors::accent_muted()
                    } else {
                        Colors::surface_input()
                    })
                    .text_size(px(10.0))
                    .text_color(if active {
                        Colors::text_primary()
                    } else {
                        Colors::text_muted()
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| style.bg(Colors::surface_hover()))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.set_surface_tab(tab, cx);
                    }))
                    .child(tab.label()),
            );
        }
        row
    }
}

impl Render for ClipEditorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_surface_tab_to_selection(cx);

        if self.editor_surface_tabs_visible(cx) {
            let body = match self.active_surface_tab {
                EditorSurfaceTab::Ara => {
                    self.trace("ara-tab");
                    self.audio_editor.read(cx).mark_hidden();
                    self.ara_editor.clone().into_any_element()
                }
                EditorSurfaceTab::AudioEditor => {
                    self.trace("audio-tab");
                    self.audio_editor.clone().into_any_element()
                }
            };
            return div()
                .flex()
                .flex_col()
                .size_full()
                .bg(Colors::surface_base())
                .child(self.surface_tab_bar(cx))
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element();
        }

        let kind = self.current_kind(cx);
        if kind != ClipEditorKind::Audio {
            self.audio_editor.read(cx).mark_hidden();
        }
        match kind {
            ClipEditorKind::Audio => {
                self.trace("audio");
                self.audio_editor.clone().into_any_element()
            }
            ClipEditorKind::Midi if self.solfege_selected(cx) => {
                self.trace("solfege");
                self.solfege_editor.clone().into_any_element()
            }
            ClipEditorKind::Midi => {
                self.trace("midi");
                self.piano_roll.clone().into_any_element()
            }
            ClipEditorKind::Empty => {
                self.trace("empty");
                empty_editor_panel().into_any_element()
            }
        }
    }
}

fn empty_editor_panel() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size_full()
        .bg(Colors::surface_base())
        .text_size(px(11.0))
        .text_color(Colors::text_muted())
        .child("Select a clip to edit")
}
