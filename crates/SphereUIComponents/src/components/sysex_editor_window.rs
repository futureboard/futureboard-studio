//! SysEx Editor — view, write and fix the System Exclusive messages on a MIDI
//! clip or on the arrangement markers.
//!
//! Timeline-backed like the Song Text editor: the list is re-derived from
//! `TimelineState` on every render (a clip or a song carries a handful of
//! messages, not thousands), and every change is one undoable command —
//! `SetClipSysEx` for a clip, `SetMarkers` for markers.
//!
//! The hex field is a scratch buffer. Templates and the checksum/device
//! helpers edit the field; **Add** writes it as a new message at the playhead
//! and **Apply** writes it over the selected one. Decoding, checksums and
//! templates come from `sphere_midi_service::sysex`, which knows the Roland,
//! Yamaha, Korg and Universal message layouts.
//!
//! What the messages do at playback is stated in the footer rather than
//! implied: they are sent to hardware MIDI outputs and written to MIDI export;
//! plug-in instruments do not receive SysEx.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, uniform_list, App, AppContext, Bounds, Context, Entity, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled,
    Subscription, UniformListScrollHandle, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle,
};
use sphere_midi_service::sysex::{self, Checksum, Vendor};

use crate::components::controls::{
    fb_badge, fb_button, fb_segment, fb_segmented_track, fb_stepper_button, FbButtonKind, FbSegment,
};
use crate::components::edit::EditCommand;
use crate::components::inspector_kit;
use crate::components::text_input::{
    bind_mouse_selection, text_field_with_callbacks, TextInputAction, TextInputState,
};
use crate::components::timeline::timeline_state::{
    ClipType, MidiSysExEvent, MidiSysExKind, TimelineMarkerState, TimelineState,
};
use crate::components::timeline::Timeline;
use crate::theme::{size as fb_size, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

const TITLE: &str = "SysEx Editor";

/// Which messages the editor is working on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysExScope {
    /// The MIDI clip selected in the arrangement.
    Clip,
    /// Messages carried by arrangement markers.
    Markers,
}

/// A row's identity. Clip events have no id of their own, so they are
/// addressed by index into the clip's (beat-sorted) list; marker messages by
/// marker id and index into that marker's list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowKey {
    Clip { clip_id: String, index: usize },
    Marker { marker_id: String, index: usize },
}

#[derive(Debug, Clone)]
struct Row {
    key: RowKey,
    /// Project beat, for the position column and seeking.
    beat: f64,
    marker_name: Option<String>,
    message: Vec<u8>,
    /// An SMF `F7` escape: raw bytes, shown but not decoded as SysEx.
    escaped: bool,
    decoded: sysex::Decoded,
}

/// The clip the Clip scope edits: the first selected clip, if it is MIDI.
fn target_clip(state: &TimelineState) -> Option<(String, String, String, f32, f32)> {
    let clip_id = state.selection.selected_clip_ids.first()?;
    let (track, clip) = state.find_clip(clip_id)?;
    matches!(clip.clip_type, ClipType::Midi { .. }).then(|| {
        (
            clip.id.clone(),
            clip.name.clone(),
            track.name.clone(),
            clip.start_beat,
            clip.duration_beats,
        )
    })
}

fn build_rows(state: &TimelineState, scope: SysExScope) -> Vec<Row> {
    match scope {
        SysExScope::Clip => {
            let Some((clip_id, _, _, start, _)) = target_clip(state) else {
                return Vec::new();
            };
            state
                .midi_clip_sysex(&clip_id)
                .map(|events| {
                    events
                        .iter()
                        .enumerate()
                        .map(|(index, event)| {
                            let message = event.message();
                            let escaped = event.kind == MidiSysExKind::Escaped;
                            Row {
                                key: RowKey::Clip {
                                    clip_id: clip_id.clone(),
                                    index,
                                },
                                beat: f64::from(start + event.beat),
                                marker_name: None,
                                decoded: sysex::decode(&message),
                                message,
                                escaped,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        SysExScope::Markers => state
            .markers
            .iter()
            .flat_map(|marker| {
                marker
                    .sysex
                    .iter()
                    .enumerate()
                    .map(move |(index, message)| Row {
                        key: RowKey::Marker {
                            marker_id: marker.id.clone(),
                            index,
                        },
                        beat: marker.beat,
                        marker_name: Some(marker.name.clone()),
                        message: message.clone(),
                        escaped: false,
                        decoded: sysex::decode(message),
                    })
            })
            .collect(),
    }
}

/// The field's contents, read as a message.
enum Draft {
    Empty,
    BadHex(String),
    Message {
        bytes: Vec<u8>,
        decoded: sysex::Decoded,
    },
}

impl Draft {
    fn parse(text: &str) -> Self {
        if text.trim().is_empty() {
            return Self::Empty;
        }
        match sysex::parse_hex(text) {
            Ok(bytes) => {
                let decoded = sysex::decode(&bytes);
                Self::Message { bytes, decoded }
            }
            Err(error) => Self::BadHex(error.message()),
        }
    }

    /// Bytes that may be written: a well-framed `F0 … F7` message.
    fn writable(&self) -> Option<&[u8]> {
        match self {
            Self::Message { bytes, decoded } if decoded.frame.is_ok() => Some(bytes),
            _ => None,
        }
    }
}

pub struct SysExEditorView {
    timeline: Entity<Timeline>,
    scope: SysExScope,
    vendor: Vendor,
    hex_input: TextInputState,
    selected: Option<RowKey>,
    /// The row the field was last filled from, so a timeline change does not
    /// overwrite what the user is typing unless the selection moved.
    loaded: Option<RowKey>,
    list_scroll: UniformListScrollHandle,
    _timeline_subscription: Subscription,
}

impl SysExEditorView {
    pub fn new(timeline: Entity<Timeline>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&timeline, |_view, _, cx| cx.notify());
        Self {
            timeline,
            scope: SysExScope::Clip,
            vendor: Vendor::Roland,
            hex_input: TextInputState::new("sysex-hex", cx.focus_handle())
                .with_placeholder("F0 41 10 42 12 40 00 7F 00 41 F7")
                .with_accessible_label("SysEx message bytes")
                // Hex is ASCII whatever the keyboard layout says.
                .with_ascii_charset("0123456789ABCDEFabcdefxXhH ,;:-$"),
            selected: None,
            loaded: None,
            list_scroll: UniformListScrollHandle::new(),
            _timeline_subscription: subscription,
        }
    }

    pub fn is_text_input_focused(&self, window: &Window) -> bool {
        self.hex_input.focus_handle.is_focused(window)
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        build_rows(&self.timeline.read(cx).state, self.scope)
    }

    fn selected_row(&self, cx: &App) -> Option<Row> {
        let key = self.selected.as_ref()?;
        self.rows(cx).into_iter().find(|row| &row.key == key)
    }

    /// Fill the field from the selection when the selection changed.
    fn sync_field(&mut self, cx: &App) {
        if self.selected == self.loaded {
            return;
        }
        let row = self.selected_row(cx);
        if row.is_none() {
            self.selected = None;
        }
        self.loaded = self.selected.clone();
        if let Some(row) = row {
            self.hex_input.set_value(sysex::format_hex(&row.message));
        }
    }

    fn set_scope(&mut self, scope: SysExScope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.selected = None;
            self.loaded = None;
        }
        cx.notify();
    }

    fn select(&mut self, key: RowKey, seek_beat: Option<f64>, cx: &mut Context<Self>) {
        self.selected = Some(key);
        self.sync_field(cx);
        if let Some(beat) = seek_beat {
            let _ = self.timeline.update(cx, |timeline, cx| {
                timeline.seek_to_exact_beat(
                    beat as f32,
                    crate::layout::SeekReason::TimelineClick,
                    cx,
                );
            });
        }
        cx.notify();
    }

    fn set_field(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.hex_input.set_value(sysex::format_hex(bytes));
        cx.notify();
    }

    fn playhead(&self, cx: &App) -> f64 {
        f64::from(
            self.timeline
                .read(cx)
                .state
                .transport
                .playhead_beats
                .max(0.0),
        )
    }

    /// Write the field as a new message at the playhead.
    fn add(&mut self, cx: &mut Context<Self>) {
        let Some(bytes) = Draft::parse(&self.hex_input.value)
            .writable()
            .map(<[u8]>::to_vec)
        else {
            return;
        };
        let playhead = self.playhead(cx);
        match self.scope {
            SysExScope::Clip => {
                let (clip_id, start, duration, prev) = {
                    let state = &self.timeline.read(cx).state;
                    let Some((clip_id, _, _, start, duration)) = target_clip(state) else {
                        return;
                    };
                    let prev = state.midi_clip_sysex(&clip_id).cloned().unwrap_or_default();
                    (clip_id, start, duration, prev)
                };
                // Inside the clip: a message past its end would never play.
                let local = (playhead as f32 - start).clamp(0.0, (duration - 1.0e-3).max(0.0));
                let Some(event) = MidiSysExEvent::from_message(local, &bytes) else {
                    return;
                };
                let mut next = prev.clone();
                next.push(event);
                // Same stable sort the state applies, so the index is exact.
                next.sort_by(|a, b| a.beat.total_cmp(&b.beat));
                let index = next
                    .iter()
                    .rposition(|e| e.beat == local && e.data == bytes[1..])
                    .unwrap_or(0);
                self.run_clip_command("Add SysEx", &clip_id, prev, next, cx);
                self.selected = Some(RowKey::Clip { clip_id, index });
            }
            SysExScope::Markers => {
                let prev = self.timeline.read(cx).state.markers.clone();
                let mut next = prev.clone();
                // The marker at the playhead takes the message; with none there,
                // one is placed, so a reset can be pinned to bar 1 in one step.
                let position = next.iter().position(|m| (m.beat - playhead).abs() < 1.0e-6);
                let marker_index = match position {
                    Some(i) => i,
                    None => {
                        next.push(TimelineMarkerState::new(
                            playhead,
                            "SysEx",
                            crate::color::rgba_to_hex(Colors::automation_curve()),
                        ));
                        next.len() - 1
                    }
                };
                next[marker_index].sysex.push(bytes);
                let marker_id = next[marker_index].id.clone();
                let index = next[marker_index].sysex.len() - 1;
                next.sort_by(|a, b| a.beat.total_cmp(&b.beat).then_with(|| a.id.cmp(&b.id)));
                self.run_marker_command("Add Marker SysEx", next, cx);
                self.selected = Some(RowKey::Marker { marker_id, index });
            }
        }
        self.loaded = self.selected.clone();
        cx.notify();
    }

    /// Write the field over the selected message.
    fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(bytes) = Draft::parse(&self.hex_input.value)
            .writable()
            .map(<[u8]>::to_vec)
        else {
            return;
        };
        self.edit_selected("Edit SysEx", cx, |message| *message = bytes.clone());
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        match key {
            RowKey::Clip { clip_id, index } => {
                let Some(prev) = self
                    .timeline
                    .read(cx)
                    .state
                    .midi_clip_sysex(&clip_id)
                    .cloned()
                else {
                    return;
                };
                if index >= prev.len() {
                    return;
                }
                let mut next = prev.clone();
                next.remove(index);
                self.run_clip_command("Delete SysEx", &clip_id, prev, next, cx);
            }
            RowKey::Marker { marker_id, index } => {
                let mut next = self.timeline.read(cx).state.markers.clone();
                let Some(marker) = next.iter_mut().find(|m| m.id == marker_id) else {
                    return;
                };
                if index >= marker.sysex.len() {
                    return;
                }
                marker.sysex.remove(index);
                self.run_marker_command("Delete Marker SysEx", next, cx);
            }
        }
        self.selected = None;
        self.loaded = None;
        cx.notify();
    }

    /// Clip scope only: markers own their position, the marker lane moves them.
    fn move_selected_to_playhead(&mut self, cx: &mut Context<Self>) {
        let Some(RowKey::Clip { clip_id, index }) = self.selected.clone() else {
            return;
        };
        let playhead = self.playhead(cx) as f32;
        let (prev, start, duration) = {
            let state = &self.timeline.read(cx).state;
            let Some((_, clip)) = state.find_clip(&clip_id) else {
                return;
            };
            let Some(prev) = state.midi_clip_sysex(&clip_id).cloned() else {
                return;
            };
            (prev, clip.start_beat, clip.duration_beats)
        };
        let Some(event) = prev.get(index).cloned() else {
            return;
        };
        let local = (playhead - start).clamp(0.0, (duration - 1.0e-3).max(0.0));
        let mut next = prev.clone();
        next.remove(index);
        let moved = event.at_beat(local);
        next.push(moved.clone());
        next.sort_by(|a, b| a.beat.total_cmp(&b.beat));
        let new_index = next
            .iter()
            .rposition(|e| e.beat == moved.beat && e.data == moved.data)
            .unwrap_or(0);
        self.run_clip_command("Move SysEx", &clip_id, prev, next, cx);
        self.selected = Some(RowKey::Clip {
            clip_id,
            index: new_index,
        });
        self.loaded = self.selected.clone();
        cx.notify();
    }

    /// Recompute the checksum in the field; commit it when a message is
    /// selected, since fixing is never a change anyone wants to review first.
    fn fix_checksum(&mut self, cx: &mut Context<Self>) {
        let Draft::Message { mut bytes, .. } = Draft::parse(&self.hex_input.value) else {
            return;
        };
        if sysex::fix_checksum(&mut bytes) {
            self.set_field(&bytes, cx);
            if self.selected.is_some() {
                self.edit_selected("Fix SysEx Checksum", cx, |message| *message = bytes.clone());
            }
        }
    }

    /// Step the device ID / channel, keeping any checksum right.
    fn step_device(&mut self, delta: i16, cx: &mut Context<Self>) {
        let Draft::Message { mut bytes, decoded } = Draft::parse(&self.hex_input.value) else {
            return;
        };
        let Some(device) = decoded.device else {
            return;
        };
        let max = match decoded.manufacturer {
            Some(sysex::Manufacturer::Yamaha | sysex::Manufacturer::Korg) => 0x0F,
            _ => 0x7F,
        };
        let next = (i16::from(device) + delta).clamp(0, max) as u8;
        if next == device || !sysex::set_device(&mut bytes, next) {
            return;
        }
        self.set_field(&bytes, cx);
        if self.selected.is_some() {
            self.edit_selected("Change SysEx Device", cx, |message| {
                *message = bytes.clone()
            });
        }
    }

    /// Rewrite the selected message's bytes with `edit`, as one undo step.
    fn edit_selected(
        &mut self,
        label: &'static str,
        cx: &mut Context<Self>,
        edit: impl Fn(&mut Vec<u8>),
    ) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        match key {
            RowKey::Clip { clip_id, index } => {
                let Some(prev) = self
                    .timeline
                    .read(cx)
                    .state
                    .midi_clip_sysex(&clip_id)
                    .cloned()
                else {
                    return;
                };
                let Some(event) = prev.get(index) else {
                    return;
                };
                let mut message = event.message();
                edit(&mut message);
                let Some(replacement) = MidiSysExEvent::from_message(event.beat, &message) else {
                    return;
                };
                let mut next = prev.clone();
                next[index] = MidiSysExEvent {
                    tick: event.tick,
                    ..replacement
                };
                if next != prev {
                    self.run_clip_command(label, &clip_id, prev, next, cx);
                }
            }
            RowKey::Marker { marker_id, index } => {
                let prev = self.timeline.read(cx).state.markers.clone();
                let mut next = prev.clone();
                let Some(message) = next
                    .iter_mut()
                    .find(|m| m.id == marker_id)
                    .and_then(|m| m.sysex.get_mut(index))
                else {
                    return;
                };
                edit(message);
                if next != prev {
                    self.run_marker_command(label, next, cx);
                }
            }
        }
        cx.notify();
    }

    fn run_clip_command(
        &mut self,
        label: &'static str,
        clip_id: &str,
        prev: Vec<MidiSysExEvent>,
        next: Vec<MidiSysExEvent>,
        cx: &mut Context<Self>,
    ) {
        let clip_id = clip_id.to_string();
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.run_edit_command(
                EditCommand::SetClipSysEx {
                    label,
                    clip_id,
                    prev,
                    next,
                },
                cx,
            );
        });
    }

    fn run_marker_command(
        &mut self,
        label: &'static str,
        next: Vec<TimelineMarkerState>,
        cx: &mut Context<Self>,
    ) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            let prev = timeline.state.markers.clone();
            timeline.run_edit_command(EditCommand::SetMarkers { label, prev, next }, cx);
        });
    }

    fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.hex_input.focus_handle.is_focused(window) {
            return false;
        }
        match self.hex_input.handle_key_with_clipboard(event, Some(cx)) {
            TextInputAction::Submit => {
                if self.selected.is_some() {
                    self.apply(cx);
                } else {
                    self.add(cx);
                }
            }
            TextInputAction::Consumed => cx.notify(),
            TextInputAction::Cancel => {
                self.loaded = None;
                self.sync_field(cx);
                cx.notify();
            }
            TextInputAction::Pass => return false,
        }
        true
    }
}

fn vendor_of(manufacturer: Option<sysex::Manufacturer>) -> (&'static str, gpui::Rgba) {
    use sysex::Manufacturer as M;
    match manufacturer {
        Some(M::Roland) => ("Roland", Colors::accent_primary()),
        Some(M::Yamaha) => ("Yamaha", Colors::status_warning()),
        Some(M::Korg) => ("Korg", Colors::status_success()),
        Some(M::UniversalNonRealtime | M::UniversalRealtime) => {
            ("Universal", Colors::text_secondary())
        }
        Some(_) => ("Other", Colors::text_muted()),
        None => ("—", Colors::text_muted()),
    }
}

fn checksum_badge(checksum: Checksum) -> Option<gpui::AnyElement> {
    match checksum {
        Checksum::None => None,
        Checksum::Valid => Some(fb_badge("✓ Sum", Colors::status_success()).into_any_element()),
        Checksum::Invalid { .. } => {
            Some(fb_badge("✕ Sum", Colors::status_error()).into_any_element())
        }
    }
}

fn sysex_row(row: &Row, position: &str, selected: bool) -> gpui::Stateful<gpui::Div> {
    let (vendor, tone) = if row.escaped {
        ("Raw", Colors::text_muted())
    } else {
        vendor_of(row.decoded.manufacturer)
    };
    let row_id = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", row.key).hash(&mut hasher);
        hasher.finish()
    };
    let summary = if row.escaped {
        format!("F7 escape · {}", sysex::format_hex(&row.message))
    } else {
        row.decoded.summary.clone()
    };
    div()
        .id(("sysex-row", row_id))
        .h(px(fb_size::ROW))
        .flex()
        .items_center()
        .gap(px(space::SNUG))
        .px(px(space::BASE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(if selected {
            Colors::accent_soft()
        } else {
            Colors::surface_base()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(Colors::surface_hover()))
        .child(
            div()
                .w(px(inspector_kit::LABEL_COL))
                .flex_shrink_0()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(position.to_string()),
        )
        .when_some(row.marker_name.clone(), |this, name| {
            this.child(
                div()
                    .w(px(84.0))
                    .flex_shrink_0()
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_secondary())
                    .child(name),
            )
        })
        .child(
            div()
                .w(px(70.0))
                .flex_shrink_0()
                .child(fb_badge(vendor, tone)),
        )
        .child(
            div()
                .w(px(76.0))
                .flex_shrink_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(row.decoded.model.clone().unwrap_or_default()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_SM))
                .text_color(if row.decoded.frame.is_ok() || row.escaped {
                    Colors::text_primary()
                } else {
                    Colors::status_error()
                })
                .child(summary),
        )
        .children(checksum_badge(row.decoded.checksum))
        .child(
            div()
                .w(px(44.0))
                .flex_shrink_0()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .child(format!("{} B", row.message.len())),
        )
}

fn caption(text: impl Into<String>) -> impl IntoElement {
    div()
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_secondary())
        .child(text.into())
}

fn quiet(text: impl Into<String>) -> impl IntoElement {
    div()
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_muted())
        .child(text.into())
}

impl Render for SysExEditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_field(cx);
        let rows = self.rows(cx);
        let (target_label, positions, playhead_label, has_target) = {
            let state = &self.timeline.read(cx).state;
            let positions: Vec<String> = rows
                .iter()
                .map(|row| state.format_position_at(row.beat))
                .collect();
            let playhead_label = state.format_position_at(state.transport.playhead_beats as f64);
            let (label, has_target) = match self.scope {
                SysExScope::Clip => match target_clip(state) {
                    Some((_, clip, track, _, _)) => (format!("{track} · {clip}"), true),
                    None => ("Select a MIDI clip in the arrangement".to_string(), false),
                },
                SysExScope::Markers => (format!("{} markers", state.markers.len()), true),
            };
            (label, positions, playhead_label, has_target)
        };
        let draft = Draft::parse(&self.hex_input.value);
        let can_write = has_target && draft.writable().is_some();
        let has_selection = self.selected.is_some();
        let selected_is_clip = matches!(self.selected, Some(RowKey::Clip { .. }));
        let (draft_line, draft_ok, draft_checksum, draft_device) = match &draft {
            Draft::Empty => (
                "Pick a template or paste hex from a synth manual.".to_string(),
                true,
                Checksum::None,
                None,
            ),
            Draft::BadHex(error) => (error.clone(), false, Checksum::None, None),
            Draft::Message { decoded, .. } => {
                let who = decoded
                    .manufacturer
                    .map(|m| m.label())
                    .unwrap_or_else(|| "—".to_string());
                let model = decoded
                    .model
                    .as_ref()
                    .map(|m| format!(" {m}"))
                    .unwrap_or_default();
                (
                    format!("{who}{model} · {}", decoded.summary),
                    decoded.frame.is_ok(),
                    decoded.checksum,
                    decoded.device,
                )
            }
        };

        let entity = cx.entity().clone();
        let hex_callbacks = bind_mouse_selection(entity.clone(), |view| &mut view.hex_input);
        let scope = self.scope;
        let vendor = self.vendor;

        // ── Scope bar ────────────────────────────────────────────────────
        let scope_bar = {
            let clip_target = entity.clone();
            let marker_target = entity.clone();
            div()
                .flex()
                .items_center()
                .gap(px(space::BASE))
                .px(px(space::BASE))
                .py(px(space::SNUG))
                .border_b(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    fb_segmented_track()
                        .w(px(220.0))
                        .flex_shrink_0()
                        .child(fb_segment(
                            "sysex-scope-clip",
                            "Clip",
                            scope == SysExScope::Clip,
                            FbSegment::First,
                            move |_, _, cx| {
                                let _ = clip_target
                                    .update(cx, |view, cx| view.set_scope(SysExScope::Clip, cx));
                            },
                        ))
                        .child(fb_segment(
                            "sysex-scope-markers",
                            "Markers",
                            scope == SysExScope::Markers,
                            FbSegment::Last,
                            move |_, _, cx| {
                                let _ = marker_target
                                    .update(cx, |view, cx| view.set_scope(SysExScope::Markers, cx));
                            },
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(if has_target {
                            Colors::text_secondary()
                        } else {
                            Colors::text_muted()
                        })
                        .child(target_label),
                )
                .child(quiet(format!("Playhead {playhead_label}")))
        };

        // ── List ─────────────────────────────────────────────────────────
        let row_count = rows.len();
        let list_rows = std::sync::Arc::new(rows);
        let list_positions = std::sync::Arc::new(positions);
        let selected_key = self.selected.clone();
        let list_entity = entity.clone();
        let list = uniform_list("sysex-list", row_count, move |range, _window, _cx| {
            range
                .map(|index| {
                    let row = &list_rows[index];
                    let key = row.key.clone();
                    let beat = row.beat;
                    let selected = selected_key.as_ref() == Some(&row.key);
                    let target = list_entity.clone();
                    sysex_row(row, &list_positions[index], selected).on_mouse_down(
                        gpui::MouseButton::Left,
                        move |mouse, _, cx| {
                            cx.stop_propagation();
                            let seek = (mouse.click_count >= 2).then_some(beat);
                            let key = key.clone();
                            let _ = target.update(cx, |view, cx| view.select(key, seek, cx));
                        },
                    )
                })
                .collect()
        })
        .size_full()
        .track_scroll(&self.list_scroll);
        let empty_message = match (scope, has_target) {
            (SysExScope::Clip, false) => "Select a MIDI clip to see and add its SysEx.",
            (SysExScope::Clip, true) => {
                "No SysEx in this clip. Pick a template below, then Add at Playhead."
            }
            (SysExScope::Markers, _) => {
                "No marker carries SysEx. Add puts it on the marker at the playhead, or places one."
            }
        };

        // ── Templates ────────────────────────────────────────────────────
        let vendor_tabs = Vendor::ALL.iter().enumerate().fold(
            fb_segmented_track().flex_shrink_0(),
            |track, (i, v)| {
                let v = *v;
                let target = entity.clone();
                let position = match i {
                    0 => FbSegment::First,
                    i if i == Vendor::ALL.len() - 1 => FbSegment::Last,
                    _ => FbSegment::Middle,
                };
                track.child(fb_segment(
                    ("sysex-vendor", i),
                    v.label(),
                    vendor == v,
                    position,
                    move |_, _, cx| {
                        let _ = target.update(cx, |view, cx| {
                            view.vendor = v;
                            cx.notify();
                        });
                    },
                ))
            },
        );
        let template_chips = sysex::TEMPLATES
            .iter()
            .enumerate()
            .filter(|(_, t)| t.vendor == vendor)
            .fold(
                div().flex().flex_wrap().gap(px(space::TIGHT)),
                |chips, (i, template)| {
                    let target = entity.clone();
                    let bytes = template.bytes;
                    chips.child(
                        div()
                            .id(("sysex-template", i))
                            .h(px(fb_size::DENSE))
                            .px(px(space::BASE))
                            .flex()
                            .items_center()
                            .rounded(px(crate::theme::radius::CONTROL_SM))
                            .border(px(1.0))
                            .border_color(Colors::border_subtle())
                            .bg(Colors::surface_input())
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_secondary())
                            .cursor(gpui::CursorStyle::PointingHand)
                            .hover(|s| s.bg(Colors::surface_hover()))
                            .child(template.label)
                            .on_click(move |_, _, cx| {
                                let _ = target.update(cx, |view, cx| view.set_field(bytes, cx));
                            }),
                    )
                },
            );

        // ── Message editor ───────────────────────────────────────────────
        let fix_target = entity.clone();
        let dev_down = entity.clone();
        let dev_up = entity.clone();
        let add_target = entity.clone();
        let apply_target = entity.clone();
        let move_target = entity.clone();
        let delete_target = entity.clone();
        let checksum_bad = matches!(draft_checksum, Checksum::Invalid { .. });
        let checksum_line = match draft_checksum {
            Checksum::None => None,
            Checksum::Valid => Some(("Checksum OK".to_string(), Colors::status_success())),
            Checksum::Invalid { found, expected } => Some((
                format!("Checksum {found:02X}h, should be {expected:02X}h"),
                Colors::status_error(),
            )),
        };

        let editor = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap(px(space::SNUG))
            .p(px(space::BASE))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(caption("TEMPLATES"))
                    .child(vendor_tabs),
            )
            .child(template_chips)
            .child(caption("MESSAGE (HEX)"))
            .child(text_field_with_callbacks(
                &self.hex_input,
                self.hex_input.is_focused(window),
                hex_callbacks,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(if draft_ok {
                                Colors::text_secondary()
                            } else {
                                Colors::status_error()
                            })
                            .child(draft_line),
                    )
                    .when_some(checksum_line, |this, (text, tone)| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(typography::UI_XS))
                                .text_color(tone)
                                .child(text),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(space::SNUG))
                    .when_some(draft_device, |this, device| {
                        this.child(quiet("Device"))
                            .child(fb_stepper_button(
                                "sysex-device-down",
                                "-",
                                move |_, _, cx| {
                                    let _ =
                                        dev_down.update(cx, |view, cx| view.step_device(-1, cx));
                                },
                            ))
                            .child(
                                div()
                                    .w(px(34.0))
                                    .text_size(px(typography::UI_SM))
                                    .text_color(Colors::text_primary())
                                    .flex()
                                    .justify_center()
                                    .child(format!("{device:02X}h")),
                            )
                            .child(fb_stepper_button(
                                "sysex-device-up",
                                "+",
                                move |_, _, cx| {
                                    let _ = dev_up.update(cx, |view, cx| view.step_device(1, cx));
                                },
                            ))
                    })
                    .child(fb_button(
                        "sysex-fix-checksum",
                        "Fix Checksum",
                        FbButtonKind::Default,
                        checksum_bad,
                        move |_, _, cx| {
                            let _ = fix_target.update(cx, |view, cx| view.fix_checksum(cx));
                        },
                    ))
                    .child(div().flex_1())
                    .child(fb_button(
                        "sysex-delete",
                        "Delete",
                        FbButtonKind::Default,
                        has_selection,
                        move |_, _, cx| {
                            let _ = delete_target.update(cx, |view, cx| view.delete_selected(cx));
                        },
                    ))
                    .when(scope == SysExScope::Clip, |this| {
                        this.child(fb_button(
                            "sysex-move",
                            "Move to Playhead",
                            FbButtonKind::Default,
                            selected_is_clip,
                            move |_, _, cx| {
                                let _ = move_target
                                    .update(cx, |view, cx| view.move_selected_to_playhead(cx));
                            },
                        ))
                    })
                    .child(fb_button(
                        "sysex-apply",
                        "Apply",
                        FbButtonKind::Default,
                        can_write && has_selection,
                        move |_, _, cx| {
                            let _ = apply_target.update(cx, |view, cx| view.apply(cx));
                        },
                    ))
                    .child(fb_button(
                        "sysex-add",
                        "Add at Playhead",
                        FbButtonKind::Primary,
                        can_write,
                        move |_, _, cx| {
                            let _ = add_target.update(cx, |view, cx| view.add(cx));
                        },
                    )),
            );

        let footer = div()
            .flex_shrink_0()
            .px(px(space::BASE))
            .py(px(space::TIGHT))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(typography::DENSE_CAPTION))
            .text_color(Colors::text_faint())
            .child(match scope {
                SysExScope::Clip => {
                    "Played to the track's hardware MIDI output · written to MIDI export · plug-in instruments do not receive SysEx"
                }
                SysExScope::Markers => {
                    "Played to every hardware MIDI output a MIDI track uses · written to the MIDI export conductor track"
                }
            });

        div()
            .id("sysex-editor")
            .flex()
            .flex_col()
            .size_full()
            .min_w(px(420.0))
            .overflow_hidden()
            .bg(Colors::surface_base())
            .capture_key_down(move |event, window, cx| {
                let handled = entity.update(cx, |view, cx| view.handle_key(event, window, cx));
                if handled {
                    cx.stop_propagation();
                }
            })
            .child(scope_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .children((row_count > 0).then_some(list))
                    .children((row_count == 0).then(|| {
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .min_w_0()
                            .child(inspector_kit::ins_empty(
                                crate::assets::ICON_LIST_MUSIC_PATH,
                                "No SysEx",
                                empty_message,
                            ))
                    })),
            )
            .child(editor)
            .child(footer)
    }
}

pub struct SysExEditorWindow {
    view: Entity<SysExEditorView>,
    on_close: std::sync::Arc<dyn Fn(&mut App) + Send + Sync>,
}

impl Render for SysExEditorWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let on_close = self.on_close.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(Colors::surface_window())
            .child(crate::components::title_bar::external_window_titlebar(
                TITLE,
                "sysex-editor-close",
                move |window, cx| {
                    on_close(cx);
                    window.remove_window();
                },
            ))
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}

pub fn open_sysex_editor_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    timeline: Entity<Timeline>,
    on_close: std::sync::Arc<dyn Fn(&mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<SysExEditorWindow>, String> {
    let window_bounds = centered_window_bounds(owner_bounds, size(px(760.0), px(540.0)), cx);
    let mut options = crate::platform_chrome::external_window_options_partial();
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some(crate::platform_chrome::branded_window_title(TITLE).into());
    }
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(px(480.0), px(380.0)));
    apply_owner_display(&mut options, owner_bounds, cx);
    cx.open_window(options, move |_window, cx| {
        let view = cx.new(|cx| SysExEditorView::new(timeline, cx));
        cx.new(|_| SysExEditorWindow { view, on_close })
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_only_writes_framed_messages() {
        assert!(Draft::parse("").writable().is_none());
        assert!(Draft::parse("F0 41 zz").writable().is_none());
        assert!(Draft::parse("F0 41 10").writable().is_none());
        assert_eq!(
            Draft::parse("F0 7E 7F 09 01 F7").writable(),
            Some(&[0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7][..])
        );
    }
}
