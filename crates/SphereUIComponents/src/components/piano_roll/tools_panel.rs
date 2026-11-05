//! The MIDI editor's tools column, on the left of the keys.
//!
//! Three pages instead of a stack of collapsible sections: **Timing**
//! (quantize, length), **Pitch** (transpose, scale, chords) and
//! **Perform** (articulation, note expression). A page shows its controls
//! flat, settings are picked from segmented choices rather than stepped, and
//! each group ends in the button that applies it.
//!
//! Every action works on the selected notes, or on every shown note of the
//! edited clip when nothing is selected — the header says which — and lands
//! as one undoable `EditMidiNotes` command through
//! [`PianoRoll::commit_note_transform`]. The transforms are the pure functions
//! in `timeline::state::midi_transform`; this module gathers their settings,
//! resolves the target notes and draws the column.
//!
//! The column's settings are editor view state like the grid resolution: per
//! editor instance, not saved with the project.

use super::*;
use crate::components::controls::FbSegment;
use crate::components::timeline::timeline_state::{
    conform_to_chords, delete_overlaps, invert_pitches, legato_notes, quantize_notes,
    retrograde_notes, scale_note_lengths, set_note_lengths, transpose_in_scale, voice_chords,
    ChordVoicing, MidiScale, QuantizeSpec, QuantizeTarget,
};
use sphere_midi_service::{ExpressionCurve, ExpressionPoint};

/// Width of the tools column.
pub(super) const TOOLS_W: f32 = 232.0;
/// Height of the Chord Track strip under the ruler.
pub(super) const CHORD_STRIP_H: f32 = 20.0;
/// Label column of a labelled choice row.
const LABEL_W: f32 = 52.0;

/// The column's pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolTab {
    Timing,
    Pitch,
    Perform,
}

impl ToolTab {
    const ALL: [Self; 3] = [Self::Timing, Self::Pitch, Self::Perform];

    fn label(self) -> &'static str {
        match self {
            Self::Timing => "Timing",
            Self::Pitch => "Pitch",
            Self::Perform => "Perform",
        }
    }
}

/// Grid choices for Quantize and Length; triplets are a checkbox on top.
const GRID_CHOICES: [(GridRes, &str); 4] = [
    (GridRes::Quarter, "1/4"),
    (GridRes::Eighth, "1/8"),
    (GridRes::Sixteenth, "1/16"),
    (GridRes::ThirtySecond, "1/32"),
];

/// A grid split into its straight value and whether it is the triplet of it.
fn split_grid(grid: GridRes) -> (GridRes, bool) {
    match grid {
        GridRes::QuarterTriplet => (GridRes::Quarter, true),
        GridRes::EighthTriplet => (GridRes::Eighth, true),
        GridRes::SixteenthTriplet => (GridRes::Sixteenth, true),
        other => (other, false),
    }
}

/// The inverse of [`split_grid`]. A value with no triplet (1/32) stays
/// straight.
fn join_grid(base: GridRes, triplet: bool) -> GridRes {
    match (base, triplet) {
        (GridRes::Quarter, true) => GridRes::QuarterTriplet,
        (GridRes::Eighth, true) => GridRes::EighthTriplet,
        (GridRes::Sixteenth, true) => GridRes::SixteenthTriplet,
        (other, _) => other,
    }
}

fn has_triplet(base: GridRes) -> bool {
    join_grid(base, true) != base
}

/// Shape written by a Note Expression preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExpressionShape {
    FadeIn,
    FadeOut,
    Swell,
    Clear,
}

impl ExpressionShape {
    fn curve(self, duration: f32) -> ExpressionCurve {
        let d = duration.max(MIN_NOTE_BEATS);
        let points = match self {
            Self::FadeIn => vec![ExpressionPoint::new(0.0, 0.0), ExpressionPoint::new(d, 1.0)],
            Self::FadeOut => vec![ExpressionPoint::new(0.0, 1.0), ExpressionPoint::new(d, 0.0)],
            Self::Swell => vec![
                ExpressionPoint::new(0.0, 0.2),
                ExpressionPoint::new(d * 0.5, 1.0),
                ExpressionPoint::new(d, 0.2),
            ],
            Self::Clear => Vec::new(),
        };
        ExpressionCurve::from_points(points)
    }
}

/// Settings and open state of the tools column.
pub(super) struct ToolsPanelState {
    pub(super) open: bool,
    tab: ToolTab,
    /// Draw the project's Chord Track as a strip under the editor's ruler.
    pub(super) show_chord_strip: bool,
    /// Quantize strength, percent.
    pub(super) quantize_strength: u8,
    /// Off-beat swing, percent (100 = triplet feel).
    pub(super) quantize_swing: u8,
    /// ±1 moves by scale degrees instead of semitones (when a scale is set).
    pub(super) transpose_in_scale: bool,
    pub(super) length_res: GridRes,
    /// The expression lane the shape buttons write.
    expression_lane: NoteExpressionLane,
}

impl Default for ToolsPanelState {
    fn default() -> Self {
        Self {
            open: false,
            tab: ToolTab::Timing,
            show_chord_strip: false,
            quantize_strength: 100,
            quantize_swing: 0,
            transpose_in_scale: false,
            length_res: GridRes::Eighth,
            expression_lane: NoteExpressionLane::Pressure,
        }
    }
}

impl PianoRoll {
    /// Show or hide the tools column (the floating editor opens with it).
    pub fn set_tools_panel_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.tools.open = open;
        cx.notify();
    }

    /// Preview scenes: pick the page (0 Timing, 1 Pitch, 2 Perform) and show
    /// or hide the Chord Track strip.
    #[cfg(feature = "ui-preview")]
    pub fn preview_tools(&mut self, tab: usize, chord_strip: bool) {
        self.tools.tab = ToolTab::ALL[tab.min(ToolTab::ALL.len() - 1)];
        self.tools.show_chord_strip = chord_strip;
    }

    /// The quantize pass the column's settings describe; `Q` and the toolbar
    /// button use it too, so every quantize in the editor agrees.
    pub(super) fn quantize_spec(&self) -> QuantizeSpec {
        QuantizeSpec {
            step: self.quantize_res.beats().max(MIN_NOTE_BEATS),
            strength: self.tools.quantize_strength as f32 / 100.0,
            swing: self.tools.quantize_swing as f32 / 100.0,
        }
    }

    /// Notes a tool acts on: the selection, or every note of the edited clip
    /// on a shown channel when nothing is selected.
    pub(super) fn tool_target_ids(&self, cx: &gpui::App) -> Vec<u64> {
        if !self.selection.is_empty() {
            return self.selected_note_ids();
        }
        let Some(clip_id) = self.editing_clip_id_in(cx) else {
            return Vec::new();
        };
        self.timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| self.channel_visible(n.channel))
                    .map(|n| n.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Run one transform over the target notes as a single undoable edit.
    ///
    /// `label` names the step in Edit > Undo / Redo ("Undo Strum"). The clip
    /// grows to fit if the tool pushes notes past its end, and the step
    /// carries the clip's length on both sides so undo shrinks it back.
    pub(super) fn run_note_tool<F>(
        &mut self,
        label: &'static str,
        cx: &mut Context<Self>,
        transform: F,
    ) where
        F: FnOnce(&mut [MidiNoteState], &HashSet<u64>) -> usize,
    {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids = self.tool_target_ids(cx);
        if ids.is_empty() {
            return;
        }
        let set: HashSet<u64> = ids.iter().copied().collect();
        let prev = self.snapshot_notes(cx, &clip_id, &ids);
        let prev_len = self.timeline.read(cx).state.clip_duration_beats(&clip_id);
        self.timeline.update(cx, |tl, tcx| {
            if let Some(notes) = tl.state.midi_clip_notes_mut(&clip_id) {
                transform(notes, &set);
            }
            tl.state.expand_clip_to_contain_notes(&clip_id);
            tcx.notify();
        });
        let next = self.snapshot_notes(cx, &clip_id, &ids);
        let next_len = self.timeline.read(cx).state.clip_duration_beats(&clip_id);
        self.record_note_transform(label, clip_id, prev, next, prev_len, next_len, cx);
        cx.notify();
    }

    /// Record an already-applied tool run as one named undo step. Dropped
    /// when nothing changed, so a tool that found nothing to do leaves no
    /// empty step in the history and the project is not marked unsaved.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_note_transform(
        &mut self,
        label: &'static str,
        clip_id: String,
        prev: Vec<MidiNoteState>,
        next: Vec<MidiNoteState>,
        prev_clip_beats: Option<f32>,
        next_clip_beats: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        let length_changed = match (prev_clip_beats, next_clip_beats) {
            (Some(a), Some(b)) => (a - b).abs() > 1.0e-4,
            _ => false,
        };
        if prev == next && !length_changed {
            return;
        }
        self.timeline.update(cx, |tl, tcx| {
            tl.record_executed_command(
                EditCommand::TransformMidiNotes {
                    label,
                    clip_id,
                    prev,
                    next,
                    prev_clip_beats,
                    next_clip_beats,
                },
                tcx,
            );
        });
    }

    pub(super) fn tool_quantize(&mut self, target: QuantizeTarget, cx: &mut Context<Self>) {
        let spec = self.quantize_spec();
        let label = match target {
            QuantizeTarget::Start => "Quantize",
            QuantizeTarget::End => "Quantize Ends",
            QuantizeTarget::Length => "Quantize Lengths",
        };
        self.run_note_tool(label, cx, move |notes, ids| {
            quantize_notes(notes, ids, spec, target)
        });
    }

    /// `steps` semitones, or scale degrees when "Stay in scale" is on and a
    /// scale is set. Octaves (`±12`) are always semitones.
    fn tool_transpose(&mut self, steps: i32, cx: &mut Context<Self>) {
        let scale = self.pitch_ctx.scale;
        let in_scale = self.tools.transpose_in_scale
            && scale.kind != ScaleKind::Chromatic
            && steps.abs() != 12;
        let scale = if in_scale {
            scale
        } else {
            MidiScale {
                root: scale.root,
                kind: ScaleKind::Chromatic,
            }
        };
        let label = if steps.abs() == 12 {
            if steps > 0 {
                "Octave Up"
            } else {
                "Octave Down"
            }
        } else {
            "Transpose"
        };
        self.run_note_tool(label, cx, move |notes, ids| {
            transpose_in_scale(notes, ids, scale, steps)
        });
    }

    /// The Chord Track's chords as clip-local spans with their pitch classes.
    fn chord_track_spans(&self, cx: &Context<Self>) -> Vec<(f32, f32, Vec<u8>)> {
        let origin = self.edit_origin();
        self.timeline
            .read(cx)
            .state
            .chord_events
            .iter()
            .map(|event| {
                (
                    event.start_beat as f32 - origin,
                    event.end_beat() as f32 - origin,
                    event.chord.pitch_classes(),
                )
            })
            .collect()
    }

    fn tool_conform_to_chords(&mut self, cx: &mut Context<Self>) {
        let spans = self.chord_track_spans(cx);
        if spans.is_empty() {
            return;
        }
        self.run_note_tool("Fit Notes to Chords", cx, move |notes, ids| {
            conform_to_chords(notes, ids, |beat| {
                spans
                    .iter()
                    .find(|(start, end, _)| beat >= *start - 1.0e-4 && beat < *end)
                    .map(|(_, _, classes)| classes.clone())
            })
        });
    }

    fn tool_expression_shape(
        &mut self,
        lane: NoteExpressionLane,
        shape: ExpressionShape,
        cx: &mut Context<Self>,
    ) {
        if !crate::edition::professional_features_available() {
            return;
        }
        let label = if shape == ExpressionShape::Clear {
            "Clear Note Expression"
        } else {
            "Shape Note Expression"
        };
        self.run_note_tool(label, cx, move |notes, ids| {
            let mut changed = 0;
            for note in notes.iter_mut().filter(|n| ids.contains(&n.id)) {
                let curve = shape.curve(note.duration);
                let slot = match lane {
                    NoteExpressionLane::Pitch => &mut note.expression.pitch,
                    NoteExpressionLane::Pressure => &mut note.expression.pressure,
                    NoteExpressionLane::Timbre => &mut note.expression.timbre,
                };
                if *slot != curve {
                    *slot = curve;
                    changed += 1;
                }
            }
            changed
        });
    }

    fn step_scale_root(&mut self, dir: i32, cx: &mut Context<Self>) {
        let pc = self.pitch_ctx.scale.root.pitch_class() as i32;
        self.pitch_ctx.scale.root = ScaleRoot::from_pitch_class((pc + dir).rem_euclid(12) as u8);
        cx.notify();
    }

    fn step_scale_kind(&mut self, dir: i32, cx: &mut Context<Self>) {
        let all = ScaleKind::ALL;
        let idx = all
            .iter()
            .position(|k| *k == self.pitch_ctx.scale.kind)
            .unwrap_or(0) as i32;
        let next = all[(idx + dir).rem_euclid(all.len() as i32) as usize];
        // Same rule as the toolbar's scale menu: picking a real scale also
        // constrains note gestures to it, Chromatic releases them.
        self.pitch_ctx.scale.kind = next;
        self.pitch_ctx.constrain = next != ScaleKind::Chromatic;
        cx.notify();
    }

    // ── Rendering ──────────────────────────────────────────────────────────

    /// The tools column, left of the piano keys.
    pub(super) fn render_tools_panel(
        &self,
        cx: &mut Context<Self>,
        _clip_id: &str,
    ) -> impl IntoElement {
        use crate::theme::{size, space, typography};
        let targets = self.tool_target_ids(cx).len();
        let enabled = targets > 0;

        // Header: what the buttons below will act on, said before the click.
        let scope: gpui::AnyElement = if !self.selection.is_empty() {
            crate::components::controls::fb_badge(
                format!("{} selected", group_thousands(targets)),
                Colors::accent_primary(),
            )
            .into_any_element()
        } else {
            div()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(if enabled {
                    format!("All {} notes", group_thousands(targets))
                } else {
                    "No notes yet".to_string()
                })
                .into_any_element()
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(space::SNUG))
            .h(px(size::DEFAULT))
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child("Tools"),
            )
            .child(scope);

        let tab = self.tools.tab;
        let mut tabs = compact_track();
        for (i, page) in ToolTab::ALL.iter().copied().enumerate() {
            tabs = tabs.child(compact_segment(
                ("pr-tools-tab", i),
                page.label(),
                page == tab,
                segment_position(i, ToolTab::ALL.len()),
                cx.listener(move |this, _, _w, cx| {
                    this.tools.tab = page;
                    cx.notify();
                }),
            ));
        }

        let body = match tab {
            ToolTab::Timing => self.timing_page(enabled, cx),
            ToolTab::Pitch => self.pitch_page(enabled, cx),
            ToolTab::Perform => self.perform_page(enabled, cx),
        };

        div()
            .id("pr-midi-tools")
            .w(px(TOOLS_W))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .border_r(px(1.0))
            .border_color(Colors::panel_border())
            .bg(Colors::surface_panel())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::SNUG))
                    .px(px(space::BASE))
                    .pt(px(space::BASE))
                    .pb(px(space::SNUG))
                    .child(header)
                    .child(tabs),
            )
            .child(
                div()
                    .id("pr-midi-tools-page")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .px(px(space::BASE))
                    .pb(px(space::BASE))
                    .children(body),
            )
    }

    fn timing_page(&self, enabled: bool, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let t = &self.tools;
        let (q_base, q_triplet) = split_grid(self.quantize_res);
        vec![
            insp_section("QUANTIZE").into_any_element(),
            choice_track(
                "pr-tools-q-grid",
                &GRID_CHOICES,
                q_base,
                cx,
                move |this, base| {
                    let triplet = split_grid(this.quantize_res).1 && has_triplet(base);
                    this.quantize_res = join_grid(base, triplet);
                },
            ),
            compact_check(
                "pr-tools-q-triplet",
                "Triplet grid",
                q_triplet,
                has_triplet(q_base),
                cx.listener(|this, _, _w, cx| {
                    let (base, triplet) = split_grid(this.quantize_res);
                    this.quantize_res = join_grid(base, !triplet);
                    cx.notify();
                }),
            )
            .into_any_element(),
            labeled(
                "Strength",
                choice_track(
                    "pr-tools-q-strength",
                    &[(50u8, "50"), (75, "75"), (100, "100")],
                    t.quantize_strength,
                    cx,
                    |this, value| this.tools.quantize_strength = value,
                ),
            ),
            labeled(
                "Swing",
                choice_track(
                    "pr-tools-q-swing",
                    &[(0u8, "Off"), (33, "33"), (66, "66")],
                    t.quantize_swing,
                    cx,
                    |this, value| this.tools.quantize_swing = value,
                ),
            ),
            tool_row(vec![tool_button(
                "pr-tools-q-start",
                "Quantize",
                Emphasis::Primary,
                enabled,
                cx.listener(|this, _, _w, cx| this.tool_quantize(QuantizeTarget::Start, cx)),
            )
            .on_hover(cx.listener(|this, hovered: &bool, _w, cx| {
                this.quantize_preview = *hovered;
                cx.notify();
            }))
            .into_any_element()]),
            tool_row(vec![
                tool_button(
                    "pr-tools-q-end",
                    "Ends",
                    Emphasis::Normal,
                    enabled,
                    cx.listener(|this, _, _w, cx| this.tool_quantize(QuantizeTarget::End, cx)),
                )
                .into_any_element(),
                tool_button(
                    "pr-tools-q-length",
                    "Lengths",
                    Emphasis::Normal,
                    enabled,
                    cx.listener(|this, _, _w, cx| this.tool_quantize(QuantizeTarget::Length, cx)),
                )
                .into_any_element(),
            ]),
            tool_caption("Q quantizes with these settings too. Hover Quantize to preview."),
            insp_section("LENGTH").into_any_element(),
            choice_track(
                "pr-tools-len-grid",
                &GRID_CHOICES,
                t.length_res,
                cx,
                |this, value| this.tools.length_res = value,
            ),
            tool_row(vec![
                tool_button(
                    "pr-tools-len-fixed",
                    "Set Length",
                    Emphasis::Normal,
                    enabled,
                    cx.listener(|this, _, _w, cx| {
                        let length = this.tools.length_res.beats().max(MIN_NOTE_BEATS);
                        this.run_note_tool("Set Length", cx, move |notes, ids| {
                            set_note_lengths(notes, ids, length)
                        })
                    }),
                )
                .into_any_element(),
                tool_button(
                    "pr-tools-len-legato",
                    "Legato",
                    Emphasis::Normal,
                    enabled,
                    cx.listener(|this, _, _w, cx| {
                        this.run_note_tool("Legato", cx, |notes, ids| legato_notes(notes, ids, 0.0))
                    }),
                )
                .into_any_element(),
            ]),
            tool_row(vec![
                tool_button(
                    "pr-tools-len-half",
                    "Half",
                    Emphasis::Normal,
                    enabled,
                    cx.listener(|this, _, _w, cx| {
                        this.run_note_tool("Halve Lengths", cx, |notes, ids| {
                            scale_note_lengths(notes, ids, 0.5)
                        })
                    }),
                )
                .into_any_element(),
                tool_button(
                    "pr-tools-len-double",
                    "Double",
                    Emphasis::Normal,
                    enabled,
                    cx.listener(|this, _, _w, cx| {
                        this.run_note_tool("Double Lengths", cx, |notes, ids| {
                            scale_note_lengths(notes, ids, 2.0)
                        })
                    }),
                )
                .into_any_element(),
            ]),
            tool_row(vec![tool_button(
                "pr-tools-len-overlaps",
                "Remove Overlaps",
                Emphasis::Normal,
                enabled,
                cx.listener(|this, _, _w, cx| {
                    this.run_note_tool("Remove Overlaps", cx, |notes, ids| {
                        delete_overlaps(notes, ids, true)
                    })
                }),
            )
            .into_any_element()]),
        ]
    }

    fn pitch_page(&self, enabled: bool, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let scale = self.pitch_ctx.scale;
        let has_scale = scale.kind != ScaleKind::Chromatic;
        let in_scale = self.tools.transpose_in_scale && has_scale;
        let has_chords = !self.timeline.read(cx).state.chord_events.is_empty();
        let step = |cx: &mut Context<Self>, id: &'static str, label: &'static str, steps: i32| {
            tool_button(
                id,
                label,
                Emphasis::Normal,
                enabled,
                cx.listener(move |this, _, _w, cx| this.tool_transpose(steps, cx)),
            )
            .into_any_element()
        };
        let voicing = |cx: &mut Context<Self>, id: &'static str, label, voicing: ChordVoicing| {
            tool_button(
                id,
                label,
                Emphasis::Normal,
                enabled,
                cx.listener(move |this, _, _w, cx| {
                    this.run_note_tool(voicing.label(), cx, move |notes, ids| {
                        voice_chords(notes, ids, voicing)
                    })
                }),
            )
            .into_any_element()
        };
        vec![
            insp_section("TRANSPOSE").into_any_element(),
            tool_row(vec![
                step(cx, "pr-tools-tr-oct-down", "−12", -12),
                step(cx, "pr-tools-tr-down", "−1", -1),
                step(cx, "pr-tools-tr-up", "+1", 1),
                step(cx, "pr-tools-tr-oct-up", "+12", 12),
            ]),
            compact_check(
                "pr-tools-tr-in-scale",
                "±1 moves by scale steps",
                in_scale,
                has_scale,
                cx.listener(|this, _, _w, cx| {
                    this.tools.transpose_in_scale = !this.tools.transpose_in_scale;
                    cx.notify();
                }),
            )
            .into_any_element(),
            tool_row(vec![
                tool_button(
                    "pr-tools-tr-invert",
                    "Mirror",
                    Emphasis::Normal,
                    enabled,
                    cx.listener(|this, _, _w, cx| {
                        this.run_note_tool("Mirror Pitches", cx, invert_pitches)
                    }),
                )
                .into_any_element(),
                tool_button(
                    "pr-tools-tr-reverse",
                    "Reverse",
                    Emphasis::Normal,
                    enabled,
                    cx.listener(|this, _, _w, cx| {
                        this.run_note_tool("Reverse Notes", cx, retrograde_notes)
                    }),
                )
                .into_any_element(),
            ]),
            insp_section("SCALE").into_any_element(),
            insp_row(
                "Root",
                scale.root.label().to_string(),
                Some(insp_stepper(
                    "pr-tools-scale-root",
                    cx.listener(|this, _, _w, cx| this.step_scale_root(-1, cx)),
                    cx.listener(|this, _, _w, cx| this.step_scale_root(1, cx)),
                )),
            )
            .into_any_element(),
            insp_row(
                "Scale",
                scale.kind.label().to_string(),
                Some(insp_stepper(
                    "pr-tools-scale-kind",
                    cx.listener(|this, _, _w, cx| this.step_scale_kind(-1, cx)),
                    cx.listener(|this, _, _w, cx| this.step_scale_kind(1, cx)),
                )),
            )
            .into_any_element(),
            compact_check(
                "pr-tools-scale-lock",
                "Keep drawn notes in scale",
                self.pitch_ctx.constrain && has_scale,
                has_scale,
                cx.listener(|this, _, _w, cx| {
                    if this.pitch_ctx.scale.kind != ScaleKind::Chromatic {
                        this.pitch_ctx.constrain = !this.pitch_ctx.constrain;
                        cx.notify();
                    }
                }),
            )
            .into_any_element(),
            tool_row(vec![tool_button(
                "pr-tools-scale-snap",
                "Snap to Scale",
                Emphasis::Normal,
                enabled && has_scale,
                cx.listener(|this, _, _w, cx| this.snap_selection_to_scale(cx)),
            )
            .into_any_element()]),
            insp_section("CHORDS").into_any_element(),
            labeled(
                "Draw",
                choice_track(
                    "pr-tools-chord-draw",
                    &[
                        (ChordInsertKind::Off, "Note"),
                        (ChordInsertKind::Triad, "Triad"),
                        (ChordInsertKind::Seventh, "7th"),
                    ],
                    self.chord_kind,
                    cx,
                    |this, value| this.chord_kind = value,
                ),
            ),
            tool_row(vec![
                voicing(
                    cx,
                    "pr-tools-chord-inv-down",
                    "Invert Down",
                    ChordVoicing::InvertDown,
                ),
                voicing(
                    cx,
                    "pr-tools-chord-inv-up",
                    "Invert Up",
                    ChordVoicing::InvertUp,
                ),
            ]),
            tool_row(vec![
                voicing(cx, "pr-tools-chord-drop2", "Drop 2", ChordVoicing::Drop2),
                voicing(cx, "pr-tools-chord-close", "Close", ChordVoicing::Close),
            ]),
            compact_check(
                "pr-tools-chord-strip",
                "Show Chord Track",
                self.tools.show_chord_strip,
                true,
                cx.listener(|this, _, _w, cx| {
                    this.tools.show_chord_strip = !this.tools.show_chord_strip;
                    cx.notify();
                }),
            )
            .into_any_element(),
            tool_row(vec![tool_button(
                "pr-tools-chord-conform",
                "Fit Notes to Chords",
                Emphasis::Normal,
                enabled && has_chords,
                cx.listener(|this, _, _w, cx| this.tool_conform_to_chords(cx)),
            )
            .into_any_element()]),
            tool_caption(if has_chords {
                "Moves each note to the nearest tone of the Chord Track chord at its start."
            } else {
                "Add chords to the Chord Track in the arrangement to fit notes to them."
            }),
        ]
    }

    fn perform_page(&self, enabled: bool, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut out = vec![insp_section("ARTICULATION").into_any_element()];
        if self.selection.is_empty() {
            out.push(tool_caption("Select notes to give them an articulation."));
        } else {
            out.push(self.articulation_assign_row(cx));
        }
        out.push(tool_row(vec![tool_button(
            "pr-tools-art-lane",
            "Show Articulation Lane",
            Emphasis::Normal,
            true,
            cx.listener(|this, _, _w, cx| this.set_lane(ControllerLaneKind::Articulations, cx)),
        )
        .into_any_element()]));

        out.push(insp_section("EXPRESSION").into_any_element());
        if !crate::edition::professional_features_available() {
            out.push(tool_caption(
                "Shaping note expression is part of the Professional edition. Existing curves still play.",
            ));
            return out;
        }
        let lane = self.tools.expression_lane;
        out.push(choice_track(
            "pr-tools-ne-lane",
            &[
                (NoteExpressionLane::Pressure, "Pressure"),
                (NoteExpressionLane::Timbre, "Timbre"),
            ],
            lane,
            cx,
            |this, value| this.tools.expression_lane = value,
        ));
        let shape = |cx: &mut Context<Self>,
                     id: &'static str,
                     label: &'static str,
                     shape: ExpressionShape| {
            tool_button(
                id,
                label,
                if shape == ExpressionShape::Clear {
                    Emphasis::Destructive
                } else {
                    Emphasis::Normal
                },
                enabled,
                cx.listener(move |this, _, _w, cx| {
                    let lane = this.tools.expression_lane;
                    this.tool_expression_shape(lane, shape, cx)
                }),
            )
            .into_any_element()
        };
        out.push(tool_row(vec![
            shape(cx, "pr-tools-ne-in", "Fade In", ExpressionShape::FadeIn),
            shape(cx, "pr-tools-ne-out", "Fade Out", ExpressionShape::FadeOut),
        ]));
        out.push(tool_row(vec![
            shape(cx, "pr-tools-ne-swell", "Swell", ExpressionShape::Swell),
            shape(cx, "pr-tools-ne-clear", "Clear", ExpressionShape::Clear),
        ]));
        out.push(tool_row(vec![tool_button(
            "pr-tools-ne-pitch-clear",
            "Clear Pitch Expression",
            Emphasis::Destructive,
            enabled,
            cx.listener(|this, _, _w, cx| {
                this.tool_expression_shape(NoteExpressionLane::Pitch, ExpressionShape::Clear, cx)
            }),
        )
        .into_any_element()]));
        out.push(tool_caption("Shapes span each note's own length."));
        out
    }

    /// The Chord Track drawn under the ruler, through the same transform as
    /// the grid. Display only: chords are edited on the arrangement's lane.
    pub(super) fn build_chord_strip(
        &self,
        cx: &Context<Self>,
        view_w: f32,
    ) -> Vec<gpui::AnyElement> {
        use crate::theme::{radius, space, typography};
        let tl = self.timeline.read(cx);
        let events = &tl.state.chord_events;
        if events.is_empty() {
            return vec![div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .px(px(space::BASE))
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_faint())
                .child("Chord Track is empty")
                .into_any_element()];
        }
        let start = self.x_to_project_beat(0.0) as f64;
        let end = self.x_to_project_beat(view_w) as f64;
        let accent = Colors::accent_primary();
        events
            .iter()
            .filter(|e| e.end_beat() > start && e.start_beat < end)
            .map(|e| {
                let x0 = self.project_beat_to_x(e.start_beat as f32);
                let x1 = self.project_beat_to_x(e.end_beat() as f32);
                let left = x0.max(-1.0);
                let width = (x1.min(view_w + 1.0) - left).max(1.0);
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(2.0))
                    .bottom(px(2.0))
                    .w(px(width))
                    .flex()
                    .items_center()
                    .px(px(space::TIGHT))
                    .overflow_hidden()
                    .rounded(px(radius::MICRO))
                    .border_l(px(1.0))
                    .border_color(Colors::with_alpha(accent, 0.7))
                    .bg(Colors::with_alpha(accent, 0.1))
                    .text_size(px(typography::DENSE_LABEL))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_primary())
                    .child(div().truncate().child(e.name()))
                    .into_any_element()
            })
            .collect()
    }
}

fn segment_position(index: usize, count: usize) -> FbSegment {
    match (index, count) {
        (_, 1) => FbSegment::Only,
        (0, _) => FbSegment::First,
        (i, n) if i + 1 == n => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

/// A row of mutually exclusive choices, the current one latched.
fn choice_track<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    current: T,
    cx: &mut Context<PianoRoll>,
    set: impl Fn(&mut PianoRoll, T) + Copy + 'static,
) -> gpui::AnyElement {
    let mut track = compact_track();
    for (i, (value, label)) in options.iter().copied().enumerate() {
        track = track.child(compact_segment(
            (id, i),
            label,
            current == value,
            segment_position(i, options.len()),
            cx.listener(move |this, _, _w, cx| {
                set(this, value);
                cx.notify();
            }),
        ));
    }
    track.into_any_element()
}

/// A short label in front of a control.
fn labeled(label: &'static str, control: gpui::AnyElement) -> gpui::AnyElement {
    use crate::theme::{space, typography};
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(
            div()
                .w(px(LABEL_W))
                .flex_shrink_0()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(control))
        .into_any_element()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Emphasis {
    /// The group's apply button: accent fill, one per group at most.
    Primary,
    Normal,
    /// Removes data; the label is in the error hue.
    Destructive,
}

/// A command button that shares a row with its siblings. Disabled when there
/// is nothing for it to act on — no state layer, no click.
fn tool_button(
    id: &'static str,
    label: &'static str,
    emphasis: Emphasis,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size, state, typography};
    let (rest, hover, pressed, border, text) = match emphasis {
        Emphasis::Primary => (
            Colors::accent_primary(),
            Colors::accent_primary_hover(),
            Colors::accent_pressed(),
            Colors::with_alpha(Colors::accent_primary(), 0.0),
            Colors::text_inverse(),
        ),
        Emphasis::Normal | Emphasis::Destructive => {
            let rest = Colors::button_bg();
            (
                rest,
                Colors::composite(rest, Colors::state_hover()),
                Colors::composite(rest, Colors::state_recessed()),
                Colors::button_border(),
                if emphasis == Emphasis::Destructive {
                    Colors::status_error()
                } else {
                    Colors::button_text()
                },
            )
        }
    };
    let button = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .aria_disabled(!enabled)
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .justify_center()
        .h(px(size::ROW_DENSE))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(border)
        .bg(rest)
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(text)
        .child(div().truncate().child(label));
    if enabled {
        button
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |s| s.bg(hover))
            .active(move |s| s.bg(pressed))
            .on_click(on_click)
    } else {
        button.opacity(state::DISABLED_CONTENT)
    }
}

/// The column's segmented track: the shared control's shape and state
/// layers at the inspector's density (11 px text, 20 px segments), so it
/// reads at the same size as the note inspector beside it.
fn compact_track() -> gpui::Div {
    use crate::theme::{radius, space};
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .p(px(space::HAIR))
        .rounded(px(radius::CONTROL))
        .bg(Colors::surface_input())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
}

fn compact_segment(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    active: bool,
    position: FbSegment,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size, space, typography};
    let track = Colors::surface_input();
    let rest = if active {
        Colors::composite(track, Colors::state_selected())
    } else {
        Colors::with_alpha(track, 0.0)
    };
    let hover = Colors::composite(if active { rest } else { track }, Colors::state_hover());
    let pressed = Colors::composite(if active { rest } else { track }, Colors::state_recessed());
    let r = radius::inner(radius::CONTROL, space::HAIR);
    let (tl, tr, bl, br) = match position {
        FbSegment::Only => (r, r, r, r),
        FbSegment::First => (r, radius::NONE, r, radius::NONE),
        FbSegment::Middle => (radius::NONE, radius::NONE, radius::NONE, radius::NONE),
        FbSegment::Last => (radius::NONE, r, radius::NONE, r),
    };
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .aria_toggled(if active {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .justify_center()
        .h(px(size::DENSE))
        .px(px(space::TIGHT))
        .rounded_tl(px(tl))
        .rounded_tr(px(tr))
        .rounded_bl(px(bl))
        .rounded_br(px(br))
        .bg(rest)
        .text_size(px(typography::UI_XS))
        .font_weight(if active {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_muted()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .on_click(on_click)
        .child(div().truncate().child(label))
}

/// The column's checkbox: the shared checkbox's states at inspector size.
fn compact_check(
    id: &'static str,
    label: &'static str,
    checked: bool,
    enabled: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size, space, state, typography};
    let hover = Colors::composite(Colors::surface_panel(), Colors::state_hover());
    let row = div()
        .id(id)
        .role(gpui::Role::CheckBox)
        .aria_label(label)
        .aria_disabled(!enabled)
        .aria_toggled(if checked {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(size::DENSE))
        .px(px(space::HAIR))
        .rounded(px(radius::CONTROL_SM))
        .child(
            div()
                .w(px(12.0))
                .h(px(12.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::MICRO))
                .border(px(1.0))
                .border_color(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::border_strong()
                })
                .bg(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::surface_input()
                })
                .when(checked, |b| {
                    b.child(
                        svg()
                            .path(assets::ICON_CHECK_PATH)
                            .size(px(9.0))
                            .text_color(Colors::on_accent()),
                    )
                }),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(if enabled {
                    Colors::text_secondary()
                } else {
                    Colors::text_disabled()
                })
                .child(label),
        );
    if enabled {
        row.cursor(gpui::CursorStyle::PointingHand)
            .hover(move |s| s.bg(hover))
            .on_click(on_toggle)
    } else {
        row.opacity(state::DISABLED_CONTENT + 0.2)
    }
}

fn tool_row(children: Vec<gpui::AnyElement>) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .gap(px(crate::theme::space::TIGHT))
        .children(children)
        .into_any_element()
}

fn tool_caption(text: &'static str) -> gpui::AnyElement {
    use crate::theme::typography;
    div()
        .text_size(px(typography::DENSE_LABEL))
        .line_height(px(14.0))
        .text_color(Colors::text_muted())
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triplets_round_trip_and_1_32_has_none() {
        for (base, _) in GRID_CHOICES {
            let (b, t) = split_grid(join_grid(base, true));
            assert_eq!(b, base);
            assert_eq!(t, has_triplet(base));
        }
        assert!(!has_triplet(GridRes::ThirtySecond));
        assert_eq!(
            join_grid(GridRes::ThirtySecond, true),
            GridRes::ThirtySecond
        );
    }

    #[test]
    fn expression_shapes_span_the_note() {
        let swell = ExpressionShape::Swell.curve(2.0);
        assert_eq!(swell.points.len(), 3);
        assert!((swell.points[2].position - 2.0).abs() < 1e-6);
        assert!(ExpressionShape::Clear.curve(1.0).points.is_empty());
    }
}
