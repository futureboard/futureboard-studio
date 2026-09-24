//! Dropping a plug-in from the Browser onto the arrangement.
//!
//! The rule the user sees: a **track header** means "put it on this track"; the
//! **timeline** (a lane, a clip, the empty space below the tracks) means "give
//! me a new track with it". The drag-over hint and the drop both resolve
//! through [`resolve_plugin_drop`], so what the hint promises is what the drop
//! does.

use super::*;
use crate::components::plugin_picker::{PluginInsertKind, PluginInsertTarget};

/// What kind of track a timeline drop creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTrackKind {
    /// An instrument gets a MIDI track to play it.
    Midi,
    /// An effect gets an audio track to process.
    Audio,
}

impl NewTrackKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Midi => "New MIDI Track",
            Self::Audio => "New Audio Track",
        }
    }
}

/// Where a dropped plug-in goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginDropTarget {
    /// Onto this existing track.
    Track { track_id: String },
    /// Onto a new track created at `insert_index` in the track list.
    NewTrack {
        insert_index: usize,
        kind: NewTrackKind,
    },
    /// Nowhere: the header under the pointer cannot host this plug-in.
    Refused { reason: &'static str },
}

/// Resolve the drop target for a plug-in released over `target`.
pub fn resolve_plugin_drop(
    state: &TimelineState,
    target: &TimelineContextTarget,
    instrument: bool,
) -> PluginDropTarget {
    let kind = if instrument {
        NewTrackKind::Midi
    } else {
        NewTrackKind::Audio
    };
    let new_below = |track_id: &str| PluginDropTarget::NewTrack {
        insert_index: insert_index_below(state, track_id),
        kind,
    };
    let at_end = PluginDropTarget::NewTrack {
        insert_index: state.tracks.len(),
        kind,
    };
    match target {
        TimelineContextTarget::TrackHeader(track_id) => {
            let Some(track) = state.find_track(track_id) else {
                return at_end;
            };
            let host = PluginInsertTarget {
                track_id: track.id.clone(),
                track_name: track.name.clone(),
                track_type: track.track_type,
                next_slot_index: 0,
                desired_kind: if instrument {
                    PluginInsertKind::Instrument
                } else {
                    PluginInsertKind::Effect
                },
            };
            if instrument {
                // An instrument on a track that cannot play one gets the MIDI
                // track it needs, right below — refusing the drop would leave
                // the user to find out why nothing happened.
                if host.accepts_instrument() {
                    PluginDropTarget::Track {
                        track_id: track_id.clone(),
                    }
                } else {
                    new_below(track_id)
                }
            } else if host.accepts_effect() {
                PluginDropTarget::Track {
                    track_id: track_id.clone(),
                }
            } else {
                PluginDropTarget::Refused {
                    reason: "This track can't host an effect",
                }
            }
        }
        TimelineContextTarget::TrackLane { track_id, .. }
        | TimelineContextTarget::AudioClip { track_id, .. }
        | TimelineContextTarget::MidiClip { track_id, .. }
        | TimelineContextTarget::AutomationLane { track_id, .. } => new_below(track_id),
        TimelineContextTarget::Clip(clip_id) => match state.find_clip(clip_id) {
            Some((track, _)) => new_below(&track.id),
            None => at_end,
        },
        _ => at_end,
    }
}

/// Index just below `track_id` — below its whole group when it is a group or
/// sits in one, so a new track never lands inside a group it does not belong
/// to.
fn insert_index_below(state: &TimelineState, track_id: &str) -> usize {
    let Some(index) = state.tracks.iter().position(|track| track.id == track_id) else {
        return state.tracks.len();
    };
    let track = &state.tracks[index];
    let group = if track.track_type == TrackType::Group {
        Some(track.id.as_str())
    } else {
        track.parent_group_id.as_deref()
    };
    match group {
        Some(group) => state
            .tracks
            .iter()
            .rposition(|t| t.id == group || t.parent_group_id.as_deref() == Some(group))
            .map_or(index + 1, |last| last + 1),
        None => index + 1,
    }
}

/// The live drag-over state: where the pointer is and what a release there
/// would do.
#[derive(Debug, Clone)]
pub(crate) struct PluginDropHint {
    pub target: PluginDropTarget,
    pub plugin_name: String,
}

/// The hint drawn while a plug-in is dragged over the arrangement: the
/// receiving track lit, or an insertion line where the new track will go.
/// In track-area coordinates (y = 0 at the first track row, headers
/// included).
pub(crate) fn plugin_drop_hint_overlay(
    hint: &PluginDropHint,
    state: &TimelineState,
) -> Option<gpui::AnyElement> {
    let layout = state.track_row_layout();
    let scroll_y = state.viewport.scroll_y;
    let accent = Colors::accent_primary();
    let chip = |text: String, tone: gpui::Rgba| {
        div()
            .absolute()
            .left(px(crate::theme::space::BASE))
            .px(px(crate::theme::space::BASE))
            .py(px(crate::theme::space::TIGHT))
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::with_alpha(tone, 0.62))
            .bg(Colors::with_alpha(Colors::surface_panel(), 0.96))
            .shadow_lg()
            .text_size(px(crate::theme::typography::UI_XS))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_primary())
            .whitespace_nowrap()
            .child(text)
    };

    let element = match &hint.target {
        PluginDropTarget::Track { track_id } => {
            let index = state.tracks.iter().position(|t| &t.id == track_id)?;
            let row = layout.row_for_index(index)?;
            let top = row.y - scroll_y;
            let name = state.tracks[index].name.clone();
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(top))
                .h(px(row.height))
                .bg(Colors::with_alpha(accent, 0.10))
                .border_t(px(1.0))
                .border_b(px(1.0))
                .border_color(Colors::with_alpha(accent, 0.55))
                .child(
                    chip(format!("Add {} to {}", hint.plugin_name, name), accent)
                        .top(px(crate::theme::space::TIGHT)),
                )
                .into_any_element()
        }
        PluginDropTarget::NewTrack { insert_index, kind } => {
            let y = match insert_index.checked_sub(1) {
                Some(above) => layout
                    .row_for_index(above)
                    .map(|row| row.y + row.height)
                    .unwrap_or_else(|| state.total_track_rows_height()),
                None => 0.0,
            } - scroll_y;
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px((y - 1.0).max(0.0)))
                .h(px(2.0))
                .bg(accent)
                .child(
                    chip(format!("{} · {}", kind.label(), hint.plugin_name), accent)
                        .top(px(crate::theme::space::SNUG)),
                )
                .into_any_element()
        }
        PluginDropTarget::Refused { reason } => {
            let error = Colors::status_error();
            chip(format!("{} — {reason}", hint.plugin_name), error)
                .top(px(crate::theme::space::BASE))
                .into_any_element()
        }
    };
    Some(element)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{CreateTrackOptions, InputMonitorMode};

    fn add(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: Colors::accent_primary(),
            volume: 0.8,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    fn setup() -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let audio = add(&mut state, TrackType::Audio, "Vox");
        let midi = add(&mut state, TrackType::Midi, "Keys");
        (state, audio, midi)
    }

    #[test]
    fn a_header_takes_the_plugin_the_timeline_makes_a_new_track() {
        let (state, audio, _) = setup();
        assert_eq!(
            resolve_plugin_drop(
                &state,
                &TimelineContextTarget::TrackHeader(audio.clone()),
                false
            ),
            PluginDropTarget::Track {
                track_id: audio.clone()
            }
        );
        assert_eq!(
            resolve_plugin_drop(
                &state,
                &TimelineContextTarget::TrackLane {
                    track_id: audio,
                    beat: 4.0
                },
                false
            ),
            PluginDropTarget::NewTrack {
                insert_index: 1,
                kind: NewTrackKind::Audio
            }
        );
        assert_eq!(
            resolve_plugin_drop(&state, &TimelineContextTarget::TimelineEmpty, true),
            PluginDropTarget::NewTrack {
                insert_index: 2,
                kind: NewTrackKind::Midi
            }
        );
    }

    #[test]
    fn an_instrument_on_an_audio_header_gets_a_midi_track_below_it() {
        let (state, audio, midi) = setup();
        assert_eq!(
            resolve_plugin_drop(&state, &TimelineContextTarget::TrackHeader(audio), true),
            PluginDropTarget::NewTrack {
                insert_index: 1,
                kind: NewTrackKind::Midi
            }
        );
        assert_eq!(
            resolve_plugin_drop(
                &state,
                &TimelineContextTarget::TrackHeader(midi.clone()),
                true
            ),
            PluginDropTarget::Track { track_id: midi }
        );
    }

    #[test]
    fn a_new_track_lands_below_the_whole_group() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let group = add(&mut state, TrackType::Group, "Drums");
        let kick = add(&mut state, TrackType::Audio, "Kick");
        let snare = add(&mut state, TrackType::Audio, "Snare");
        add(&mut state, TrackType::Audio, "Bass");
        for id in [&kick, &snare] {
            let track = state.tracks.iter_mut().find(|t| &t.id == id).unwrap();
            track.parent_group_id = Some(group.clone());
        }
        assert_eq!(insert_index_below(&state, &kick), 3);
        assert_eq!(insert_index_below(&state, &group), 3);
    }
}
