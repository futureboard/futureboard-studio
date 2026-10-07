//! The native editor window of a built-in EQ insert — EQ-Z8 or EQ-ZX — drawn
//! with GPUI inside the shared [`native_plugin_shell`], replacing both
//! CEF/React `editor/` bundles.
//!
//! It talks to the plug-in through the host ops `plugin_ops.rs` injects, the
//! way the CEF editors did: every edit is a wire param sent with
//! `forward_param` (folded into Studio's state mirror, saved with the
//! project, replayed into a restarted host), computed generically by
//! [`EqParams::wire_diff`]. The analyser reads the insert's spectrum frames;
//! the curves are the DSP crates' own response functions at the host's
//! sample rate.
//!
//! This window owns all editor state and every gesture; `eq_panel` only
//! renders it and hands clicks back through the `*_cb` builders here.
//!
//! Gestures on the graph: drag a numbered node to move it (Shift fine, Alt
//! keeps its gain), double-click empty graph to add a band, double-click a
//! node to flatten it, wheel over a node for Q (or a cut's slope), hold the
//! right button on a node to hear it alone while moving it, Alt-click a
//! node to keep listening. Escape ends an audition; so does closing.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Render,
    ScrollWheelEvent, Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind,
};

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::context_menu::{context_menu_overlay, ContextMenuEntry};
use crate::components::eq_graph::{
    self, freq_at_fraction, node_at, point_value, Curves, DEFAULT_DB_RANGE,
};
use crate::components::eq_model::{
    empty_band, matching_preset, new_band, preset_applied, presets, step_slope, Band, EqKind,
    EqParams, Placement, Shape, GAIN_MAX_DB, GAIN_MIN_DB, Q_MAX, Q_MIN,
};
use crate::components::eq_panel::eq_panel;
use crate::components::native_plugin_shell::{
    native_plugin_shell, NativeBuiltinEditor, ShellIdentity, ShellMeter, SHELL_METER_INTERVAL,
};

const SPECTRUM_BINS: usize = SpherePluginHost::spectrum::SPECTRUM_BINS;
/// How long a passing word ("All 8 bands are in use") stays up.
const NOTICE: Duration = Duration::from_millis(2_200);
/// Drag travel under Shift.
const FINE: f32 = 0.25;
const DEFAULT_SAMPLE_RATE: f32 = 48_000.0;

/// A node being dragged.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Drag {
    pub band: usize,
    /// Where the pointer went down, window space.
    origin: (f32, f32),
    /// The node's place then, as fractions across and down the plot.
    start: (f32, f32),
    /// The solo to put back when a right-button audition ends.
    audition_restores: Option<Option<usize>>,
}

/// The A/B comparison: the snapshot not playing, and which letter plays.
#[derive(Clone, Debug)]
pub(crate) struct Compare {
    pub other: EqParams,
    pub on_b: bool,
}

pub struct EqEditorWindow {
    kind: EqKind,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    pub(crate) params: EqParams,
    pub(crate) curves: Arc<Curves>,
    sample_rate: f32,
    pub(crate) selected: Option<usize>,
    pub(crate) view: Placement,
    pub(crate) db_range: f32,
    pub(crate) show_spectrum: bool,
    pub(crate) show_band_curves: bool,
    pub(crate) spectrum: Option<Arc<[f32; SPECTRUM_BINS]>>,
    spectrum_seq: u32,
    pub(crate) preset: Option<usize>,
    pub(crate) compare: Compare,
    pub(crate) drag: Option<Drag>,
    /// The pointer's place over the plot, as fractions, while it hovers.
    pub(crate) hover: Option<(f32, f32)>,
    notice: Option<(String, Instant)>,
    /// The open preset menu, at a window-space point.
    menu: Option<(f32, f32)>,
    /// The plot's bounds at the last paint: what every gesture is measured
    /// against.
    pub(crate) plot_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    meter: ShellMeter,
    /// Cleared when the window closes, ending the telemetry timer.
    alive: Rc<Cell<bool>>,
}

impl EqEditorWindow {
    pub fn new(
        kind: EqKind,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        on_close: Arc<dyn Fn(&mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let params = EqParams::defaults(kind);
        let mut window = Self {
            kind,
            key,
            identity,
            host_ops,
            on_close,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            compare: Compare {
                other: params.clone(),
                on_b: false,
            },
            params,
            curves: Arc::new(Curves::default()),
            sample_rate: DEFAULT_SAMPLE_RATE,
            selected: None,
            view: Placement::Stereo,
            db_range: DEFAULT_DB_RANGE,
            show_spectrum: true,
            show_band_curves: true,
            spectrum: None,
            spectrum_seq: 0,
            preset: None,
            drag: None,
            hover: None,
            notice: None,
            menu: None,
            plot_bounds: Rc::new(Cell::new(None)),
            meter: ShellMeter::default(),
            alive: Rc::new(Cell::new(true)),
        };
        window.sync_from_mirror(cx);
        window.compare.other = window.params.clone();
        window.start_telemetry(cx);
        window
    }

    pub fn kind(&self) -> EqKind {
        self.kind
    }

    pub fn key(&self) -> &PluginInstanceKey {
        &self.key
    }

    /// Re-reads the insert's params from Studio's state mirror — after an
    /// undo, a preset, or a project reload. A drag in progress keeps the
    /// params it is writing.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            return;
        }
        let insert = &self.key.insert_id;
        self.params = match self.kind {
            EqKind::Z8 => EqParams::Z8(
                crate::components::builtin_plugin_editor::builtin_equz8_params(insert)
                    .unwrap_or_else(equz8::default_params),
            ),
            EqKind::Zx => EqParams::Zx(
                crate::components::builtin_plugin_editor::builtin_equzx_params(insert)
                    .unwrap_or_else(equzx::default_params),
            ),
        };
        self.preset = matching_preset(&self.params);
        self.keep_selection_valid();
        self.refresh_curves();
        cx.notify();
    }

    /// Rebinds the window to another insert, keeping the OS window. An
    /// audition on the insert left behind ends first.
    pub fn rebind(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        if key != self.key {
            self.end_audition(cx);
            self.key = key;
            self.selected = None;
            self.drag = None;
            self.menu = None;
            self.spectrum = None;
            self.spectrum_seq = 0;
        }
        self.identity = identity;
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
        self.compare = Compare {
            other: self.params.clone(),
            on_b: false,
        };
    }

    fn keep_selection_valid(&mut self) {
        let listed = self.params.listed();
        if self.selected.is_some_and(|band| !listed.contains(&band)) {
            self.selected = None;
        }
        // EQ-Z8 always has a band in the editor; EQ-ZX starts on its first.
        if self.selected.is_none() {
            self.selected = listed.first().copied();
        }
    }

    fn refresh_curves(&mut self) {
        let view = if self.params.uses_mid_side() {
            self.view
        } else {
            Placement::Stereo
        };
        self.curves = Arc::new(Curves::compute(
            &self.params,
            view,
            self.selected,
            self.sample_rate,
        ));
    }

    /// The part of the image the graph shows: the view picked, once any
    /// band is mid- or side-only; otherwise the whole, which is the same.
    pub(crate) fn shown_view(&self) -> Placement {
        if self.kind.has_placement() {
            self.view
        } else {
            Placement::Stereo
        }
    }

    /// Sends what changed between the params and `next` as wire edits. Each
    /// one reaches the host DSP, the state mirror, and the project's dirty
    /// flag through `forward_param`.
    pub(crate) fn set_params(&mut self, next: EqParams, cx: &mut Context<Self>) {
        let edits = self.params.wire_diff(&next);
        self.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        self.preset = matching_preset(&self.params);
        self.keep_selection_valid();
        self.refresh_curves();
        cx.notify();
    }

    /// Edits band `index` with `change`. On EQ-Z8 any edit to a band switched
    /// off switches it on: moving a band you cannot hear would be an edit
    /// with no result.
    pub(crate) fn edit_band(
        &mut self,
        index: usize,
        change: impl FnOnce(&mut Band),
        cx: &mut Context<Self>,
    ) {
        let before = self.params.band(index);
        let mut band = before;
        change(&mut band);
        if band == before {
            return;
        }
        if self.kind.edit_switches_on() && !before.active && band.active == before.active {
            band.active = true;
        }
        let mut next = self.params.clone();
        next.set_band(index, band);
        self.set_params(next, cx);
    }

    fn set_solo(&mut self, band: Option<usize>, cx: &mut Context<Self>) {
        if self.params.solo() == band {
            return;
        }
        let mut next = self.params.clone();
        next.set_solo(band);
        self.set_params(next, cx);
    }

    /// Ends any audition: solo is something to listen through, never a
    /// state to leave behind.
    fn end_audition(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.as_mut() {
            drag.audition_restores = None;
        }
        self.set_solo(None, cx);
    }

    fn show_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some((text.into(), Instant::now()));
        cx.notify();
    }

    /// The passing word to show, if one is still up.
    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE)
            .map(|(text, _)| text.as_str())
    }

    pub(crate) fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    // ── Graph gestures ──────────────────────────────────────────────────

    fn plot_fraction(&self, (x, y): (f32, f32)) -> Option<(f32, f32)> {
        let bounds = self.plot_bounds.get()?;
        let fx = (x - f32::from(bounds.origin.x)) / f32::from(bounds.size.width).max(1.0);
        let fy = (y - f32::from(bounds.origin.y)) / f32::from(bounds.size.height).max(1.0);
        ((0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy)).then_some((fx, fy))
    }

    fn node_under(&self, at: (f32, f32)) -> Option<usize> {
        let bounds = self.plot_bounds.get()?;
        node_at(
            &self.params,
            self.shown_view(),
            self.db_range,
            bounds,
            at,
            self.selected,
        )
    }

    pub(crate) fn plot_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let at = (f32::from(event.position.x), f32::from(event.position.y));
        self.menu = None;
        let node = self.node_under(at);
        match (event.button, node) {
            (MouseButton::Left, Some(band)) if event.modifiers.alt => {
                // Alt-click: keep listening to this band, or stop.
                self.selected = Some(band);
                let next = (self.params.solo() != Some(band)).then_some(band);
                self.set_solo(next, cx);
            }
            (MouseButton::Left, Some(band)) if event.click_count >= 2 => {
                self.selected = Some(band);
                self.drag = None;
                self.edit_band(
                    band,
                    |b| {
                        b.gain_db = 0.0;
                        b.q = 1.0;
                    },
                    cx,
                );
            }
            (MouseButton::Left, Some(band)) => self.start_drag(band, at, None, cx),
            (MouseButton::Right, Some(band)) => {
                // Hold to hear the band alone while moving it; the solo
                // before it comes back on release.
                let previous = self.params.solo();
                self.start_drag(band, at, Some(previous), cx);
                self.set_solo(Some(band), cx);
            }
            (MouseButton::Left, None) if event.click_count >= 2 => {
                if let Some(bounds) = self.plot_bounds.get() {
                    let (freq, gain) = point_value(bounds, at, self.db_range);
                    self.add_band(freq, gain, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn start_drag(
        &mut self,
        band: usize,
        origin: (f32, f32),
        audition_restores: Option<Option<usize>>,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(band);
        self.drag = Some(Drag {
            band,
            origin,
            start: eq_graph::node_fractions(&self.params, band, self.db_range),
            audition_restores,
        });
        self.refresh_curves();
        cx.notify();
    }

    fn add_band(&mut self, freq: f32, gain: f32, cx: &mut Context<Self>) {
        let Some(slot) = self.params.free_slot() else {
            let slots = self.kind.slots();
            let text = match self.kind {
                EqKind::Z8 => {
                    format!("All {slots} bands are in use — switch one off to add another")
                }
                EqKind::Zx => format!("All {slots} bands are in use — remove one to add another"),
            };
            self.show_notice(text, cx);
            return;
        };
        let mut band = new_band(self.kind, freq, gain, self.shown_view());
        band.shape = Shape::Bell;
        let mut next = self.params.clone();
        next.set_band(slot, band);
        self.selected = Some(slot);
        self.set_params(next, cx);
    }

    /// The pointer moved anywhere over the window: drag the node it holds,
    /// or follow it over the plot.
    pub(crate) fn mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let at = (f32::from(event.position.x), f32::from(event.position.y));
        let Some(drag) = self.drag else {
            let hover = self.plot_fraction(at);
            if hover != self.hover {
                self.hover = hover;
                cx.notify();
            }
            return;
        };
        if event.pressed_button.is_none() {
            self.end_drag(cx);
            return;
        }
        let Some(bounds) = self.plot_bounds.get() else {
            return;
        };
        let scale = if event.modifiers.shift { FINE } else { 1.0 };
        let dx = (at.0 - drag.origin.0) / f32::from(bounds.size.width).max(1.0) * scale;
        let dy = (at.1 - drag.origin.1) / f32::from(bounds.size.height).max(1.0) * scale;
        let freq = freq_at_fraction(drag.start.0 + dx);
        let gain = eq_graph::db_at_fraction(drag.start.1 + dy, self.db_range)
            .clamp(GAIN_MIN_DB, GAIN_MAX_DB);
        // Alt keeps the gain where it is; an audition moves frequency only,
        // as the old editors did.
        let move_gain = !event.modifiers.alt && drag.audition_restores.is_none();
        self.hover = None;
        self.edit_band(
            drag.band,
            |band| {
                band.freq = freq;
                if move_gain && band.shape.has_gain() {
                    band.gain_db = (gain * 10.0).round() / 10.0;
                }
            },
            cx,
        );
    }

    pub(crate) fn mouse_up(&mut self, button: MouseButton, cx: &mut Context<Self>) {
        let ends = match (button, self.drag) {
            (MouseButton::Right, Some(drag)) => drag.audition_restores.is_some(),
            (MouseButton::Left, Some(drag)) => drag.audition_restores.is_none(),
            _ => false,
        };
        if ends {
            self.end_drag(cx);
        }
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            if let Some(previous) = drag.audition_restores {
                self.set_solo(previous, cx);
            }
            cx.notify();
        }
    }

    pub(crate) fn plot_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let at = (f32::from(event.position.x), f32::from(event.position.y));
        let Some(band) = self.node_under(at).or(self.selected) else {
            return;
        };
        let up = match event.delta {
            gpui::ScrollDelta::Pixels(delta) => f32::from(delta.y),
            gpui::ScrollDelta::Lines(delta) => delta.y,
        };
        if up == 0.0 {
            return;
        }
        let current = self.params.band(band);
        let kind = self.kind;
        if kind.has_slopes() && current.shape.is_cut() {
            // Down steepens a cut, up softens it.
            self.edit_band(band, |b| b.slope = step_slope(b.slope, up < 0.0), cx);
        } else if current.shape.uses_q(kind) {
            let per_notch: f32 = if event.modifiers.shift { 1.03 } else { 1.15 };
            let factor = if up > 0.0 {
                per_notch
            } else {
                per_notch.recip()
            };
            self.edit_band(band, |b| b.q = (b.q * factor).clamp(Q_MIN, Q_MAX), cx);
        }
        self.selected = Some(band);
    }

    pub(crate) fn mouse_left_plot(&mut self, cx: &mut Context<Self>) {
        if self.hover.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let shift = event.keystroke.modifiers.shift;
        match key {
            "escape" => {
                if self.menu.take().is_some() {
                    cx.notify();
                } else if self.params.solo().is_some() {
                    self.end_audition(cx);
                } else if self.kind == EqKind::Zx && self.selected.take().is_some() {
                    self.refresh_curves();
                    cx.notify();
                }
            }
            "delete" | "backspace" => {
                if let Some(band) = self.selected {
                    self.remove_band(band, cx);
                }
            }
            "left" | "right" => {
                if let Some(band) = self.selected {
                    let step: f32 = if shift { 1.01 } else { 1.05 };
                    let factor = if key == "right" { step } else { step.recip() };
                    self.edit_band(band, |b| b.freq *= factor, cx);
                }
            }
            "up" | "down" => {
                if let Some(band) = self.selected {
                    if self.params.band(band).shape.has_gain() {
                        let step = if key == "up" { 0.5 } else { -0.5 };
                        self.edit_band(band, |b| b.gain_db += step, cx);
                    }
                }
            }
            _ => {}
        }
    }

    // ── Band and global edits ───────────────────────────────────────────

    /// Takes band `index` out: EQ-Z8 switches it off, EQ-ZX empties its slot.
    pub(crate) fn remove_band(&mut self, index: usize, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        let mut band = match self.kind {
            EqKind::Z8 => next.band(index),
            EqKind::Zx => empty_band(EqKind::Zx),
        };
        band.active = false;
        next.set_band(index, band);
        if next.solo() == Some(index) {
            next.set_solo(None);
        }
        self.set_params(next, cx);
    }

    pub(crate) fn toggle_band(&mut self, index: usize, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        let mut band = next.band(index);
        band.active = !band.active;
        next.set_band(index, band);
        self.set_params(next, cx);
    }

    pub(crate) fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selected != Some(index) {
            self.selected = Some(index);
            self.refresh_curves();
            cx.notify();
        }
    }

    pub(crate) fn toggle_solo(&mut self, index: usize, cx: &mut Context<Self>) {
        let next = (self.params.solo() != Some(index)).then_some(index);
        self.set_solo(next, cx);
    }

    pub(crate) fn set_output_db(&mut self, db: f32, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        next.set_output_db(db);
        self.set_params(next, cx);
    }

    pub(crate) fn set_mix(&mut self, mix: f32, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        next.set_mix(mix);
        self.set_params(next, cx);
    }

    pub(crate) fn toggle_power(&mut self, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        next.set_power(!next.power());
        self.set_params(next, cx);
    }

    pub(crate) fn set_view(&mut self, view: Placement, cx: &mut Context<Self>) {
        self.view = view;
        self.refresh_curves();
        cx.notify();
    }

    pub(crate) fn set_db_range(&mut self, range: f32, cx: &mut Context<Self>) {
        self.db_range = range;
        cx.notify();
    }

    pub(crate) fn toggle_spectrum(&mut self, cx: &mut Context<Self>) {
        self.show_spectrum = !self.show_spectrum;
        if !self.show_spectrum {
            self.spectrum = None;
        }
        cx.notify();
    }

    pub(crate) fn toggle_band_curves(&mut self, cx: &mut Context<Self>) {
        self.show_band_curves = !self.show_band_curves;
        cx.notify();
    }

    // ── Presets and A/B ─────────────────────────────────────────────────

    /// Loads preset `index` — wrapping, so the arrows cycle — as ordinary
    /// wire edits, so it saves and undoes like any other edit.
    pub(crate) fn load_preset(&mut self, index: isize, cx: &mut Context<Self>) {
        let bank = presets(self.kind);
        let wrapped = index.rem_euclid(bank.len() as isize) as usize;
        let next = preset_applied(&self.params, &bank[wrapped].params);
        self.menu = None;
        self.set_params(next, cx);
        self.preset = Some(wrapped);
    }

    pub(crate) fn step_preset(&mut self, delta: isize, cx: &mut Context<Self>) {
        let from = match self.preset {
            Some(index) => index as isize,
            // Edited: the arrows start from either end of the bank.
            None if delta > 0 => -1,
            None => 0,
        };
        self.load_preset(from + delta, cx);
    }

    pub(crate) fn preset_label(&self) -> String {
        match self.preset {
            Some(index) => presets(self.kind)
                .get(index)
                .map_or("Edited", |preset| preset.name)
                .to_string(),
            None => "Edited".to_string(),
        }
    }

    pub(crate) fn open_preset_menu(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        self.menu = Some((x, y));
        cx.notify();
    }

    fn preset_entries(&self) -> Vec<ContextMenuEntry> {
        let mut entries = vec![ContextMenuEntry::Header(format!(
            "{} presets",
            self.kind.title()
        ))];
        entries.extend(
            presets(self.kind)
                .iter()
                .enumerate()
                .map(|(index, preset)| {
                    ContextMenuEntry::checked_item(
                        preset.name,
                        format!("preset:{index}"),
                        self.preset == Some(index),
                    )
                }),
        );
        entries
    }

    fn run_menu_command(&mut self, command: &str, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some(index) = command
            .strip_prefix("preset:")
            .and_then(|index| index.parse::<isize>().ok())
        {
            self.load_preset(index, cx);
        }
        cx.notify();
    }

    /// Switches between A and B: what plays parks, the other plays, sent as
    /// wire edits. Listening state — power, solo — stays as it is.
    pub(crate) fn swap_compare(&mut self, cx: &mut Context<Self>) {
        let mut next = self.compare.other.clone();
        next.set_power(self.params.power());
        next.set_solo(None);
        self.compare.other = self.params.clone();
        self.compare.on_b = !self.compare.on_b;
        self.set_params(next, cx);
    }

    /// Copies what plays into the other letter.
    pub(crate) fn copy_compare(&mut self, cx: &mut Context<Self>) {
        self.compare.other = self.params.clone();
        self.show_notice(
            if self.compare.on_b {
                "Copied B to A"
            } else {
                "Copied A to B"
            },
            cx,
        );
    }

    // ── Telemetry ───────────────────────────────────────────────────────

    /// Polls the shell meter, the analyser and the host's sample rate at
    /// the CEF editors' telemetry rate, redrawing only when one moves.
    fn start_telemetry(&mut self, cx: &mut Context<Self>) {
        let alive = self.alive.clone();
        cx.spawn(async move |this, cx| {
            while alive.get() {
                cx.background_executor().timer(SHELL_METER_INTERVAL).await;
                let keep = this.update(cx, |this, cx| this.poll_telemetry(cx));
                if keep.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn poll_telemetry(&mut self, cx: &mut Context<Self>) {
        let key = self.key.clone();
        let mut moved = self.meter.poll(self.host_ops.meter_source.as_ref(), &key);
        if let Some((rate, ..)) = self
            .host_ops
            .host_status_source
            .as_ref()
            .and_then(|source| source(&key))
        {
            let rate = rate as f32;
            if rate > 0.0 && rate != self.sample_rate {
                self.sample_rate = rate;
                self.refresh_curves();
                moved = true;
            }
        }
        let frame = self
            .show_spectrum
            .then(|| self.host_ops.spectrum_source.as_ref())
            .flatten()
            .and_then(|source| source(&key));
        if let Some((seq, bins)) = frame {
            if seq != self.spectrum_seq {
                self.spectrum_seq = seq;
                self.spectrum = Some(Arc::new(bins));
                moved = true;
            }
        }
        // A notice that has just run out disappears without waiting for
        // the next edit.
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= NOTICE)
        {
            self.notice = None;
            moved = true;
        }
        if moved {
            cx.notify();
        }
    }

    // ── Click handlers for the panel ────────────────────────────────────

    /// A click handler that runs `action` on this window.
    pub(crate) fn click_cb(
        &self,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| action(this, cx));
        }
    }

    /// A knob handler that runs `action` with the knob's value.
    pub(crate) fn value_cb(
        &self,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, f32, &mut Context<Self>) + 'static,
    ) -> impl Fn(&f32, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |value: &f32, _window, app: &mut App| {
            let value = *value;
            let _ = entity.update(app, |this, cx| action(this, value, cx));
        }
    }

    /// A knob handler that writes the knob's value into the selected band.
    pub(crate) fn band_cb(
        &self,
        cx: &Context<Self>,
        index: usize,
        apply: impl Fn(&mut Band, f32) + 'static,
    ) -> impl Fn(&f32, &mut Window, &mut App) + 'static {
        let apply = Rc::new(apply);
        self.value_cb(cx, move |this, value, cx| {
            let apply = apply.clone();
            this.edit_band(index, move |band| apply(band, value), cx);
        })
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        // An audition never outlives the editor that started it.
        self.drag = None;
        self.end_audition(cx);
        self.alive.set(false);
    }
}

impl NativeBuiltinEditor for EqEditorWindow {
    fn rebind_insert(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        self.rebind(key, identity, host_ops, cx);
    }
}

impl Render for EqEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let entity = cx.entity().clone();
        let on_close = self.on_close.clone();
        let content = eq_panel(self, cx);

        let menu = self.menu.map(|(x, y)| {
            let viewport = window.viewport_size();
            let command_target = entity.clone();
            let close_target = entity.clone();
            context_menu_overlay(
                self.preset_entries(),
                x,
                y,
                viewport.width.into(),
                viewport.height.into(),
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let _ =
                        command_target.update(cx, |this, cx| this.run_menu_command(&command, cx));
                }),
                Arc::new(move |_: &(), _window, cx| {
                    let _ = close_target.update(cx, |this, cx| {
                        this.menu = None;
                        cx.notify();
                    });
                }),
            )
        });

        div()
            .relative()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _window, cx| this.on_key_down(event, cx)),
            )
            // A node drag follows the pointer anywhere in the window and
            // ends wherever the button comes up.
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _window, cx| this.mouse_move(event, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _window, cx| this.mouse_up(MouseButton::Left, cx)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, _, _window, cx| this.mouse_up(MouseButton::Right, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _window, cx| this.mouse_up(MouseButton::Left, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Right,
                cx.listener(|this, _, _window, cx| this.mouse_up(MouseButton::Right, cx)),
            )
            .child(native_plugin_shell(
                "eq-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    // End any audition and the telemetry timer, drop the
                    // window from Studio's map, then actually close it.
                    let _ = entity.update(cx, |this, cx| this.close(cx));
                    on_close(window, cx);
                    window.remove_window();
                },
                content,
            ))
            .children(menu)
    }
}

pub const EQ_WINDOW_MIN_WIDTH: f32 = 760.0;
pub const EQ_WINDOW_MIN_HEIGHT: f32 = 560.0;

fn open_eq_editor(
    kind: EqKind,
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<EqEditorWindow>, String> {
    let (width, height) = match kind {
        EqKind::Z8 => (980.0, 720.0),
        EqKind::Zx => (1_080.0, 740.0),
    };
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(width), px(height)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(px(EQ_WINDOW_MIN_WIDTH), px(EQ_WINDOW_MIN_HEIGHT)));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| EqEditorWindow::new(kind, key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

/// Opens with a track's EQ-Z8 insert.
pub fn open_equz8_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<EqEditorWindow>, String> {
    open_eq_editor(
        EqKind::Z8,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

/// Opens with a track's EQ-ZX insert.
pub fn open_equzx_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<EqEditorWindow>, String> {
    open_eq_editor(
        EqKind::Zx,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}
