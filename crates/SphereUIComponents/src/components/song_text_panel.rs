//! Timeline-backed chord, lyric, and Song Text editing surfaces.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, uniform_list, App, AppContext, Bounds, Context, Entity, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, ScrollStrategy, StatefulInteractiveElement,
    Styled, Subscription, UniformListScrollHandle, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle,
};

use crate::components::edit::EditCommand;
use crate::components::inspector_kit;
use crate::components::text_input::{
    bind_mouse_selection, text_field_with_callbacks, TextInputAction, TextInputState,
};
use crate::components::timeline::timeline_state::{
    SongTextEvent, SongTextEventKind, SongTextEventType, TimeSignatureMap,
};
use crate::components::timeline::Timeline;
use crate::theme::Colors;
use crate::window_position::{apply_owner_display, centered_window_bounds};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SongTextPanelKind {
    ChordDisplay,
    LyricDisplay,
    LyricEditor,
}

impl SongTextPanelKind {
    /// Header glyph. Matches the dock tab so a popped-out window and the docked
    /// tab read as the same view.
    pub fn icon(self) -> &'static str {
        match self {
            Self::ChordDisplay => crate::assets::ICON_MUSIC_PATH,
            Self::LyricDisplay => crate::assets::ICON_NEWSPAPER_PATH,
            Self::LyricEditor => crate::assets::ICON_PENCIL_PATH,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::ChordDisplay => "Chords",
            Self::LyricDisplay => "Lyrics",
            Self::LyricEditor => "Song Text Editor",
        }
    }
}

pub struct SongTextPanelView {
    timeline: Entity<Timeline>,
    kind: SongTextPanelKind,
    chord_input: TextInputState,
    lyric_input: TextInputState,
    loaded_selection_id: Option<String>,
    list_scroll: UniformListScrollHandle,
    last_followed_event_id: Option<String>,
    manual_follow_until: Option<std::time::Instant>,
    cached_event_revision: u64,
    cached_time_signature_map: TimeSignatureMap,
    cached_events: std::sync::Arc<Vec<SongTextEvent>>,
    cached_position_labels: std::sync::Arc<Vec<String>>,
    _timeline_subscription: Subscription,
}

impl SongTextPanelView {
    pub fn new(
        timeline: Entity<Timeline>,
        kind: SongTextPanelKind,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&timeline, |view, _, cx| {
            view.sync_inputs_from_selection(cx);
            view.refresh_event_cache(cx);
            view.follow_playback(cx);
            cx.notify();
        });
        Self {
            timeline,
            kind,
            chord_input: TextInputState::new("song-text-chord", cx.focus_handle())
                .with_placeholder("Chord, e.g. Am7"),
            lyric_input: TextInputState::new("song-text-lyric", cx.focus_handle())
                .with_placeholder("Lyric phrase or line"),
            loaded_selection_id: None,
            list_scroll: UniformListScrollHandle::new(),
            last_followed_event_id: None,
            manual_follow_until: None,
            cached_event_revision: u64::MAX,
            cached_time_signature_map: TimeSignatureMap::with_default_4_4(),
            cached_events: std::sync::Arc::new(Vec::new()),
            cached_position_labels: std::sync::Arc::new(Vec::new()),
            _timeline_subscription: subscription,
        }
    }

    pub fn kind(&self) -> SongTextPanelKind {
        self.kind
    }

    pub fn is_text_input_focused(&self, window: &Window) -> bool {
        self.chord_input.focus_handle.is_focused(window)
            || self.lyric_input.focus_handle.is_focused(window)
    }

    fn refresh_event_cache(&mut self, cx: &Context<Self>) {
        let state = &self.timeline.read(cx).state;
        if self.cached_event_revision == state.song_text_revision
            && self.cached_time_signature_map == state.time_signature_map
        {
            return;
        }
        let events: Vec<_> = state
            .song_text_events
            .iter()
            .filter(|event| match self.kind {
                SongTextPanelKind::ChordDisplay => event.event_type() == SongTextEventType::Chord,
                SongTextPanelKind::LyricDisplay => matches!(
                    event.event_type(),
                    SongTextEventType::Lyric | SongTextEventType::Section
                ),
                SongTextPanelKind::LyricEditor => true,
            })
            .cloned()
            .collect();
        self.cached_position_labels = std::sync::Arc::new(
            events
                .iter()
                .map(|event| format!("[{}]", state.format_position_at(event.beat)))
                .collect(),
        );
        self.cached_events = std::sync::Arc::new(events);
        self.cached_event_revision = state.song_text_revision;
        self.cached_time_signature_map = state.time_signature_map.clone();
    }

    fn follow_playback(&mut self, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        if self.manual_follow_until.is_some_and(|until| until > now) {
            return;
        }
        self.manual_follow_until = None;
        let timeline = self.timeline.read(cx);
        let state = &timeline.state;
        if !state.transport.playing {
            self.last_followed_event_id = None;
            return;
        }
        let active = match self.kind {
            SongTextPanelKind::ChordDisplay => {
                state.active_song_text_event(SongTextEventType::Chord)
            }
            SongTextPanelKind::LyricDisplay | SongTextPanelKind::LyricEditor => state
                .active_song_text_event(SongTextEventType::Lyric)
                .or_else(|| state.active_song_text_event(SongTextEventType::Chord)),
        };
        let Some(active) = active else {
            return;
        };
        if self.last_followed_event_id.as_deref() == Some(active.id.as_str()) {
            return;
        }
        let active_id = active.id.clone();
        let start = self
            .cached_events
            .partition_point(|event| event.beat < active.beat);
        let index = self.cached_events[start..]
            .iter()
            .take_while(|event| event.beat == active.beat)
            .position(|event| event.id == active_id)
            .map(|offset| start + offset);
        if let Some(index) = index {
            self.list_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
            self.last_followed_event_id = Some(active_id);
        }
    }

    fn sync_inputs_from_selection(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .timeline
            .read(cx)
            .state
            .selected_song_text_event()
            .cloned();
        let selected_id = selected.as_ref().map(|event| event.id.clone());
        if selected_id == self.loaded_selection_id {
            return;
        }
        self.loaded_selection_id = selected_id;
        match selected.map(|event| event.kind) {
            Some(SongTextEventKind::Chord(chord)) => {
                self.chord_input.set_value(chord.symbol);
                self.lyric_input.set_value("");
            }
            Some(SongTextEventKind::Lyric(lyric)) => {
                self.chord_input.set_value("");
                self.lyric_input.set_value(lyric.text);
            }
            Some(SongTextEventKind::Section(_)) | None => {
                self.chord_input.set_value("");
                self.lyric_input.set_value("");
            }
        }
    }

    fn select(&mut self, id: &str, additive: bool, seek: bool, cx: &mut Context<Self>) {
        let id = id.to_string();
        let _ = self.timeline.update(cx, |timeline, cx| {
            let beat = timeline.state.song_text_event(&id).map(|event| event.beat);
            timeline.state.select_song_text_event(&id, additive);
            if seek {
                if let Some(beat) = beat {
                    timeline.seek_to_exact_beat(
                        beat as f32,
                        crate::layout::SeekReason::TimelineClick,
                        cx,
                    );
                }
            } else {
                cx.notify();
            }
        });
        self.loaded_selection_id = None;
        self.sync_inputs_from_selection(cx);
        cx.notify();
    }

    fn insertion_beat(&self, bypass_snap: bool, cx: &Context<Self>) -> f64 {
        let timeline = self.timeline.read(cx);
        timeline
            .state
            .snap_beats_with_bypass(timeline.state.transport.playhead_beats, bypass_snap)
            as f64
    }

    fn add_from_inputs(
        &mut self,
        add_chord: bool,
        add_lyric: bool,
        bypass_snap: bool,
        cx: &mut Context<Self>,
    ) {
        let beat = self.insertion_beat(bypass_snap, cx);
        let chord = self.chord_input.value.trim();
        let lyric = self.lyric_input.value.trim();
        let mut events = Vec::with_capacity(2);
        if add_chord {
            if let Some(event) = SongTextEvent::chord(beat, chord) {
                events.push(event);
            }
        }
        if add_lyric {
            if let Some(event) = SongTextEvent::lyric(beat, lyric) {
                events.push(event);
            }
        }
        if events.is_empty() {
            return;
        }
        let label = if events.len() == 2 {
            "Add Chord and Lyric"
        } else if events[0].event_type() == SongTextEventType::Chord {
            "Add Chord"
        } else {
            "Add Lyric"
        };
        let selected_ids: Vec<_> = events.iter().map(|event| event.id.clone()).collect();
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.run_metadata_edit_command(
                EditCommand::SetSongTextEvents {
                    label,
                    previous: Vec::new(),
                    next: events,
                },
                cx,
            );
            timeline.state.selection.selected_song_text_event_ids = selected_ids;
            cx.notify();
        });
        self.loaded_selection_id = None;
        self.sync_inputs_from_selection(cx);
        cx.notify();
    }

    fn update_selected(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self
            .timeline
            .read(cx)
            .state
            .selected_song_text_event()
            .cloned()
        else {
            return;
        };
        let mut next = previous.clone();
        match &mut next.kind {
            SongTextEventKind::Chord(chord) => {
                let value = self.chord_input.value.trim();
                if value.is_empty() {
                    return;
                }
                chord.symbol = value.to_string();
            }
            SongTextEventKind::Lyric(lyric) => {
                let value = self.lyric_input.value.trim();
                if value.is_empty() {
                    return;
                }
                lyric.text = value.to_string();
            }
            SongTextEventKind::Section(_) => return,
        }
        if previous == next {
            return;
        }
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.run_metadata_edit_command(
                EditCommand::SetSongTextEvents {
                    label: "Edit Song Text",
                    previous: vec![previous],
                    next: vec![next],
                },
                cx,
            );
        });
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let previous: Vec<_> = {
            let timeline = self.timeline.read(cx);
            timeline
                .state
                .selection
                .selected_song_text_event_ids
                .iter()
                .filter_map(|id| timeline.state.song_text_event(id).cloned())
                .collect()
        };
        if previous.is_empty() {
            return;
        }
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.run_metadata_edit_command(
                EditCommand::SetSongTextEvents {
                    label: "Delete Song Text",
                    previous,
                    next: Vec::new(),
                },
                cx,
            );
        });
        self.loaded_selection_id = None;
        self.sync_inputs_from_selection(cx);
        cx.notify();
    }

    fn move_selected_to_playhead(&mut self, bypass_snap: bool, cx: &mut Context<Self>) {
        let target = self.insertion_beat(bypass_snap, cx);
        let previous: Vec<_> = {
            let timeline = self.timeline.read(cx);
            timeline
                .state
                .selection
                .selected_song_text_event_ids
                .iter()
                .filter_map(|id| timeline.state.song_text_event(id).cloned())
                .collect()
        };
        let Some(anchor) = previous.first().map(|event| event.beat) else {
            return;
        };
        let delta = target - anchor;
        let next: Vec<_> = previous
            .iter()
            .cloned()
            .map(|mut event| {
                event.beat = (event.beat + delta).max(0.0);
                event
            })
            .collect();
        if previous == next {
            return;
        }
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.run_metadata_edit_command(
                EditCommand::SetSongTextEvents {
                    label: "Move Song Text",
                    previous,
                    next,
                },
                cx,
            );
        });
        cx.notify();
    }

    fn navigate(&mut self, next: bool, cx: &mut Context<Self>) {
        let target = {
            let timeline = self.timeline.read(cx);
            let state = &timeline.state;
            if let Some(selected) = state.selected_song_text_event() {
                if next {
                    state.next_song_text_event(&selected.id)
                } else {
                    state.previous_song_text_event(&selected.id)
                }
            } else if next {
                state
                    .song_text_events
                    .iter()
                    .find(|event| event.beat >= state.transport.playhead_beats as f64)
            } else {
                state
                    .song_text_events
                    .iter()
                    .rev()
                    .find(|event| event.beat <= state.transport.playhead_beats as f64)
            }
            .map(|event| event.id.clone())
        };
        if let Some(id) = target {
            self.select(&id, false, true, cx);
        }
    }

    fn clear_inputs(&mut self, cx: &mut Context<Self>) {
        self.chord_input.set_value("");
        self.lyric_input.set_value("");
        cx.notify();
    }

    pub fn command_add_chord_at_playhead(&mut self, cx: &mut Context<Self>) {
        self.add_from_inputs(true, false, false, cx);
    }

    pub fn command_add_lyric_at_playhead(&mut self, cx: &mut Context<Self>) {
        self.add_from_inputs(false, true, false, cx);
    }

    pub fn command_add_both_at_playhead(&mut self, cx: &mut Context<Self>) {
        self.add_from_inputs(true, true, false, cx);
    }

    pub fn command_commit(&mut self, cx: &mut Context<Self>) {
        if self
            .timeline
            .read(cx)
            .state
            .selected_song_text_event()
            .is_some()
        {
            self.update_selected(cx);
        } else {
            let add_chord = !self.chord_input.value.trim().is_empty();
            let add_lyric = !self.lyric_input.value.trim().is_empty();
            self.add_from_inputs(add_chord, add_lyric, false, cx);
        }
    }

    pub fn command_commit_next_grid(&mut self, cx: &mut Context<Self>) {
        self.command_commit(cx);
        self.advance_playhead(SongTextAdvance::Grid, cx);
    }

    pub fn command_commit_next_beat(&mut self, cx: &mut Context<Self>) {
        self.command_commit(cx);
        self.advance_playhead(SongTextAdvance::Beat, cx);
    }

    pub fn command_commit_next_bar(&mut self, cx: &mut Context<Self>) {
        self.command_commit(cx);
        self.advance_playhead(SongTextAdvance::Bar, cx);
    }

    pub fn command_previous_event(&mut self, cx: &mut Context<Self>) {
        self.navigate(false, cx);
    }

    pub fn command_next_event(&mut self, cx: &mut Context<Self>) {
        self.navigate(true, cx);
    }

    pub fn command_move_to_playhead(&mut self, cx: &mut Context<Self>) {
        self.move_selected_to_playhead(false, cx);
    }

    pub fn command_delete_selected(&mut self, cx: &mut Context<Self>) {
        self.delete_selected(cx);
    }

    fn advance_playhead(&mut self, mode: SongTextAdvance, cx: &mut Context<Self>) {
        let target = {
            let timeline = self.timeline.read(cx);
            let state = &timeline.state;
            let current = state.transport.playhead_beats as f64;
            match mode {
                SongTextAdvance::Grid => {
                    let step =
                        crate::components::timeline::timeline_state::SnapSettings::from_timeline(
                            state,
                        )
                        .to_musical()
                        .step_beats()
                        .unwrap_or(0.25);
                    current + step
                }
                SongTextAdvance::Beat => {
                    let signature = state.time_signature_map.time_signature_at_beat(current);
                    current
                        + crate::components::timeline::timeline_state::denominator_unit_quarter_beats(
                            signature.denominator,
                        )
                }
                SongTextAdvance::Bar => state.time_signature_map.next_bar_beat(current),
            }
        };
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.seek_to_exact_beat(
                target as f32,
                crate::layout::SeekReason::TimelineClick,
                cx,
            );
        });
    }

    fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let action = if self.chord_input.focus_handle.is_focused(window) {
            self.chord_input.handle_key_with_clipboard(event, Some(cx))
        } else if self.lyric_input.focus_handle.is_focused(window) {
            self.lyric_input.handle_key_with_clipboard(event, Some(cx))
        } else {
            return false;
        };
        match action {
            TextInputAction::Submit => {
                if self
                    .timeline
                    .read(cx)
                    .state
                    .selected_song_text_event()
                    .is_some()
                {
                    self.update_selected(cx);
                } else {
                    let add_chord = !self.chord_input.value.trim().is_empty();
                    let add_lyric = !self.lyric_input.value.trim().is_empty();
                    self.add_from_inputs(add_chord, add_lyric, event.keystroke.modifiers.shift, cx);
                }
            }
            TextInputAction::Consumed => cx.notify(),
            TextInputAction::Cancel => {
                self.loaded_selection_id = None;
                self.sync_inputs_from_selection(cx);
            }
            TextInputAction::Pass => return false,
        }
        true
    }
}

#[derive(Clone, Copy)]
enum SongTextAdvance {
    Grid,
    Beat,
    Bar,
}

impl Render for SongTextPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_inputs_from_selection(cx);
        self.refresh_event_cache(cx);
        let events = self.cached_events.clone();
        let position_labels = self.cached_position_labels.clone();
        let timeline = self.timeline.read(cx);
        let state = &timeline.state;
        let kind = self.kind;
        let selected_ids = state.selection.selected_song_text_event_ids.clone();
        let playhead_label = state.format_position_at(state.transport.playhead_beats as f64);
        let selected_label = state
            .selected_song_text_event()
            .map(|event| state.format_position_at(event.beat));
        let active_chord = state
            .active_song_text_event(SongTextEventType::Chord)
            .cloned();
        let active_lyric = state
            .active_song_text_event(SongTextEventType::Lyric)
            .cloned();

        let entity = cx.entity().clone();
        let chord_callbacks = bind_mouse_selection(entity.clone(), |view| &mut view.chord_input);
        let lyric_callbacks = bind_mouse_selection(entity.clone(), |view| &mut view.lyric_input);
        let scroll_entity = cx.entity().clone();
        let root = div()
            .id(("song-text-panel", kind as u32))
            .flex()
            .flex_col()
            .size_full()
            .min_w(px(260.0))
            .min_h(px(160.0))
            .overflow_hidden()
            .bg(Colors::surface_base())
            .capture_key_down(move |event, window, cx| {
                let handled = entity.update(cx, |view, cx| view.handle_key(event, window, cx));
                if handled {
                    cx.stop_propagation();
                }
            })
            .on_scroll_wheel(move |_, _, cx| {
                let _ = scroll_entity.update(cx, |view, _cx| {
                    view.manual_follow_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                });
            })
            .child(panel_header(kind.icon(), kind.title()));

        match kind {
            SongTextPanelKind::ChordDisplay | SongTextPanelKind::LyricDisplay => {
                let active = if kind == SongTextPanelKind::ChordDisplay {
                    active_chord.as_ref()
                } else {
                    active_lyric.as_ref()
                };
                let row_count = events.len();
                let list_events = events.clone();
                let list_positions = position_labels.clone();
                let list_entity = cx.entity().clone();
                let active_id = active.map(|event| event.id.clone());
                let rows = uniform_list(
                    ("song-text-display-list", kind as u32),
                    row_count,
                    move |range, _window, _cx| {
                        range
                            .map(|index| {
                                let event = &list_events[index];
                                let id = event.id.clone();
                                let selected = selected_ids.iter().any(|selected| selected == &id);
                                let is_active = active_id.as_deref() == Some(id.as_str());
                                let row_entity = list_entity.clone();
                                song_text_row(event, &list_positions[index], selected, is_active)
                                    .on_mouse_down(gpui::MouseButton::Left, move |mouse, _, cx| {
                                        cx.stop_propagation();
                                        let additive = mouse.modifiers.control
                                            || mouse.modifiers.platform
                                            || mouse.modifiers.shift;
                                        let _ = row_entity.update(cx, |view, cx| {
                                            view.select(&id, additive, true, cx)
                                        });
                                    })
                            })
                            .collect()
                    },
                )
                .size_full()
                .track_scroll(&self.list_scroll);

                root.child(active_summary(active, &playhead_label, kind))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .children((row_count > 0).then_some(rows))
                            .children((row_count == 0).then_some(empty_state(match kind {
                                SongTextPanelKind::ChordDisplay => {
                                    "No chords yet. Add one from the Edit tab at the playhead."
                                }
                                _ => "No lyrics yet. Add a line from the Edit tab at the playhead.",
                            }))),
                    )
            }
            SongTextPanelKind::LyricEditor => {
                let selected_event = self
                    .timeline
                    .read(cx)
                    .state
                    .selected_song_text_event()
                    .cloned();
                let can_update = match selected_event.as_ref().map(|event| event.event_type()) {
                    Some(SongTextEventType::Chord) => !self.chord_input.value.trim().is_empty(),
                    Some(SongTextEventType::Lyric) => !self.lyric_input.value.trim().is_empty(),
                    _ => false,
                };
                let has_selection = !selected_ids.is_empty();
                let can_add_chord = !self.chord_input.value.trim().is_empty();
                let can_add_lyric = !self.lyric_input.value.trim().is_empty();
                let can_add_both = can_add_chord && can_add_lyric;

                let list_events = events.clone();
                let list_positions = position_labels.clone();
                let row_count = list_events.len();
                let list_entity = cx.entity().clone();
                let active_chord_id = active_chord.as_ref().map(|event| event.id.clone());
                let active_lyric_id = active_lyric.as_ref().map(|event| event.id.clone());
                let editor_rows = uniform_list(
                    "song-text-editor-list",
                    row_count,
                    move |range, _window, _cx| {
                        range
                            .map(|index| {
                                let event = &list_events[index];
                                let id = event.id.clone();
                                let selected = selected_ids.iter().any(|selected| selected == &id);
                                let active = active_chord_id.as_deref() == Some(id.as_str())
                                    || active_lyric_id.as_deref() == Some(id.as_str());
                                let row_entity = list_entity.clone();
                                song_text_row(event, &list_positions[index], selected, active)
                                    .on_mouse_down(gpui::MouseButton::Left, move |mouse, _, cx| {
                                        cx.stop_propagation();
                                        let additive = mouse.modifiers.control
                                            || mouse.modifiers.platform
                                            || mouse.modifiers.shift;
                                        let seek = mouse.click_count >= 2;
                                        let _ = row_entity.update(cx, |view, cx| {
                                            view.select(&id, additive, seek, cx)
                                        });
                                    })
                            })
                            .collect()
                    },
                )
                .size_full()
                .track_scroll(&self.list_scroll);

                let add_chord_target = cx.entity().clone();
                let add_lyric_target = cx.entity().clone();
                let add_both_target = cx.entity().clone();
                let update_target = cx.entity().clone();
                let move_target = cx.entity().clone();
                let delete_target = cx.entity().clone();
                let previous_target = cx.entity().clone();
                let next_target = cx.entity().clone();
                let clear_target = cx.entity().clone();

                root.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_shrink_0()
                        .gap(px(6.0))
                        .p(px(9.0))
                        .border_b(px(1.0))
                        .border_color(Colors::border_subtle())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(info_value("PLAYHEAD", &playhead_label))
                                .child(info_value(
                                    "SELECTED",
                                    selected_label.as_deref().unwrap_or("None"),
                                )),
                        )
                        .child(field_label("CHORD"))
                        .child(text_field_with_callbacks(
                            &self.chord_input,
                            self.chord_input.is_focused(window),
                            chord_callbacks,
                        ))
                        .child(
                            div()
                                .flex()
                                .gap(px(5.0))
                                .child(action_button("Add Chord", can_add_chord).when(
                                    can_add_chord,
                                    |button| {
                                        button.on_click(move |mouse, _, cx| {
                                            let bypass = mouse.modifiers().shift;
                                            let _ = add_chord_target.update(cx, |view, cx| {
                                                view.add_from_inputs(true, false, bypass, cx)
                                            });
                                        })
                                    },
                                ))
                                .child(action_button("Update", can_update).when(
                                    can_update,
                                    |button| {
                                        button.on_click(move |_, _, cx| {
                                            let _ = update_target
                                                .update(cx, |view, cx| view.update_selected(cx));
                                        })
                                    },
                                )),
                        )
                        .child(field_label("LYRIC"))
                        .child(text_field_with_callbacks(
                            &self.lyric_input,
                            self.lyric_input.is_focused(window),
                            lyric_callbacks,
                        ))
                        .child(
                            div()
                                .flex()
                                .gap(px(5.0))
                                .child(action_button("Add Lyric", can_add_lyric).when(
                                    can_add_lyric,
                                    |button| {
                                        button.on_click(move |mouse, _, cx| {
                                            let bypass = mouse.modifiers().shift;
                                            let _ = add_lyric_target.update(cx, |view, cx| {
                                                view.add_from_inputs(false, true, bypass, cx)
                                            });
                                        })
                                    },
                                ))
                                .child(action_button("Add Both", can_add_both).when(
                                    can_add_both,
                                    |button| {
                                        button.on_click(move |mouse, _, cx| {
                                            let bypass = mouse.modifiers().shift;
                                            let _ = add_both_target.update(cx, |view, cx| {
                                                view.add_from_inputs(true, true, bypass, cx)
                                            });
                                        })
                                    },
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap(px(5.0))
                                .child(action_button("Previous", row_count > 0).when(
                                    row_count > 0,
                                    |button| {
                                        button.on_click(move |_, _, cx| {
                                            let _ = previous_target
                                                .update(cx, |view, cx| view.navigate(false, cx));
                                        })
                                    },
                                ))
                                .child(action_button("Next", row_count > 0).when(
                                    row_count > 0,
                                    |button| {
                                        button.on_click(move |_, _, cx| {
                                            let _ = next_target
                                                .update(cx, |view, cx| view.navigate(true, cx));
                                        })
                                    },
                                ))
                                .child(action_button("Move to Playhead", has_selection).when(
                                    has_selection,
                                    |button| {
                                        button.on_click(move |mouse, _, cx| {
                                            let bypass = mouse.modifiers().shift;
                                            let _ = move_target.update(cx, |view, cx| {
                                                view.move_selected_to_playhead(bypass, cx)
                                            });
                                        })
                                    },
                                ))
                                .child(action_button("Delete", has_selection).when(
                                    has_selection,
                                    |button| {
                                        button.on_click(move |_, _, cx| {
                                            let _ = delete_target
                                                .update(cx, |view, cx| view.delete_selected(cx));
                                        })
                                    },
                                ))
                                .child(action_button("Clear", true).on_click(move |_, _, cx| {
                                    let _ = clear_target
                                        .update(cx, |view, cx| view.clear_inputs(cx));
                                })),
                        ),
                )
                .child(
                    div()
                        .h(px(22.0))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .px(px(8.0))
                        .bg(Colors::surface_panel())
                        .border_b(px(1.0))
                        .border_color(Colors::border_subtle())
                        .text_size(px(9.5))
                        .text_color(Colors::text_muted())
                        .child("EVENTS IN TIMELINE ORDER"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .children((row_count > 0).then_some(editor_rows))
                        .children((row_count == 0).then_some(empty_state(
                            "Place the playhead, enter a chord or lyric, then add it to the timeline.",
                        ))),
                )
            }
        }
    }
}

fn panel_header(icon: &'static str, title: &'static str) -> impl IntoElement {
    div()
        .h(px(crate::theme::size::PROMINENT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(crate::theme::space::SNUG))
        .px(px(crate::theme::space::BASE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(
            gpui::svg()
                .path(icon)
                .w(px(inspector_kit::ICON))
                .h(px(inspector_kit::ICON))
                .flex_shrink_0()
                .text_color(Colors::text_muted()),
        )
        .child(
            div()
                .text_size(px(crate::theme::typography::UI_XS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_secondary())
                .child(title),
        )
}

fn active_summary(
    active: Option<&SongTextEvent>,
    playhead_label: &str,
    kind: SongTextPanelKind,
) -> impl IntoElement {
    let empty = if kind == SongTextPanelKind::ChordDisplay {
        "No active chord"
    } else {
        "No active lyric"
    };
    div()
        .h(px(48.0))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(2.0))
        .px(px(10.0))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel_alt())
        .child(
            div()
                .text_size(px(if active.is_some() {
                    18.0
                } else {
                    crate::theme::typography::UI_SM
                }))
                .font_weight(if active.is_some() {
                    gpui::FontWeight::BOLD
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .text_color(if active.is_some() {
                    Colors::text_primary()
                } else {
                    Colors::text_muted()
                })
                .truncate()
                .child(active.map(SongTextEvent::text).unwrap_or(empty).to_string()),
        )
        .child(
            div()
                .text_size(px(crate::theme::typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(format!("Playhead {playhead_label}")),
        )
}

fn song_text_row(
    event: &SongTextEvent,
    position_label: &str,
    selected: bool,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    let event_type = event.event_type();
    let type_color = match event_type {
        SongTextEventType::Section => Colors::accent_success(),
        SongTextEventType::Chord => Colors::accent_primary(),
        SongTextEventType::Lyric => Colors::text_secondary(),
    };
    let row_id = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        event.id.hash(&mut hasher);
        hasher.finish()
    };
    div()
        .id(("song-text-row", row_id))
        .h(px(crate::theme::size::ROW))
        .flex()
        .items_center()
        .gap(px(crate::theme::space::SNUG))
        .px(px(crate::theme::space::BASE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(if selected {
            Colors::accent_soft()
        } else if active {
            Colors::with_alpha(Colors::accent_primary(), 0.08)
        } else {
            Colors::surface_base()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(Colors::surface_hover()))
        .child(
            div()
                .w(px(inspector_kit::LABEL_COL))
                .flex_shrink_0()
                .text_size(px(crate::theme::typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(position_label.to_string()),
        )
        .child(
            div()
                .w(px(38.0))
                .flex_shrink_0()
                .text_size(px(crate::theme::typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(type_color)
                .child(event_type.label()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(crate::theme::typography::UI_SM))
                .font_weight(if event_type == SongTextEventType::Chord {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(event.text().to_string()),
        )
}

fn info_value(label: &'static str, value: &str) -> impl IntoElement {
    div()
        .flex()
        .gap(px(crate::theme::space::TIGHT))
        .text_size(px(crate::theme::typography::UI_XS))
        .child(div().text_color(Colors::text_muted()).child(label))
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(value.to_string()),
        )
}

fn field_label(label: &'static str) -> impl IntoElement {
    div()
        .text_size(px(crate::theme::typography::UI_XS))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_secondary())
        .child(label)
}

fn empty_state(message: &'static str) -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .top_0()
        .right_0()
        .bottom_0()
        .flex()
        .min_w_0()
        .child(inspector_kit::ins_empty(
            crate::assets::ICON_LIST_MUSIC_PATH,
            "Nothing here yet",
            message,
        ))
}

fn action_button(label: &'static str, enabled: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(label)
        .h(px(crate::theme::size::DEFAULT))
        .px(px(crate::theme::space::BASE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .text_size(px(9.5))
        .text_color(if enabled {
            Colors::text_secondary()
        } else {
            Colors::text_faint()
        })
        .cursor(if enabled {
            gpui::CursorStyle::PointingHand
        } else {
            gpui::CursorStyle::Arrow
        })
        .when(enabled, |button| {
            button.hover(|style| style.bg(Colors::surface_hover()))
        })
        .child(label)
}

pub struct SongTextWindow {
    panel: Entity<SongTextPanelView>,
    kind: SongTextPanelKind,
    on_close: std::sync::Arc<dyn Fn(SongTextPanelKind, &mut App) + Send + Sync>,
}

impl Render for SongTextWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let kind = self.kind;
        let on_close = self.on_close.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(Colors::surface_window())
            .child(crate::components::title_bar::external_window_titlebar(
                kind.title(),
                "song-text-window-close",
                move |window, cx| {
                    on_close(kind, cx);
                    window.remove_window();
                },
            ))
            .child(div().flex_1().min_h_0().child(self.panel.clone()))
    }
}

pub fn open_song_text_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    timeline: Entity<Timeline>,
    kind: SongTextPanelKind,
    on_close: std::sync::Arc<dyn Fn(SongTextPanelKind, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<SongTextWindow>, String> {
    let window_bounds = centered_window_bounds(owner_bounds, size(px(640.0), px(460.0)), cx);
    let mut options = crate::platform_chrome::external_window_options_partial();
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some(crate::platform_chrome::branded_window_title(kind.title()).into());
    }
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(px(300.0), px(240.0)));
    apply_owner_display(&mut options, owner_bounds, cx);
    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        let panel = cx.new(|cx| SongTextPanelView::new(timeline, kind, cx));
        cx.new(|_| SongTextWindow {
            panel,
            kind,
            on_close,
        })
    })
    .map_err(|error| error.to_string())
}
