//! How the Voxtune editor looks — the Android editor's layout and look,
//! fitted to a desktop window:
//!
//! ```text
//! ┌ file ─────────────────────────────────────────────┬──────────── undo redo ┐
//! │ [♪ A minor] [Free Semi Scale]          [⚙] [3 off] │ E4 +42¢   meter  Snap │
//! │ keys │ ruler                                       │ Pitch Drift Time …    │
//! │      │ notes (capsules, threads, tags)             │ PITCH ───●───  +42¢   │
//! │      │            [Cut Join Ornament Mute Reset]   │ CORRECT ──●── Tune all│
//! │      │ do  re  mi …                                │ HISTORY …             │
//! │ overview ───────────────────────────── [−][+]      │ 0:00.00  ⏮ ▶ ⟲ Bypass │
//! └────────────────────────────────────────────────────┴───────────────────────┘
//! ```
//!
//! Colours are theme tokens (the phone's palette *is* the default theme's),
//! so light themes work too.

use super::icons;
use super::*;

use gpui::{
    anchored, deferred, div, px, svg, Anchor, AnyElement, App, Bounds, Context, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Rgba, ScrollWheelEvent, SharedString, WeakEntity,
    Window,
};
use sphere_voxtune::engine::{
    CantoNote, KEY_NAMES, ORN_FALL, ORN_NONE, ORN_SHAKE, ORN_TURN_DOWN, ORN_TURN_UP,
};
use sphere_voxtune::load_job::{STAGE_ANALYZING, STAGE_DECODING, STAGE_INSTALLING};
use sphere_voxtune::scale::{scale_mask, Scale};

use super::scene::{cents_from_scale, off_key, Capsule};
use crate::components::controls::{
    fb_button, fb_progress, fb_segment, fb_segmented_track, fb_tooltip, FbButtonKind, FbSegment,
};
use crate::components::slider::{bipolar_slider_with_drag_callbacks, slider_with_drag_callbacks};
use crate::theme::{elevation, radius, size, space, typography, Colors};

/// Inspector width.
pub(super) const INSPECTOR_W: f32 = 392.0;
const HEADER_H: f32 = 48.0;
const LYRICS_H: f32 = 26.0;
const OVERVIEW_H: f32 = 44.0;

/// What the inspector shows: the note and take (with history and the
/// transport), or one of the phone's sheets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelTab {
    #[default]
    Main,
    Expression,
    Advanced,
}

fn icon(path: &'static str, size_px: f32, color: Rgba) -> gpui::Svg {
    svg()
        .path(path)
        .w(px(size_px))
        .h(px(size_px))
        .flex_none()
        .text_color(color)
}

/// A note with only an ornament, for drawing the ornament's shape.
fn demo_note(style: u8) -> CantoNote {
    CantoNote {
        id: String::new(),
        lyric: String::new(),
        unpitched: false,
        detected: 60.0,
        pitch: 60.0,
        detected_start: 0.0,
        start: 0.0,
        detected_dur: 0.8,
        dur: 0.8,
        modulation: 1.0,
        formant: 0.0,
        gain_db: 0.0,
        detected_gain: 0.0,
        pinned: false,
        muted: false,
        orn_style: style,
        orn_depth: 1.0,
        orn_speed: 0.5,
        accent: 0.0,
        scoop: 0.0,
        drift: 0.0,
        vib: 0.0,
        vib_cycles: 0.0,
        vib_phase: 0.0,
        contour: Vec::new(),
        envelope: Vec::new(),
    }
}

/// The parameter a tool chip edits with the inspector's slider.
fn tool_param(tool: VoxTool) -> Param {
    match tool {
        VoxTool::Pitch | VoxTool::Cut => Param::Pitch,
        VoxTool::Drift => Param::Drift,
        VoxTool::Time => Param::Time,
        VoxTool::Formant => Param::Formant,
        VoxTool::Level => Param::Level,
    }
}

/// Status colour of a note: accent in key, error colour off it.
fn status_color(off: bool) -> Rgba {
    if off {
        Colors::status_error()
    } else {
        Colors::accent_primary()
    }
}

fn hz(midi: f32) -> f32 {
    440.0 * 2f32.powf((midi - 69.0) / 12.0)
}

impl VoxtuneEditor {
    fn weak(&self, cx: &Context<Self>) -> WeakEntity<Self> {
        cx.entity().downgrade()
    }

    /// A round-cornered icon button; `on` latches it in `hue`.
    #[allow(clippy::too_many_arguments)]
    fn icon_button(
        &self,
        id: impl Into<gpui::ElementId>,
        path: &'static str,
        tip: impl Into<SharedString>,
        on: Option<bool>,
        hue: Rgba,
        enabled: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let active = on.unwrap_or(false);
        let base = Colors::surface_panel();
        let rest = if active {
            Colors::composite(base, Colors::with_alpha(hue, 0.16))
        } else {
            Colors::with_alpha(base, 0.0)
        };
        let hover = Colors::composite(if active { rest } else { base }, Colors::state_hover());
        let tint = if !enabled {
            Colors::text_disabled()
        } else if active {
            hue
        } else {
            Colors::text_secondary()
        };
        div()
            .id(id)
            .tooltip(fb_tooltip(tip))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(size::PROMINENT))
            .rounded(px(radius::CONTROL))
            .bg(rest)
            .when(active, |d| {
                d.border(px(1.0))
                    .border_color(Colors::with_alpha(hue, 0.55))
            })
            .child(icon(path, 16.0, tint))
            .when(enabled, |d| {
                d.cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        action(this, cx);
                        cx.notify();
                    }))
            })
            .into_any_element()
    }

    // ---- Header and toolbar ------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let undo_tip = match self.session.undo_label() {
            Some(l) => format!("Undo {} (Ctrl+Z)", history_label(l)),
            None => "Nothing to undo".into(),
        };
        let redo_tip = match self.session.redo_label() {
            Some(l) => format!("Redo {} (Ctrl+Shift+Z)", history_label(l)),
            None => "Nothing to redo".into(),
        };
        let accent = Colors::accent_primary();
        div()
            .flex()
            .items_center()
            .gap(px(space::BASE))
            .h(px(HEADER_H))
            .px(px(space::LOOSE))
            .bg(Colors::surface_sidebar())
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(icon(icons::TAKE, 18.0, accent))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(px(typography::DENSE_LABEL))
                            .text_color(Colors::text_muted())
                            .child("Solfege Voxtune"),
                    )
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(Colors::text_primary())
                            .whitespace_nowrap()
                            .child(if self.title.is_empty() {
                                "No take".to_string()
                            } else {
                                self.title.clone()
                            }),
                    ),
            )
            .child(div().flex_1())
            .child(self.icon_button(
                "voxtune-undo",
                icons::UNDO,
                undo_tip,
                None,
                accent,
                self.session.can_undo(),
                cx,
                |this, cx| this.undo(cx),
            ))
            .child(self.icon_button(
                "voxtune-redo",
                icons::REDO,
                redo_tip,
                None,
                accent,
                self.session.can_redo(),
                cx,
                |this, cx| this.redo(cx),
            ))
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.editable();
        let doc = self.session.doc();
        let accent = Colors::accent_primary();
        let open = self.menu == Some(Menu::KeyScale);

        // Key chip: the key and scale, a popover to change them.
        let chip_rest = Colors::surface_raised();
        let chip_hover = Colors::composite(chip_rest, Colors::state_hover());
        let key_chip = div()
            .id("voxtune-key-chip")
            .flex()
            .items_center()
            .gap(px(space::SNUG))
            .h(px(size::PROMINENT))
            .px(px(space::LOOSE))
            .rounded(px(radius::PILL))
            .bg(chip_rest)
            .border(px(1.0))
            .border_color(if open {
                Colors::with_alpha(accent, 0.6)
            } else {
                Colors::border_normal()
            })
            .child(icon(icons::NOTE, 14.0, accent))
            .child(
                div()
                    .text_size(px(typography::UI_MD))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if editable {
                        Colors::text_primary()
                    } else {
                        Colors::text_disabled()
                    })
                    .child(format!(
                        "{} {}",
                        doc.key_name(),
                        doc.scale.label().to_lowercase()
                    )),
            )
            .child(icon(icons::CHEVRON_DOWN, 13.0, Colors::text_muted()))
            .when(editable, |d| {
                d.cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(chip_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.menu = if this.menu.is_some() {
                            None
                        } else {
                            Some(Menu::KeyScale)
                        };
                        cx.notify();
                    }))
            });
        let mut key_wrap = div().relative().child(key_chip);
        if open {
            key_wrap = key_wrap.child(
                div().absolute().left_0().bottom_0().child(deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(gpui::point(px(0.0), px(space::SNUG)))
                        .snap_to_window_with_margin(px(space::BASE))
                        .child(self.render_key_menu(cx)),
                )),
            );
        }

        // Snap: where a pitch drag lands.
        let snap = div()
            .flex()
            .items_center()
            .gap(px(space::SNUG))
            .child(
                div()
                    .id("voxtune-snap-tip")
                    .tooltip(fb_tooltip("Where a pitch drag lands · Shift drags freely"))
                    .child(icon(icons::MAGNET, 15.0, Colors::text_muted())),
            )
            .child(
                fb_segmented_track().children(Snap::ALL.iter().enumerate().map(|(i, &s)| {
                    div()
                        .flex_1()
                        .child(fb_segment(
                            ("voxtune-snap", i),
                            s.label(),
                            self.snap == s,
                            match i {
                                0 => FbSegment::First,
                                2 => FbSegment::Last,
                                _ => FbSegment::Middle,
                            },
                            cx.listener(move |this, _, _, cx| {
                                this.snap = s;
                                cx.notify();
                            }),
                        ))
                        .into_any_element()
                })),
            );

        // Off-key count: jumps to the next one; in key, says so.
        let off = self.off_count();
        let (hue, label, glyph) = if off > 0 {
            (Colors::status_error(), format!("{off} off"), None)
        } else {
            (
                Colors::status_success(),
                "In key".to_string(),
                Some(icons::CHECK),
            )
        };
        let pill_hover = Colors::with_alpha(hue, 0.22);
        let off_pill = div()
            .id("voxtune-next-off")
            .tooltip(fb_tooltip(if off > 0 {
                "Select the next note off the scale"
            } else {
                "Every note sits within 15 cents of the scale"
            }))
            .flex()
            .items_center()
            .gap(px(space::TIGHT))
            .h(px(size::PROMINENT))
            .px(px(space::LOOSE))
            .rounded(px(radius::PILL))
            .bg(Colors::with_alpha(hue, 0.12))
            .border(px(1.0))
            .border_color(Colors::with_alpha(hue, 0.55))
            .when_some(glyph, |d, g| d.child(icon(g, 13.0, hue)))
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(hue)
                    .child(label),
            )
            .when(off > 0 && self.has_audio(), |d| {
                d.cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(pill_hover))
                    .on_click(cx.listener(|this, _, _, cx| this.select_next_off(cx)))
            })
            .when(!self.has_audio(), |d| d.opacity(0.0));

        div()
            .flex()
            .items_center()
            .gap(px(space::LOOSE))
            .h(px(TOOLBAR_H + 8.0))
            .px(px(space::LOOSE))
            .bg(Colors::surface_sidebar())
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(key_wrap)
            .child(snap)
            .child(div().flex_1())
            .child(self.icon_button(
                "voxtune-advanced",
                icons::SLIDERS,
                "Advanced tools: macros, quantize, transpose",
                Some(self.panel == PanelTab::Advanced),
                accent,
                self.has_audio(),
                cx,
                |this, _| {
                    this.panel = if this.panel == PanelTab::Advanced {
                        PanelTab::Main
                    } else {
                        PanelTab::Advanced
                    };
                },
            ))
            .child(off_pill)
            .into_any_element()
    }

    /// Key grid and scale list in one popover.
    fn render_key_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let doc = self.session.doc();
        let accent = Colors::accent_primary();
        let hover = Colors::composite(Colors::surface_raised(), Colors::state_hover());
        let keys = div()
            .flex()
            .flex_wrap()
            .gap(px(space::TIGHT))
            .children((0u8..12).map(|k| {
                let current = doc.key == k;
                div()
                    .id(("voxtune-key", k as usize))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(44.0))
                    .h(px(size::COMFORTABLE))
                    .rounded(px(radius::CONTROL))
                    .bg(if current {
                        Colors::with_alpha(accent, 0.18)
                    } else {
                        Colors::surface_input()
                    })
                    .border(px(1.0))
                    .border_color(if current {
                        Colors::with_alpha(accent, 0.7)
                    } else {
                        Colors::border_subtle()
                    })
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if current {
                        accent
                    } else {
                        Colors::text_secondary()
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session.set_key(k);
                        this.changed(cx);
                    }))
                    .child(KEY_NAMES[k as usize])
            }));
        let scales = div()
            .flex()
            .flex_col()
            .children(Scale::ALL.iter().enumerate().map(|(i, &s)| {
                let current = doc.scale == s;
                div()
                    .id(("voxtune-scale", i))
                    .relative()
                    .flex()
                    .items_center()
                    .h(px(size::ROW))
                    .px(px(space::BASE))
                    .rounded(px(radius::CONTROL))
                    .text_size(px(typography::UI_SM))
                    .text_color(if current {
                        Colors::text_primary()
                    } else {
                        Colors::text_secondary()
                    })
                    .when(current, |d| {
                        d.bg(Colors::with_alpha(accent, 0.12)).child(
                            div()
                                .absolute()
                                .left_0()
                                .top(px(5.0))
                                .bottom(px(5.0))
                                .w(px(2.0))
                                .rounded(px(radius::MICRO))
                                .bg(accent),
                        )
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session.set_scale(s);
                        this.changed(cx);
                    }))
                    .child(s.label())
            }));
        div()
            .id("voxtune-key-menu")
            .occlude()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .w(px(232.0))
            .max_h(px(460.0))
            .overflow_y_scroll()
            .p(px(space::BASE))
            .rounded(px(radius::SURFACE))
            .bg(Colors::surface_raised())
            .border(px(1.0))
            .border_color(Colors::border_normal())
            .shadow(elevation::shadow(elevation::OVERLAY))
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(Colors::text_faint())
                    .child("KEY"),
            )
            .child(keys)
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(Colors::text_faint())
                    .child("SCALE"),
            )
            .child(scales)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }))
            .into_any_element()
    }

    // ---- Pitch keys ------------------------------------------------------------

    /// The pitch ruler as a piano keyboard, scale degrees ticked on the right
    /// (the tonic solid), the selected note's key marked.
    fn render_keys(&self) -> AnyElement {
        let (_, h) = self.grid_size();
        let (lo, hi) = self.view.visible_pitches(h);
        let row = self.view.row_h;
        let doc = self.session.doc();
        let mask = scale_mask(doc.key, doc.scale);
        let tonic = doc.key % 12;
        let show_all = row >= 13.0;
        let selected_pitch = self
            .selected_note()
            .filter(|n| !n.unpitched)
            .map(|n| n.pitch.round() as i32);
        let accent = Colors::accent_primary();
        let keys: Vec<AnyElement> = (lo..=hi)
            .map(|p| {
                let pc = p.rem_euclid(12) as u8;
                let black = matches!(pc, 1 | 3 | 6 | 8 | 10);
                let in_scale = mask & (1 << pc) != 0;
                let is_selected = selected_pitch == Some(p);
                let ink = if black {
                    Colors::text_secondary()
                } else {
                    Colors::text_inverse()
                };
                div()
                    .absolute()
                    .left_0()
                    .w_full()
                    .top(px(self.view.row_top(p)))
                    .h(px(row))
                    .flex()
                    .items_center()
                    .justify_end()
                    .pr(px(space::BASE))
                    .bg(if is_selected {
                        Colors::composite(
                            if black {
                                Colors::piano_black_key()
                            } else {
                                Colors::piano_white_key()
                            },
                            Colors::with_alpha(accent, 0.28),
                        )
                    } else if black {
                        Colors::piano_black_key()
                    } else {
                        Colors::piano_white_key()
                    })
                    .border_b(px(1.0))
                    .border_color(Colors::piano_key_seam())
                    .when(in_scale, |d| {
                        d.child(
                            div()
                                .absolute()
                                .right_0()
                                .top(px(1.0))
                                .bottom(px(1.0))
                                .w(px(3.0))
                                .bg(if pc == tonic {
                                    accent
                                } else {
                                    Colors::with_alpha(accent, 0.55)
                                }),
                        )
                    })
                    .when(show_all || pc == tonic || pc == 0 || is_selected, |d| {
                        d.child(
                            div()
                                .text_size(px(typography::DENSE_LABEL))
                                .font_weight(if in_scale {
                                    gpui::FontWeight::SEMIBOLD
                                } else {
                                    gpui::FontWeight::NORMAL
                                })
                                .text_color(if in_scale {
                                    ink
                                } else {
                                    Colors::with_alpha(ink, 0.5)
                                })
                                .child(note_name(p)),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        div()
            .id("voxtune-keys")
            .relative()
            .flex_none()
            .w(px(KEYS_W))
            .h_full()
            .overflow_hidden()
            .border_r(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_sidebar())
            .children(keys)
            .into_any_element()
    }

    // ---- Note overlays ---------------------------------------------------------

    fn capsule(&self, index: usize) -> Option<Capsule> {
        let n = self.session.notes().get(index)?.present(self.bypass);
        Some(Capsule::of(&n, &self.view))
    }

    /// A tag above a note: cents off the scale, or (selected) name and cents.
    fn tag(cap: &Capsule, text: String, hue: Rgba, strong: bool, grid_w: f32) -> AnyElement {
        let w = text.chars().count() as f32 * if strong { 7.2 } else { 6.2 } + 14.0;
        let h = if strong { 20.0 } else { 16.0 };
        let x = ((cap.x0 + cap.x1) * 0.5 - w * 0.5).clamp(2.0, (grid_w - w - 2.0).max(2.0));
        let y = if cap.top() - h - 4.0 >= 0.0 {
            cap.top() - h - 4.0
        } else {
            cap.bottom() + 4.0
        };
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .h(px(h))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(radius::PILL))
            .bg(Colors::with_alpha(Colors::surface_canvas(), 0.92))
            .when(strong, |d| {
                d.border(px(1.0)).border_color(Colors::with_alpha(hue, 0.8))
            })
            .text_size(px(if strong {
                typography::UI_XS
            } else {
                typography::DENSE_CAPTION
            }))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(hue)
            .whitespace_nowrap()
            .child(text)
            .into_any_element()
    }

    /// What the tool is doing, for the drag readout: icon, value, detail.
    fn tool_readout(&self, index: usize) -> Option<(&'static str, String, String)> {
        let n = self.session.notes().get(index)?;
        let bpm = self.session.doc().bpm;
        let snap = if self.drag_fine {
            "free"
        } else {
            match self.snap {
                Snap::Free => "free · Shift",
                Snap::Semi => "semitones · Shift = free",
                Snap::Scale => "scale notes · Shift = free",
            }
        };
        Some(match self.drag? {
            Drag::Value { tool, start, .. } => match tool {
                VoxTool::Pitch => (
                    icons::TOOL_PITCH,
                    format!(
                        "{} {}",
                        note_name(n.pitch.round() as i32),
                        format_cents((n.pitch - n.pitch.round()) * 100.0)
                    ),
                    format!("{:+.2} st  ·  {snap}", n.pitch - start + 0.0),
                ),
                VoxTool::Drift => (
                    icons::TOOL_DRIFT,
                    format!("Drift {}%", (n.modulation * 100.0).round()),
                    format!("was {}%  ·  down flattens", (start * 100.0).round()),
                ),
                VoxTool::Formant => (
                    icons::TOOL_FORMANT,
                    format!("Formant {:+.1} st", n.formant + 0.0),
                    format!("was {:+.1} st", start + 0.0),
                ),
                VoxTool::Level => (
                    icons::TOOL_LEVEL,
                    format!("{:+.1} dB", (n.gain_db * 10.0).round() / 10.0 + 0.0),
                    format!("{:+.1} dB change", (n.gain_db - start) + 0.0),
                ),
                VoxTool::Time | VoxTool::Cut => return None,
            },
            Drag::Time {
                edge, start, dur, ..
            } => match edge {
                TimingEdge::Body => (
                    icons::TOOL_TIME,
                    format!("Bar {}", format_position(n.start, bpm)),
                    format!("{:+.3} s  ·  at {:.3} s", n.start - start + 0.0, n.start),
                ),
                TimingEdge::Start => (
                    icons::TOOL_TIME,
                    format!("Start {:.3} s", n.start),
                    format!("length {:.2} s ({:+.3})", n.dur, n.dur - dur + 0.0),
                ),
                TimingEdge::End => (
                    icons::TOOL_TIME,
                    format!("Length {:.2} s", n.dur),
                    format!(
                        "{:+.3} s  ·  ends {:.3} s",
                        n.dur - dur + 0.0,
                        n.start + n.dur
                    ),
                ),
            },
            _ => return None,
        })
    }

    fn hud(
        glyph: &'static str,
        value: String,
        detail: String,
        cap: &Capsule,
        grid_w: f32,
        anchor_x: f32,
    ) -> AnyElement {
        const HUD_W: f32 = 236.0;
        const HUD_H: f32 = 44.0;
        let y = if cap.top() - HUD_H - 12.0 >= 0.0 {
            cap.top() - HUD_H - 12.0
        } else {
            cap.bottom() + 12.0
        };
        let x = (anchor_x - HUD_W * 0.5).clamp(4.0, (grid_w - HUD_W - 4.0).max(4.0));
        let accent = Colors::accent_primary();
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(HUD_W))
            .h(px(HUD_H))
            .flex()
            .items_center()
            .gap(px(space::BASE))
            .px(px(space::BASE))
            .rounded(px(radius::SURFACE))
            .bg(Colors::with_alpha(Colors::surface_raised(), 0.97))
            .border(px(1.0))
            .border_color(Colors::with_alpha(accent, 0.45))
            .shadow(elevation::shadow(elevation::OVERLAY))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.0))
                    .rounded(px(radius::CONTROL))
                    .bg(Colors::with_alpha(accent, 0.16))
                    .child(icon(glyph, 15.0, accent)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(px(typography::UI_MD))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .whitespace_nowrap()
                            .child(value),
                    )
                    .child(
                        div()
                            .text_size(px(typography::DENSE_LABEL))
                            .text_color(Colors::text_muted())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(detail),
                    ),
            )
            .into_any_element()
    }

    /// Off-key tags, the selected note's badge, and the drag readout.
    fn render_note_overlays(&self) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let (w, _) = self.grid_size();
        let doc = self.session.doc();
        let (t0, t1) = self.view.visible_time(w);
        // Off-key tags, without letting two overlap.
        let mut placed: Vec<(f32, f32)> = Vec::new();
        if self.drag.is_none() {
            for (i, raw) in self.session.notes().iter().enumerate() {
                let n = raw.present(self.bypass);
                if Some(i) == self.selected
                    || n.start > t1
                    || n.start + n.dur < t0
                    || !off_key(&n, doc.key, doc.scale)
                {
                    continue;
                }
                let cap = Capsule::of(&n, &self.view);
                if cap.x1 - cap.x0 < 14.0 {
                    continue;
                }
                let cx = (cap.x0 + cap.x1) * 0.5;
                if placed
                    .iter()
                    .any(|&(x, y)| (x - cx).abs() < 44.0 && (y - cap.top()).abs() < 20.0)
                {
                    continue;
                }
                placed.push((cx, cap.top()));
                let cents = cents_from_scale(n.pitch, doc.key, doc.scale).1;
                out.push(Self::tag(
                    &cap,
                    format!("{}¢", format_signed_cents(cents)),
                    Colors::status_error(),
                    false,
                    w,
                ));
            }
        }
        let Some(i) = self.selected else {
            return out;
        };
        let Some(cap) = self.capsule(i) else {
            return out;
        };
        match self.tool_readout(i) {
            Some((glyph, value, detail)) => {
                let anchor = self.pointer.map(|p| p.0).unwrap_or((cap.x0 + cap.x1) * 0.5);
                out.push(Self::hud(glyph, value, detail, &cap, w, anchor));
            }
            None => {
                if let Some(n) = self
                    .session
                    .notes()
                    .get(i)
                    .map(|n| n.present(self.bypass))
                    .filter(|n| !n.unpitched)
                {
                    let (target, cents) = cents_from_scale(n.pitch, doc.key, doc.scale);
                    let off = cents.abs() >= scene::OFF_KEY_CENTS;
                    let _ = target;
                    out.push(Self::tag(
                        &cap,
                        format!(
                            "{} · {}¢",
                            note_name(n.pitch.round() as i32),
                            format_signed_cents(cents)
                        ),
                        status_color(off),
                        true,
                        w,
                    ));
                }
            }
        }
        out
    }

    /// The selected note's actions, floating over the bottom of the lane.
    fn render_action_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let note = self.selected_note()?.clone();
        if !self.editable() || self.drag.is_some() {
            return None;
        }
        let can_join = self
            .selected
            .is_some_and(|i| i + 1 < self.session.notes().len());
        let cutting = self.tool == VoxTool::Cut;
        let item = |id: &'static str,
                    glyph: &'static str,
                    label: &'static str,
                    enabled: bool,
                    latched: Option<Rgba>,
                    cx: &mut Context<Self>,
                    action: Box<dyn Fn(&mut Self, &mut Context<Self>)>| {
            let base = Colors::surface_raised();
            let hover = Colors::composite(base, Colors::state_hover());
            let ink = latched.unwrap_or(Colors::text_primary());
            div()
                .id(id)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(3.0))
                .w(px(62.0))
                .h(px(48.0))
                .rounded(px(radius::CONTROL))
                .when_some(latched, |d, hue| d.bg(Colors::with_alpha(hue, 0.16)))
                .child(icon(
                    glyph,
                    17.0,
                    if enabled {
                        ink
                    } else {
                        Colors::text_disabled()
                    },
                ))
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if enabled {
                            ink
                        } else {
                            Colors::text_disabled()
                        })
                        .child(label),
                )
                .when(enabled, |d| {
                    d.cursor(gpui::CursorStyle::PointingHand)
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            action(this, cx);
                            cx.notify();
                        }))
                })
                .into_any_element()
        };
        let pitched = !note.unpitched;
        let bar = div()
            .flex()
            .items_center()
            .gap(px(space::HAIR))
            .p(px(space::TIGHT))
            .rounded(px(radius::SURFACE + 4.0))
            .bg(Colors::with_alpha(Colors::surface_raised(), 0.97))
            .border(px(1.0))
            .border_color(Colors::border_normal())
            .shadow(elevation::shadow(elevation::OVERLAY))
            .child(item(
                "voxtune-act-cut",
                icons::TOOL_CUT,
                "Cut",
                true,
                cutting.then(Colors::accent_primary),
                cx,
                Box::new(|this, _| this.toggle_cut_mode()),
            ))
            .child(item(
                "voxtune-act-join",
                icons::JOIN,
                "Join",
                can_join,
                None,
                cx,
                Box::new(|this, cx| this.join_selected(cx)),
            ))
            .child(item(
                "voxtune-act-ornament",
                icons::EXPRESSION,
                "Ornament",
                pitched,
                (self.panel == PanelTab::Expression).then(Colors::accent_primary),
                cx,
                Box::new(|this, _| {
                    this.panel = if this.panel == PanelTab::Expression {
                        PanelTab::Main
                    } else {
                        PanelTab::Expression
                    };
                }),
            ))
            .child(item(
                "voxtune-act-mute",
                icons::MUTE,
                "Mute",
                true,
                note.muted.then(Colors::state_mute),
                cx,
                Box::new(|this, cx| {
                    this.run_command("mute", cx);
                }),
            ))
            .child(item(
                "voxtune-act-reset",
                icons::RESET,
                "Reset",
                true,
                None,
                cx,
                Box::new(|this, cx| {
                    this.run_command("reset-note", cx);
                }),
            ));
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(space::LOOSE))
                .flex()
                .justify_center()
                .child(bar)
                .into_any_element(),
        )
    }

    fn render_grid(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let doc = self.session.doc().clone();
        let notes = self.notes_snapshot();
        let cut = (self.tool == VoxTool::Cut && self.drag.is_none())
            .then(|| {
                let i = self.hovered?;
                let (x, _) = self.pointer?;
                let cap = self.capsule(i)?;
                (x > cap.x0 && x < cap.x1).then(|| (i, self.view.x_to_time(x)))
            })
            .flatten();
        let data = CanvasData {
            notes,
            view: self.view,
            key: doc.key,
            scale: doc.scale,
            bpm: doc.bpm,
            duration: self.session.duration(),
            selected: self.selected,
            hovered: self.hovered,
            bypass: self.bypass,
            cut,
        };
        let (w, _) = self.grid_size();
        let play_x = self.view.time_to_x(self.last_transport.position as f32);
        let cursor = match (self.drag, self.tool) {
            (Some(Drag::Pan { .. }), _) => gpui::CursorStyle::ClosedHand,
            (Some(Drag::Value { .. }), _) => gpui::CursorStyle::ResizeUpDown,
            (Some(Drag::Time { .. }), _) => gpui::CursorStyle::ResizeLeftRight,
            (_, VoxTool::Cut) => gpui::CursorStyle::IBeam,
            (_, VoxTool::Time) if self.hovered.is_some() => gpui::CursorStyle::ResizeLeftRight,
            (_, _) if self.hovered.is_some() => gpui::CursorStyle::PointingHand,
            _ => gpui::CursorStyle::Arrow,
        };
        let overlays = self.render_note_overlays();
        let action_bar = self.render_action_bar(cx);
        div()
            .id("voxtune-grid")
            .relative()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .bg(Colors::timeline_background())
            .cursor(cursor)
            .child(note_canvas(
                data,
                self.grid_bounds.clone(),
                self.gpu.clone(),
            ))
            .children(overlays)
            .when(play_x >= 0.0 && play_x <= w, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(play_x.floor()))
                        .w(px(1.0))
                        .bg(Colors::timeline_playhead()),
                )
            })
            .children(action_bar)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.leave_grid(cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_grid_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_grid_down))
            .on_mouse_move(
                cx.listener(|this, ev: &MouseMoveEvent, _, cx| this.on_grid_move(ev, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_pointer(cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _, _, cx| this.finish_pointer(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_pointer(cx)),
            )
            .on_scroll_wheel(
                cx.listener(|this, ev: &ScrollWheelEvent, _, cx| this.on_wheel(ev, cx)),
            )
            .into_any_element()
    }

    // ---- Solfège lane and overview -----------------------------------------------

    /// The syllable under each note; the selected one in an accent pill.
    fn render_lyrics(&self) -> AnyElement {
        let (w, _) = self.grid_size();
        let mut last_right = f32::MIN;
        let mut items = Vec::new();
        for (i, raw) in self.session.notes().iter().enumerate() {
            let n = raw.present(self.bypass);
            if n.unpitched {
                continue;
            }
            let cap = Capsule::of(&n, &self.view);
            let cx = (cap.x0 + cap.x1) * 0.5;
            let tw = n.lyric.chars().count() as f32 * 6.5 + 12.0;
            let left = cx - tw * 0.5;
            if left + tw < 0.0 || left > w || left < last_right + 2.0 {
                continue;
            }
            last_right = left + tw;
            let selected = self.selected == Some(i);
            items.push(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(4.0))
                    .w(px(tw))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(radius::PILL))
                    .when(selected, |d| d.bg(Colors::accent_primary()))
                    .text_size(px(typography::UI_XS))
                    .font_weight(if selected {
                        gpui::FontWeight::BOLD
                    } else {
                        gpui::FontWeight::MEDIUM
                    })
                    .text_color(if selected {
                        Colors::text_inverse()
                    } else if raw.muted {
                        Colors::text_disabled()
                    } else {
                        Colors::text_secondary()
                    })
                    .child(n.lyric.clone())
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_row()
            .h(px(LYRICS_H))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_sidebar())
            .child(
                div()
                    .w(px(KEYS_W))
                    .flex_none()
                    .border_r(px(1.0))
                    .border_color(Colors::border_subtle()),
            )
            .child(div().relative().flex_1().overflow_hidden().children(items))
            .into_any_element()
    }

    /// The whole take in small: notes as bars in their colours, the view as
    /// a window. Click or drag to move the view there.
    fn render_overview(&self, cx: &mut Context<Self>) -> AnyElement {
        let doc = self.session.doc();
        let notes = self.session.notes();
        let duration = self.session.duration().max(0.01);
        let (lo, hi) = notes
            .iter()
            .filter(|n| !n.unpitched)
            .fold((f32::MAX, f32::MIN), |(lo, hi), n| {
                (lo.min(n.pitch), hi.max(n.pitch))
            });
        let (lo, hi) = if lo > hi {
            (55.0, 72.0)
        } else {
            (lo - 1.0, hi + 1.0)
        };
        let (gw, _) = self.grid_size();
        let (v0, v1) = self.view.visible_time(gw);
        let bars: Vec<(f32, f32, f32, Rgba)> = notes
            .iter()
            .map(|n| {
                let n = n.present(self.bypass);
                let color = if n.unpitched || n.muted {
                    Colors::text_faint()
                } else {
                    status_color(off_key(&n, doc.key, doc.scale))
                };
                (
                    n.start / duration,
                    (n.start + n.dur) / duration,
                    1.0 - (n.pitch - lo) / (hi - lo).max(1.0),
                    color,
                )
            })
            .collect();
        let play = self.last_transport.position as f32 / duration;
        let window = (v0 / duration, v1 / duration);
        let well = Colors::timeline_background();
        let edge = Colors::with_alpha(Colors::text_primary(), 0.55);
        let playhead = Colors::timeline_playhead();
        let capture = self.overview_bounds.clone();
        let strip = gpui::canvas(
            move |b, _, _| capture.set(Some(b)),
            move |b: Bounds<Pixels>, (), window_: &mut Window, _| {
                let w = f32::from(b.size.width);
                let h = f32::from(b.size.height);
                let q = |x: f32, y: f32, qw: f32, qh: f32, c: Rgba| {
                    gpui::fill(
                        Bounds::new(
                            b.origin + gpui::point(px(x), px(y)),
                            gpui::size(px(qw), px(qh)),
                        ),
                        c,
                    )
                };
                window_.paint_quad(q(0.0, 0.0, w, h, well).corner_radii(px(radius::CONTROL)));
                for &(a, z, y, c) in &bars {
                    let bw = ((z - a) * w).max(2.0);
                    window_.paint_quad(
                        q(a * w, 4.0 + y * (h - 12.0), bw, 4.0, c).corner_radii(px(2.0)),
                    );
                }
                let (wa, wz) = (window.0.clamp(0.0, 1.0) * w, window.1.clamp(0.0, 1.0) * w);
                window_.paint_quad(gpui::quad(
                    Bounds::new(
                        b.origin + gpui::point(px(wa), px(1.0)),
                        gpui::size(px((wz - wa).max(4.0)), px(h - 2.0)),
                    ),
                    px(radius::CONTROL),
                    Colors::with_alpha(edge, 0.06),
                    px(1.0),
                    edge,
                    gpui::BorderStyle::Solid,
                ));
                if (0.0..=1.0).contains(&play) {
                    window_.paint_quad(q(play * w, 0.0, 1.0, h, playhead));
                }
            },
        )
        .size_full();
        div()
            .flex()
            .items_center()
            .gap(px(space::BASE))
            .h(px(OVERVIEW_H))
            .px(px(space::BASE))
            .bg(Colors::surface_sidebar())
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .id("voxtune-overview")
                    .flex_1()
                    .h(px(OVERVIEW_H - 14.0))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child(strip)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                            this.overview_to(f32::from(ev.position.x), cx)
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                        if ev.pressed_button == Some(MouseButton::Left) {
                            this.overview_to(f32::from(ev.position.x), cx);
                        }
                    })),
            )
            .child(self.icon_button(
                "voxtune-zoom-out",
                icons::ZOOM_OUT,
                "Zoom out (Ctrl+-)",
                None,
                Colors::accent_primary(),
                self.has_audio(),
                cx,
                |this, cx| this.zoom(0.8, cx),
            ))
            .child(self.icon_button(
                "voxtune-zoom-in",
                icons::ZOOM_IN,
                "Zoom in (Ctrl+=)",
                None,
                Colors::accent_primary(),
                self.has_audio(),
                cx,
                |this, cx| this.zoom(1.25, cx),
            ))
            .child(self.icon_button(
                "voxtune-fit",
                icons::FIT,
                "Fit the take (F)",
                None,
                Colors::accent_primary(),
                self.has_audio(),
                cx,
                |this, _| this.fit_view(),
            ))
            .into_any_element()
    }

    /// Centres the view on the overview's `window_x`.
    fn overview_to(&mut self, window_x: f32, cx: &mut Context<Self>) {
        let Some(b) = self.overview_bounds.get() else {
            return;
        };
        let w = f32::from(b.size.width).max(1.0);
        let t = ((window_x - f32::from(b.origin.x)) / w).clamp(0.0, 1.0) * self.session.duration();
        let (gw, gh) = self.grid_size();
        self.view.t0 = t - gw * 0.5 / self.view.pps;
        self.follow = false;
        self.view.clamp(self.session.duration(), gw, gh);
        cx.notify();
    }

    // ---- Inspector -------------------------------------------------------------

    fn caption(text: impl Into<SharedString>) -> gpui::Div {
        div()
            .text_size(px(typography::DENSE_CAPTION))
            .font_weight(gpui::FontWeight::BOLD)
            .text_color(Colors::text_faint())
            .child(text.into())
    }

    /// A slider for `p`, wired to the editor's gesture path.
    fn slider_for(&self, p: Param, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.param_enabled(p);
        let value = self.param_value(p).unwrap_or(0.0);
        let weak = self.weak(cx);
        let start = {
            let w = weak.clone();
            move |_: &f32, _: &mut Window, cx: &mut App| {
                let _ = w.update(cx, |ed, cx| ed.begin_param(p, cx));
            }
        };
        let preview = {
            let w = weak.clone();
            move |v: &f32, _: &mut Window, cx: &mut App| {
                let v = *v;
                let _ = w.update(cx, |ed, cx| ed.set_param(p, v, cx));
            }
        };
        let commit = {
            let w = weak.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = w.update(cx, |ed, cx| ed.end_gesture(cx));
            }
        };
        let reset = {
            let w = weak.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = w.update(cx, |ed, cx| ed.reset_param(p, cx));
            }
        };
        let accent = Colors::accent_primary();
        let slider = if p.bipolar() {
            bipolar_slider_with_drag_callbacks(
                p.slider_id(),
                value,
                accent,
                Some(self.param_text(p)),
                Some(start),
                Some(preview),
                Some(commit),
                Some(reset),
            )
            .into_any_element()
        } else {
            slider_with_drag_callbacks(
                p.slider_id(),
                value,
                accent,
                Some(start),
                Some(preview),
                Some(commit),
                Some(reset),
            )
            .into_any_element()
        };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .when(!enabled, |d| d.opacity(0.35).occlude())
            .child(slider)
            .into_any_element()
    }

    /// `LABEL ───●─── value`, the phone's slider row.
    fn slider_row(
        &self,
        p: Param,
        label: &'static str,
        cx: &mut Context<Self>,
        trailing: Option<AnyElement>,
    ) -> AnyElement {
        let enabled = self.param_enabled(p);
        div()
            .flex()
            .items_center()
            .gap(px(space::BASE))
            .h(px(size::PROMINENT))
            .child(Self::caption(label).w(px(62.0)).flex_none())
            .child(self.slider_for(p, cx))
            .child(
                div()
                    .w(px(54.0))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if enabled {
                        Colors::text_primary()
                    } else {
                        Colors::text_disabled()
                    })
                    .child(self.param_text(p)),
            )
            .children(trailing)
            .into_any_element()
    }

    /// The selected note, large: name and cents in its status colour, Hz,
    /// the cents meter, syllable and bar, and Snap.
    fn note_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let doc = self.session.doc();
        let Some(n) = self
            .selected_note()
            .map(|n| n.present(self.bypass))
            .filter(|n| !n.unpitched)
        else {
            return div()
                .flex()
                .items_center()
                .gap(px(space::LOOSE))
                .h(px(96.0))
                .child(icon(icons::VOCAL, 26.0, Colors::text_faint()))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(typography::UI_MD))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_secondary())
                                .child(if self.has_audio() {
                                    "Pick a note"
                                } else {
                                    "No take loaded"
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::text_muted())
                                .child(if self.has_audio() {
                                    "Click a note, or jump to the next one off the scale."
                                } else {
                                    "Open a vocal take to start."
                                }),
                        ),
                )
                .into_any_element();
        };
        let (target, cents) = cents_from_scale(n.pitch, doc.key, doc.scale);
        let off = cents.abs() >= scene::OFF_KEY_CENTS;
        let hue = status_color(off);
        let sung = (n.detected - target as f32) * 100.0;
        let meter_hue = hue;
        let tick = Colors::with_alpha(Colors::text_primary(), 0.35);
        let track = Colors::with_alpha(Colors::accent_primary(), 0.16);
        let ghost = Colors::text_faint();
        let meter = gpui::canvas(
            |_, _, _| {},
            move |b: Bounds<Pixels>, (), window, _| {
                let w = f32::from(b.size.width);
                let h = f32::from(b.size.height);
                let x_of = |c: f32| (c.clamp(-100.0, 100.0) + 100.0) / 200.0 * w;
                let q = |x: f32, y: f32, qw: f32, qh: f32, c: Rgba| {
                    gpui::fill(
                        Bounds::new(
                            b.origin + gpui::point(px(x), px(y)),
                            gpui::size(px(qw), px(qh)),
                        ),
                        c,
                    )
                };
                for k in 0..=8 {
                    let c = -100.0 + 25.0 * k as f32;
                    let th = if k == 4 { h } else { h * 0.4 };
                    window.paint_quad(q(x_of(c).min(w - 1.0), (h - th) * 0.5, 1.0, th, tick));
                }
                // The move from where it was sung to where it plays.
                let (a, z) = (x_of(sung.min(cents)), x_of(sung.max(cents)));
                window.paint_quad(q(a, h * 0.5 - 3.0, (z - a).max(2.0), 6.0, track));
                window.paint_quad(q(x_of(sung) - 1.0, h * 0.2, 2.0, h * 0.6, ghost));
                window.paint_quad(q(x_of(cents) - 1.5, 0.0, 3.0, h, meter_hue));
            },
        )
        .w_full()
        .h(px(18.0));
        let snap_hover = Colors::with_alpha(Colors::accent_primary(), 0.26);
        let can_snap = self.editable() && cents.abs() >= 1.0;
        let snap = div()
            .id("voxtune-snap-note")
            .tooltip(fb_tooltip("Snap this note onto the scale"))
            .flex()
            .items_center()
            .gap(px(space::SNUG))
            .h(px(size::PROMINENT))
            .px(px(space::LOOSE))
            .rounded(px(radius::PILL))
            .bg(Colors::with_alpha(Colors::accent_primary(), 0.16))
            .border(px(1.0))
            .border_color(Colors::with_alpha(Colors::accent_primary(), 0.55))
            .child(icon(icons::TOOL_FORMANT, 14.0, Colors::accent_primary()))
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::accent_primary())
                    .child("Snap"),
            )
            .when(!can_snap, |d| d.opacity(0.4))
            .when(can_snap, |d| {
                d.cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(snap_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.run_command("tune-note", cx);
                    }))
            });
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(space::TIGHT))
                            .child(
                                div()
                                    .text_size(px(30.0))
                                    .line_height(px(32.0))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(Colors::text_primary())
                                    .child(note_name(n.pitch.round() as i32)),
                            )
                            .child(
                                div()
                                    .mt(px(2.0))
                                    .text_size(px(typography::UI_MD))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(hue)
                                    .child(format!("{}¢", format_signed_cents(cents))),
                            ),
                    )
                    .child(div().flex_1())
                    .child(snap),
            )
            .child(meter)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(format!("{:.1} Hz", hz(n.pitch)))
                    .child(format!(
                        "{} · bar {}",
                        n.lyric,
                        format_position(n.start, doc.bpm)
                    )),
            )
            .into_any_element()
    }

    /// Tool chips: what a drag (and the slider below) edits, with the
    /// selected note's value under each name.
    fn tool_chips(&self, cx: &mut Context<Self>) -> AnyElement {
        let accent = Colors::accent_primary();
        let has_note = self.selected_note().is_some_and(|n| !n.unpitched);
        div()
            .flex()
            .gap(px(space::SNUG))
            .children(
                [
                    VoxTool::Pitch,
                    VoxTool::Drift,
                    VoxTool::Time,
                    VoxTool::Formant,
                    VoxTool::Level,
                ]
                .into_iter()
                .enumerate()
                .map(|(i, tool)| {
                    let active = self.tool == tool
                        || (self.tool == VoxTool::Cut && self.tool_before_cut == Some(tool));
                    let base = Colors::surface_input();
                    let rest = if active {
                        Colors::composite(base, Colors::with_alpha(accent, 0.16))
                    } else {
                        base
                    };
                    let hover = Colors::composite(rest, Colors::state_hover());
                    let value = if has_note {
                        self.param_text(tool_param(tool))
                    } else {
                        "—".to_string()
                    };
                    div()
                        .id(("voxtune-chip", i))
                        .tooltip(fb_tooltip(tool.tooltip()))
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .px(px(5.0))
                        .py(px(space::BASE))
                        .rounded(px(radius::SURFACE))
                        .bg(rest)
                        .border(px(1.0))
                        .border_color(if active {
                            Colors::with_alpha(accent, 0.7)
                        } else {
                            Colors::border_subtle()
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(3.0))
                                .child(icon(
                                    tool.icon(),
                                    12.0,
                                    if active { accent } else { Colors::text_muted() },
                                ))
                                .child(
                                    div()
                                        .text_size(px(typography::DENSE_LABEL))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(if active {
                                            Colors::text_primary()
                                        } else {
                                            Colors::text_muted()
                                        })
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .child(tool.label()),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(typography::UI_SM))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(if active {
                                    accent
                                } else {
                                    Colors::text_primary()
                                })
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .child(value),
                        )
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.tool = tool;
                            this.tool_before_cut = None;
                            cx.notify();
                        }))
                }),
            )
            .into_any_element()
    }

    fn render_history_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.session.history();
        let count = rows.len().saturating_sub(1);
        let current = rows.iter().position(|r| r.current).unwrap_or(0);
        let hover = Colors::composite(Colors::surface_sidebar(), Colors::state_hover());
        let accent = Colors::accent_primary();
        let list: Vec<AnyElement> =
            if count == 0 {
                vec![div()
                    .flex()
                    .justify_center()
                    .py(px(space::BLOCK))
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_muted())
                    .child("No edits yet")
                    .into_any_element()]
            } else {
                rows.into_iter()
                    .rev()
                    .map(|row| {
                        let position = row.position;
                        let undone = position > current;
                        div()
                            .id(("voxtune-history-row", position))
                            .relative()
                            .flex()
                            .items_center()
                            .gap(px(space::BASE))
                            .h(px(size::ROW + 2.0))
                            .px(px(space::BASE))
                            .rounded(px(radius::CONTROL))
                            .when(row.current, |d| d.bg(Colors::with_alpha(accent, 0.12)))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .hover(move |s| s.bg(hover))
                            .child(div().size(px(6.0)).rounded(px(radius::PILL)).bg(
                                if row.current {
                                    accent
                                } else if undone {
                                    Colors::with_alpha(Colors::text_faint(), 0.4)
                                } else {
                                    Colors::text_faint()
                                },
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(typography::UI_SM))
                                    .text_color(if row.current {
                                        Colors::text_primary()
                                    } else if undone {
                                        Colors::text_faint()
                                    } else {
                                        Colors::text_secondary()
                                    })
                                    .when(undone, |d| d.line_through())
                                    .child(history_label(&row.label)),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.session.jump_to(position);
                                this.changed(cx);
                            }))
                            .into_any_element()
                    })
                    .collect()
            };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .px(px(space::LOOSE + 2.0))
                    .pt(px(space::LOOSE))
                    .pb(px(space::SNUG))
                    .child(Self::caption("HISTORY"))
                    .child(Self::caption(format!("{count}"))),
            )
            .child(
                div()
                    .id("voxtune-history")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(space::SNUG))
                    .children(list),
            )
            .into_any_element()
    }

    /// Clock, level, transport and Bypass, at the inspector's foot.
    fn render_transport(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.last_transport;
        let doc = self.session.doc();
        let controls = self.host.controls_transport() && self.has_audio();
        let accent = Colors::accent_primary();
        let level = self.session.shared().level().clamp(0.0, 1.0);
        let lit = (level.sqrt() * 5.0).round() as usize;
        let meter = div()
            .flex_none()
            .flex()
            .items_end()
            .gap(px(2.0))
            .h(px(18.0))
            .children((0..5).map(|k| {
                let h = 6.0 + k as f32 * 3.0;
                div().w(px(3.0)).h(px(h)).rounded(px(1.0)).bg(if k < lit {
                    if k >= 4 {
                        Colors::meter_high()
                    } else if k >= 3 {
                        Colors::meter_mid()
                    } else {
                        Colors::meter_low()
                    }
                } else {
                    Colors::with_alpha(Colors::text_primary(), 0.14)
                })
            }));
        let play_hover = Colors::accent_primary_hover();
        let play = div()
            .id("voxtune-play")
            .tooltip(fb_tooltip(if self.host.controls_transport() {
                "Play / pause (Space)"
            } else {
                "Follows the host's transport"
            }))
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .size(px(48.0))
            .rounded(px(radius::PILL))
            .bg(if controls {
                accent
            } else {
                Colors::with_alpha(accent, 0.35)
            })
            .child(icon(
                if t.playing { icons::PAUSE } else { icons::PLAY },
                20.0,
                Colors::text_inverse(),
            ))
            .when(controls, |d| {
                d.cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(play_hover))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx)))
            });
        let bypass_hue = Colors::status_warning();
        let bypass_rest = if self.bypass {
            Colors::with_alpha(bypass_hue, 0.16)
        } else {
            Colors::surface_raised()
        };
        let bypass_hover = Colors::composite(bypass_rest, Colors::state_hover());
        let bypass = div()
            .id("voxtune-bypass")
            .flex_none()
            .tooltip(fb_tooltip("Hear the original take (A/B)"))
            .flex()
            .items_center()
            .gap(px(space::SNUG))
            .h(px(size::PROMINENT + 4.0))
            .px(px(space::LOOSE))
            .rounded(px(radius::PILL))
            .bg(bypass_rest)
            .border(px(1.0))
            .border_color(if self.bypass {
                Colors::with_alpha(bypass_hue, 0.6)
            } else {
                Colors::border_normal()
            })
            .child(icon(
                icons::EAR,
                14.0,
                if self.bypass {
                    bypass_hue
                } else {
                    Colors::text_secondary()
                },
            ))
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if self.bypass {
                        bypass_hue
                    } else {
                        Colors::text_secondary()
                    })
                    .child("Bypass"),
            )
            .when(self.has_audio(), |d| {
                d.cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(bypass_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.run_command("original", cx);
                    }))
            });
        div()
            .flex()
            .items_center()
            .gap(px(space::BASE))
            .h(px(76.0))
            .px(px(space::LOOSE))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(20.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(Colors::text_primary())
                            .child(format_clock(t.position)),
                    )
                    .child(
                        div()
                            .text_size(px(typography::DENSE_LABEL))
                            .text_color(Colors::text_muted())
                            .child(format!(
                                "{}  {:.0} BPM",
                                format_position(t.position as f32, doc.bpm),
                                doc.bpm
                            )),
                    ),
            )
            .child(meter)
            .child(div().flex_1())
            .child(self.icon_button(
                "voxtune-home",
                icons::TO_START,
                "To start (Home)",
                None,
                accent,
                self.has_audio(),
                cx,
                |this, cx| {
                    this.host.seek(0.0, cx);
                    this.view.t0 = 0.0;
                },
            ))
            .child(play)
            .when_some(self.host.looping(cx), |d, on| {
                d.child(self.icon_button(
                    "voxtune-loop",
                    icons::LOOP,
                    "Loop the take",
                    Some(on),
                    accent,
                    true,
                    cx,
                    move |this, cx| this.host.set_looping(!on, cx),
                ))
            })
            .child(div().w(px(space::TIGHT)))
            .child(bypass)
            .into_any_element()
    }

    fn page_header(&self, title: &'static str, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(space::SNUG))
            .child(self.icon_button(
                "voxtune-page-back",
                icons::BACK,
                "Back",
                None,
                Colors::accent_primary(),
                true,
                cx,
                |this, _| this.panel = PanelTab::Main,
            ))
            .child(
                div()
                    .text_size(px(typography::UI_MD + 2.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(Colors::text_primary())
                    .child(title),
            )
            .into_any_element()
    }

    /// One ornament tile, drawing the pitch shape the style makes.
    fn ornament_tile(&self, style: u8, enabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let current = self
            .selected_note()
            .is_some_and(|n| !n.unpitched && n.orn_style == style);
        let demo = demo_note(style);
        let stroke_color = if current {
            Colors::accent_primary()
        } else {
            Colors::text_secondary()
        };
        let shape = gpui::canvas(
            |_, _, _| {},
            move |b: Bounds<Pixels>, (), window, _| {
                let w = f32::from(b.size.width);
                let h = f32::from(b.size.height);
                let mid = h * 0.55;
                let scale = h * 0.32;
                let mut path = gpui::PathBuilder::stroke(px(1.6));
                for k in 0..=48 {
                    let t = k as f32 / 48.0;
                    let y = mid - sphere_voxtune::engine::ornament_semis(&demo, t) * scale;
                    let at = b.origin + gpui::point(px(t * w), px(y));
                    if k == 0 {
                        path.move_to(at);
                    } else {
                        path.line_to(at);
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, stroke_color);
                }
            },
        )
        .w_full()
        .h(px(26.0));
        let base = Colors::surface_input();
        let rest = if current {
            Colors::composite(base, Colors::with_alpha(Colors::accent_primary(), 0.16))
        } else {
            base
        };
        let hover = Colors::composite(rest, Colors::state_hover());
        div()
            .id(("voxtune-orn", style as usize))
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .p(px(space::SNUG))
            .rounded(px(radius::SURFACE))
            .bg(rest)
            .border(px(1.0))
            .border_color(if current {
                Colors::with_alpha(Colors::accent_primary(), 0.7)
            } else {
                Colors::border_subtle()
            })
            .child(if style == ORN_NONE {
                div()
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(icons::NONE, 15.0, stroke_color))
                    .into_any_element()
            } else {
                shape.into_any_element()
            })
            .child(
                div()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(if current {
                        Colors::text_primary()
                    } else {
                        Colors::text_muted()
                    })
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(ornament_name(style)),
            )
            .when(!enabled, |d| d.opacity(0.4))
            .when(enabled, |d| {
                d.cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = this.selected.filter(|_| this.editable()) {
                            this.session.set_ornament(i, style);
                            this.changed(cx);
                        }
                    }))
            })
            .into_any_element()
    }

    fn render_expression_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let pitched = self.selected_note().is_some_and(|n| !n.unpitched) && self.editable();
        let has_expression = self
            .selected_note()
            .is_some_and(|n| n.orn_style != ORN_NONE || n.accent > 0.0);
        let row = |styles: &[u8], this: &Self, cx: &mut Context<Self>| {
            div()
                .flex()
                .gap(px(space::SNUG))
                .children(
                    styles
                        .iter()
                        .map(|&s| this.ornament_tile(s, pitched, cx))
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
        };
        vec![
            self.page_header("Ornament & accent", cx),
            Self::caption("ORNAMENT").into_any_element(),
            row(&[ORN_NONE, ORN_TURN_UP, ORN_TURN_DOWN], self, cx),
            row(&[ORN_SHAKE, ORN_FALL], self, cx),
            self.slider_row(Param::OrnDepth, "WIDTH", cx, None),
            self.slider_row(Param::OrnSpeed, "SPEED", cx, None),
            Self::caption("ACCENT").into_any_element(),
            self.slider_row(Param::Accent, "ACCENT", cx, None),
            div()
                .flex()
                .child(fb_button(
                    "voxtune-expr-clear",
                    "Clear ornament and accent",
                    FbButtonKind::Default,
                    pitched && has_expression,
                    cx.listener(|this, _, _, cx| {
                        if let Some(i) = this.selected {
                            this.session.clear_expression(i);
                            this.changed(cx);
                        }
                    }),
                ))
                .into_any_element(),
        ]
    }

    fn render_advanced_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let editable = self.editable();
        let can_macro = editable && (!self.macro_selected_only || self.selected.is_some());
        let amount = self.macro_amount;
        let grid_beats = self.macro_grid;
        let seg = |id: (&'static str, usize),
                   label: &'static str,
                   active: bool,
                   pos: FbSegment,
                   cx: &mut Context<Self>,
                   f: Box<dyn Fn(&mut Self)>| {
            div()
                .flex_1()
                .child(fb_segment(
                    id,
                    label,
                    active,
                    pos,
                    cx.listener(move |this, _, _, cx| {
                        f(this);
                        cx.notify();
                    }),
                ))
                .into_any_element()
        };
        let scope = fb_segmented_track()
            .child(seg(
                ("voxtune-scope", 0),
                "All notes",
                !self.macro_selected_only,
                FbSegment::First,
                cx,
                Box::new(|this| this.macro_selected_only = false),
            ))
            .child(seg(
                ("voxtune-scope", 1),
                "Selected note",
                self.macro_selected_only,
                FbSegment::Last,
                cx,
                Box::new(|this| this.macro_selected_only = true),
            ));
        let grid = fb_segmented_track().children(
            [(1.0, "1/4"), (0.5, "1/8"), (0.25, "1/16")]
                .into_iter()
                .enumerate()
                .map(|(k, (g, label))| {
                    seg(
                        ("voxtune-grid", k),
                        label,
                        (self.macro_grid - g).abs() < 1e-4,
                        match k {
                            0 => FbSegment::First,
                            2 => FbSegment::Last,
                            _ => FbSegment::Middle,
                        },
                        cx,
                        Box::new(move |this| this.macro_grid = g),
                    )
                })
                .collect::<Vec<_>>(),
        );
        let tile = |id: &'static str,
                    glyph: &'static str,
                    label: &'static str,
                    detail: &'static str,
                    op: Macro,
                    cx: &mut Context<Self>| {
            let base = Colors::surface_input();
            let hover = Colors::composite(base, Colors::state_hover());
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(space::BASE))
                .px(px(space::BASE))
                .h(px(48.0))
                .rounded(px(radius::SURFACE))
                .bg(base)
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(30.0))
                        .rounded(px(radius::CONTROL))
                        .bg(Colors::with_alpha(Colors::accent_primary(), 0.14))
                        .child(icon(glyph, 15.0, Colors::accent_primary())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .text_size(px(typography::UI_SM))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(typography::DENSE_LABEL))
                                .text_color(Colors::text_muted())
                                .child(detail),
                        ),
                )
                .when(!can_macro, |d| d.opacity(0.4))
                .when(can_macro, |d| {
                    d.cursor(gpui::CursorStyle::PointingHand)
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(move |this, _, _, cx| this.run_macro(op, cx)))
                })
                .into_any_element()
        };
        let transpose = div().flex().gap(px(space::SNUG)).children(
            [(-12, "−12"), (-1, "−1"), (1, "+1"), (12, "+12")]
                .into_iter()
                .map(|(semitones, label)| {
                    div()
                        .flex_1()
                        .child(fb_button(
                            ("voxtune-transpose", (semitones + 12) as usize),
                            label,
                            FbButtonKind::Default,
                            can_macro,
                            cx.listener(move |this, _, _, cx| {
                                this.run_macro(Macro::Transpose { semitones }, cx)
                            }),
                        ))
                        .into_any_element()
                })
                .collect::<Vec<_>>(),
        );
        let mut out = vec![
            self.page_header("Advanced tools", cx),
            Self::caption("APPLY TO").into_any_element(),
            scope.into_any_element(),
            self.slider_row(Param::MacroAmount, "AMOUNT", cx, None),
            tile(
                "voxtune-m-center",
                icons::CENTER,
                "Center pitch",
                "Pull notes toward the scale",
                Macro::Center { amount },
                cx,
            ),
            tile(
                "voxtune-m-drift",
                icons::FLATTEN,
                "Flatten drift",
                "Steady the sung wobble",
                Macro::Drift { amount },
                cx,
            ),
            tile(
                "voxtune-m-quantize",
                icons::QUANTIZE,
                "Quantize timing",
                "Pull note starts to the grid",
                Macro::Quantize { amount, grid_beats },
                cx,
            ),
            grid.into_any_element(),
            tile(
                "voxtune-m-level",
                icons::LEVEL,
                "Even out level",
                "Bring notes to one level",
                Macro::Level { amount },
                cx,
            ),
            tile(
                "voxtune-m-accent",
                icons::ACCENT,
                "Accent on beats",
                "Lift notes that start on a beat",
                Macro::AccentBeats { amount },
                cx,
            ),
            Self::caption("TRANSPOSE").into_any_element(),
            transpose.into_any_element(),
        ];
        if let Some(status) = self.status.clone() {
            out.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(space::SNUG))
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_secondary())
                    .child(icon(icons::CHECK, 13.0, Colors::status_success()))
                    .child(status)
                    .into_any_element(),
            );
        }
        out
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let shell = div()
            .flex_none()
            .w(px(INSPECTOR_W))
            .h_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_sidebar())
            .border_l(px(1.0))
            .border_color(Colors::border_subtle());
        let page = |children: Vec<AnyElement>| {
            div()
                .id("voxtune-page")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(space::BASE + 2.0))
                        .p(px(space::LOOSE + 2.0))
                        .children(children),
                )
        };
        let body = match self.panel {
            PanelTab::Expression => page(self.render_expression_page(cx)).into_any_element(),
            PanelTab::Advanced => page(self.render_advanced_page(cx)).into_any_element(),
            PanelTab::Main => {
                let tune_all = fb_button(
                    "voxtune-tune-all",
                    "Tune all",
                    FbButtonKind::Default,
                    self.editable()
                        && !self.session.notes().is_empty()
                        && self.session.doc().correct < 1.0,
                    cx.listener(|this, _, _, cx| {
                        this.run_command("tune-all", cx);
                    }),
                )
                .into_any_element();
                let label = match self.tool {
                    VoxTool::Pitch | VoxTool::Cut => "PITCH",
                    VoxTool::Drift => "DRIFT",
                    VoxTool::Time => "TIME",
                    VoxTool::Formant => "FORMANT",
                    VoxTool::Level => "LEVEL",
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(space::BASE + 2.0))
                            .p(px(space::LOOSE + 2.0))
                            .border_b(px(1.0))
                            .border_color(Colors::border_subtle())
                            .child(self.note_header(cx))
                            .child(self.tool_chips(cx))
                            .child(self.slider_row(tool_param(self.tool), label, cx, None))
                            .child(self.slider_row(Param::Correct, "CORRECT", cx, Some(tune_all))),
                    )
                    .child(self.render_history_list(cx))
                    .into_any_element()
            }
        };
        shell
            .child(body)
            .child(self.render_transport(cx))
            .into_any_element()
    }

    fn render_ruler(&self, cx: &mut Context<Self>) -> AnyElement {
        let (w, _) = self.grid_size();
        let bpm = self.session.doc().bpm;
        let lines = grid_lines(&self.view, bpm, w);
        let capture = self.ruler_bounds.clone();
        let tick_major = Colors::timeline_ruler_tick();
        let tick_minor = Colors::with_alpha(Colors::timeline_ruler_tick(), 0.5);
        let ticks = lines.clone();
        let painter = gpui::canvas(
            move |b, _, _| capture.set(Some(b)),
            move |b: Bounds<Pixels>, (), window, _| {
                let h = f32::from(b.size.height);
                for l in &ticks {
                    let (top, color) = if l.bar.is_some() {
                        (0.0, tick_major)
                    } else {
                        (h * 0.6, tick_minor)
                    };
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            b.origin + gpui::point(px(l.x.floor()), px(top)),
                            gpui::size(px(1.0), px(h - top)),
                        ),
                        color,
                    ));
                }
            },
        )
        .absolute()
        .inset_0();
        let labels: Vec<AnyElement> = lines
            .iter()
            .filter_map(|l| {
                let bar = l.bar?;
                (l.x >= -2.0 && l.x < w - 12.0).then(|| {
                    div()
                        .absolute()
                        .left(px(l.x + 3.0))
                        .top(px(2.0))
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::timeline_ruler_text())
                        .child(bar.to_string())
                        .into_any_element()
                })
            })
            .collect();
        let play_x = self.view.time_to_x(self.last_transport.position as f32);
        div()
            .id("voxtune-ruler")
            .relative()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .bg(Colors::surface_panel())
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .cursor(gpui::CursorStyle::PointingHand)
            .child(painter)
            .children(labels)
            .when(play_x >= 0.0 && play_x <= w, |d| {
                d.child(
                    div()
                        .absolute()
                        .left(px(play_x - 4.0))
                        .bottom_0()
                        .w(px(9.0))
                        .h(px(6.0))
                        .rounded_t(px(radius::MICRO))
                        .bg(Colors::timeline_playhead()),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let Some(b) = this.ruler_bounds.get() else {
                        return;
                    };
                    if !this.has_audio() {
                        return;
                    }
                    this.drag = Some(Drag::Seek);
                    this.seek_to_x(f32::from(ev.position.x - b.origin.x), cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.drag == Some(Drag::Seek) && ev.pressed_button == Some(MouseButton::Left) {
                    let Some(b) = this.ruler_bounds.get() else {
                        return;
                    };
                    this.seek_to_x(f32::from(ev.position.x - b.origin.x), cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.drag == Some(Drag::Seek) {
                        this.drag = None;
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
    }

    /// The pitch keyboard. Labels take the key's own contrast: dark on white
    /// keys, light on black ones, dimmer off the scale.

    fn render_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let card = |children: Vec<AnyElement>| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(320.0))
                        .flex()
                        .flex_col()
                        .gap(px(space::BASE))
                        .p(px(space::SECTION))
                        .rounded(px(radius::SURFACE))
                        .bg(Colors::surface_raised())
                        .border(px(1.0))
                        .border_color(Colors::border_normal())
                        .children(children),
                )
                .into_any_element()
        };
        if let Some(load) = &self.loading {
            let st = &load.job.state;
            let stage = st.stage.load(std::sync::atomic::Ordering::Relaxed);
            let permille = st
                .progress
                .permille
                .load(std::sync::atomic::Ordering::Relaxed);
            let (label, fraction) = match stage {
                STAGE_DECODING => ("Decoding audio…", None),
                STAGE_ANALYZING if st.used_cache.load(std::sync::atomic::Ordering::Relaxed) => {
                    ("Restoring analysis…", None)
                }
                STAGE_ANALYZING => ("Finding notes…", Some(permille as f32 / 1000.0)),
                STAGE_INSTALLING => ("Preparing editor…", Some(1.0)),
                _ => ("Finishing…", Some(1.0)),
            };
            return Some(card(vec![
                div()
                    .text_size(px(typography::UI_MD))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(load.title.clone())
                    .into_any_element(),
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_secondary())
                    .child(label)
                    .when_some(fraction, |d, f| {
                        d.child(format!("{}%", (f * 100.0).round()))
                    })
                    .into_any_element(),
                // Indeterminate stages show an empty track rather than fake motion.
                fb_progress(fraction.unwrap_or(0.0)).into_any_element(),
                div()
                    .flex()
                    .justify_end()
                    .child(fb_button(
                        "voxtune-cancel-load",
                        "Cancel",
                        FbButtonKind::Default,
                        true,
                        cx.listener(|this, _, _, cx| this.cancel_load(cx)),
                    ))
                    .into_any_element(),
            ]));
        }
        if !self.has_audio() {
            let mut children = vec![div()
                .text_size(px(typography::UI_MD))
                .text_color(Colors::text_secondary())
                .child(self.empty_hint.clone())
                .into_any_element()];
            if let Some(err) = &self.error {
                children.push(
                    div()
                        .text_size(px(typography::UI_SM))
                        .text_color(Colors::status_error())
                        .child(err.clone())
                        .into_any_element(),
                );
            }
            return Some(card(children));
        }
        None
    }
}

/// `+31` / `-12` / `0` for a cents value.
fn format_signed_cents(cents: f32) -> String {
    let c = cents.round() as i32;
    if c == 0 {
        "0".into()
    } else {
        format!("{c:+}")
    }
}

impl gpui::Render for VoxtuneEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.fit_pending && self.grid_bounds.get().is_some() {
            self.fit_pending = false;
            self.fit_view();
        }
        #[cfg(feature = "ui-preview")]
        if !self.fit_pending && self.grid_bounds.get().is_some() {
            if let Some((tool, delta)) = self.preview_pending.take() {
                self.preview_gesture(tool, delta, cx);
            }
        }
        let header = self.render_header(cx);
        let toolbar = self.render_toolbar(cx);
        let ruler = self.render_ruler(cx);
        let keys = self.render_keys();
        let grid = self.render_grid(cx);
        let lyrics = self.render_lyrics();
        let overview = self.render_overview(cx);
        let inspector = self.render_inspector(cx);
        let overlay = self.render_overlay(cx);
        let (trailing, notice) = self.host.status(cx);
        let status = div()
            .flex()
            .items_center()
            .gap(px(space::LOOSE))
            .h(px(size::DENSE + 2.0))
            .px(px(space::BASE))
            .bg(Colors::statusbar_bg())
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(typography::UI_XS))
            .text_color(Colors::statusbar_text())
            .when(self.has_audio(), |d| {
                let doc = self.session.doc();
                d.child(format!("{} notes", self.session.notes().len()))
                    .child(format!("{} {}", doc.key_name(), doc.scale.label()))
                    .child(format!("{:.0} BPM", doc.bpm))
                    .child(format!("{:.1} s", self.session.duration()))
            })
            .child(div().flex_1())
            .when_some(notice, |d, (text, warn)| {
                d.child(
                    div()
                        .text_color(if warn {
                            Colors::status_warning()
                        } else {
                            Colors::statusbar_text()
                        })
                        .child(text),
                )
            })
            .when_some(trailing, |d, text| {
                d.child(div().text_color(Colors::statusbar_text_muted()).child(text))
            });

        div()
            .id("voxtune-editor")
            .key_context("VoxtuneEditor")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(header)
                            .child(toolbar)
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .h(px(RULER_H))
                                    .child(
                                        div()
                                            .w(px(KEYS_W))
                                            .h_full()
                                            .bg(Colors::surface_sidebar())
                                            .border_r(px(1.0))
                                            .border_b(px(1.0))
                                            .border_color(Colors::border_subtle()),
                                    )
                                    .child(ruler),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_h_0()
                                    .flex()
                                    .flex_row()
                                    .child(keys)
                                    .child(grid)
                                    .children(overlay),
                            )
                            .child(lyrics)
                            .child(overview),
                    )
                    .child(inspector),
            )
            .child(status)
    }
}
