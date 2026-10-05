//! The native editor window of a built-in time effect — VerbSpace or
//! EchoSpace — drawn with GPUI inside the shared [`native_plugin_shell`],
//! replacing both CEF/Svelte `editorui/` bundles.
//!
//! It talks to the plug-in through the host ops `plugin_ops.rs` injects:
//! every edit is a wire param sent with `forward_param` (folded into
//! Studio's state mirror, saved with the project, replayed into a restarted
//! host), computed by [`FxParams::wire_diff`]. The displays are the DSP
//! crates' own models — `verbspace::decay_profile`, `echospace::echo_pattern`
//! and the cut filters' responses — at the host's sample rate and tempo.
//!
//! This window owns all editor state; `fx_panel` only renders it and hands
//! clicks back through the `*_cb` builders here.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Pixels, Render, Styled, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::context_menu::{context_menu_overlay, ContextMenuEntry};
use crate::components::fx_model::{
    matching_preset, preset_applied, presets, FxKind, FxParams, KnobSpec,
};
use crate::components::fx_panel::fx_panel;
use crate::components::native_plugin_shell::{
    native_plugin_shell, NativeBuiltinEditor, ShellIdentity, ShellMeter, SHELL_METER_INTERVAL,
};

/// How long a passing word ("Copied A to B") stays up.
const NOTICE: Duration = Duration::from_millis(2_200);
const DEFAULT_SAMPLE_RATE: f32 = 48_000.0;

/// Points across a frequency curve.
pub(crate) const CURVE_POINTS: usize = 96;
/// Lowest repeat the echo display draws.
pub(crate) const ECHO_FLOOR_DB: f32 = -48.0;
const MAX_ECHOES: usize = 96;
/// Passes the echo display draws the tone of: how the repeats darken.
pub(crate) const TONE_PASSES: [u32; 4] = [1, 2, 4, 8];

/// What the display shows, computed from the params at the host's rate and
/// tempo whenever one of them moves — never per frame.
#[derive(Debug, Clone)]
pub enum FxView {
    Verb {
        profile: verbspace::DecayProfile,
        /// RT60 across the frequency axis, one per curve point.
        rt: Vec<f32>,
        /// The wet cuts across the frequency axis, in dB.
        cuts: Vec<f32>,
    },
    Echo {
        echoes: Vec<echospace::Echo>,
        /// The tone after each of [`TONE_PASSES`], in dB per curve point.
        tones: Vec<Vec<f32>>,
        /// The two lines' times as they run now.
        times_ms: (f32, f32),
        tempo_bpm: f32,
    },
}

/// The frequency of curve point `i`, on the shared log axis.
pub(crate) fn curve_hz(i: usize) -> f32 {
    crate::components::eq_graph::freq_at_fraction(i as f32 / (CURVE_POINTS - 1) as f32)
}

impl FxView {
    fn compute(params: &FxParams, sample_rate: f32, tempo_bpm: f32) -> Self {
        match params {
            FxParams::Verb(p) => {
                let profile = verbspace::decay_profile(p, sample_rate);
                FxView::Verb {
                    rt: (0..CURVE_POINTS)
                        .map(|i| profile.rt_at(curve_hz(i)))
                        .collect(),
                    cuts: (0..CURVE_POINTS)
                        .map(|i| verbspace::wet_filter_response_db(p, curve_hz(i), sample_rate))
                        .collect(),
                    profile,
                }
            }
            FxParams::Echo(p) => {
                let once: Vec<f32> = (0..CURVE_POINTS)
                    .map(|i| echospace::tone_response_db(p, curve_hz(i), sample_rate))
                    .collect();
                FxView::Echo {
                    echoes: echospace::echo_pattern(
                        p,
                        tempo_bpm,
                        sample_rate,
                        ECHO_FLOOR_DB,
                        MAX_ECHOES,
                    ),
                    tones: TONE_PASSES
                        .iter()
                        .map(|passes| once.iter().map(|db| db * *passes as f32).collect())
                        .collect(),
                    times_ms: (
                        p.effective_time_ms_l(tempo_bpm),
                        p.effective_time_ms_r(tempo_bpm),
                    ),
                    tempo_bpm,
                }
            }
        }
    }
}

/// The A/B comparison: the snapshot not playing, and which letter plays.
#[derive(Clone, Debug)]
pub(crate) struct Compare {
    pub other: FxParams,
    pub on_b: bool,
}

pub struct FxEditorWindow {
    kind: FxKind,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    pub(crate) params: FxParams,
    pub(crate) view: Arc<FxView>,
    sample_rate: f32,
    tempo_bpm: f32,
    pub(crate) preset: Option<usize>,
    pub(crate) compare: Compare,
    notice: Option<(String, Instant)>,
    /// The open preset menu, at a window-space point.
    menu: Option<(f32, f32)>,
    meter: ShellMeter,
    /// Cleared when the window closes, ending the telemetry timer.
    alive: Rc<Cell<bool>>,
    /// The display's bounds at the last paint.
    pub(crate) display_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl FxEditorWindow {
    pub fn new(
        kind: FxKind,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        on_close: Arc<dyn Fn(&mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let params = FxParams::defaults(kind);
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
            view: Arc::new(FxView::compute(
                &params,
                DEFAULT_SAMPLE_RATE,
                echospace::DEFAULT_TEMPO_BPM,
            )),
            params,
            sample_rate: DEFAULT_SAMPLE_RATE,
            tempo_bpm: echospace::DEFAULT_TEMPO_BPM,
            preset: None,
            notice: None,
            menu: None,
            meter: ShellMeter::default(),
            alive: Rc::new(Cell::new(true)),
            display_bounds: Rc::new(Cell::new(None)),
        };
        window.read_host_status();
        window.sync_from_mirror(cx);
        window.compare.other = window.params.clone();
        window.start_telemetry(cx);
        window
    }

    pub fn kind(&self) -> FxKind {
        self.kind
    }

    pub fn key(&self) -> &PluginInstanceKey {
        &self.key
    }

    /// Re-reads the insert's params from Studio's state mirror — after an
    /// undo, a preset, or a project reload.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        let insert = &self.key.insert_id;
        self.params = match self.kind {
            FxKind::Verb => FxParams::Verb(
                crate::components::builtin_plugin_editor::builtin_verbspace_params(insert)
                    .unwrap_or_else(verbspace::default_params),
            ),
            FxKind::Echo => FxParams::Echo(
                crate::components::builtin_plugin_editor::builtin_echospace_params(insert)
                    .unwrap_or_else(echospace::default_params),
            ),
        };
        self.preset = matching_preset(&self.params);
        self.refresh_view();
        cx.notify();
    }

    /// Rebinds the window to another insert, keeping the OS window.
    pub fn rebind(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        if key != self.key {
            self.key = key;
            self.menu = None;
        }
        self.identity = identity;
        self.host_ops = host_ops;
        self.read_host_status();
        self.sync_from_mirror(cx);
        self.compare = Compare {
            other: self.params.clone(),
            on_b: false,
        };
    }

    fn refresh_view(&mut self) {
        self.view = Arc::new(FxView::compute(
            &self.params,
            self.sample_rate,
            self.tempo_bpm,
        ));
    }

    /// Sends what changed between the params and `next` as wire edits. Each
    /// one reaches the host DSP, the state mirror, and the project's dirty
    /// flag through `forward_param`.
    pub(crate) fn set_params(&mut self, next: FxParams, cx: &mut Context<Self>) {
        let edits = self.params.wire_diff(&next);
        if edits.is_empty() {
            return;
        }
        self.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        self.preset = matching_preset(&self.params);
        self.refresh_view();
        cx.notify();
    }

    /// Sets one param by id, the way the DSP will take it.
    pub(crate) fn set_value(&mut self, id: &str, value: f32, cx: &mut Context<Self>) {
        let next = self.params.with(id, value);
        self.set_params(next, cx);
    }

    pub(crate) fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        let on = self.params.flag(id);
        self.set_value(id, if on { 0.0 } else { 1.0 }, cx);
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

    pub(crate) fn tempo_bpm(&self) -> f32 {
        self.tempo_bpm
    }

    pub(crate) fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && self.menu.take().is_some() {
            cx.notify();
        }
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
        cx.notify();
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
    /// wire edits. Power and freeze stay as they are.
    pub(crate) fn swap_compare(&mut self, cx: &mut Context<Self>) {
        let next = preset_applied(&self.params, &self.compare.other);
        self.compare.other = self.params.clone();
        self.compare.on_b = !self.compare.on_b;
        self.set_params(next, cx);
        cx.notify();
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

    /// Picks up the host's sample rate and tempo. True when either moved.
    fn read_host_status(&mut self) -> bool {
        let Some((rate, _, _, tempo)) = self
            .host_ops
            .host_status_source
            .as_ref()
            .and_then(|source| source(&self.key))
        else {
            return false;
        };
        let mut moved = false;
        let rate = rate as f32;
        if rate > 0.0 && rate != self.sample_rate {
            self.sample_rate = rate;
            moved = true;
        }
        let tempo = tempo as f32;
        if tempo.is_finite() && tempo > 0.0 && (tempo - self.tempo_bpm).abs() > 1.0e-3 {
            self.tempo_bpm = tempo;
            moved = true;
        }
        moved
    }

    /// Polls the shell meter and the host status at the shell's telemetry
    /// rate, redrawing only when one moves.
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
        if self.read_host_status() {
            self.refresh_view();
            moved = true;
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

    // ── Handlers for the panel ──────────────────────────────────────────

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

    /// A knob handler: the widget's value, through `knob`'s taper, into its
    /// param.
    pub(crate) fn knob_cb(
        &self,
        cx: &Context<Self>,
        knob: KnobSpec,
    ) -> impl Fn(&f32, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |units: &f32, _window, app: &mut App| {
            let value = knob.from_knob(*units);
            let _ = entity.update(app, |this, cx| this.set_value(knob.id, value, cx));
        }
    }

    fn close(&mut self) {
        self.alive.set(false);
    }
}

impl NativeBuiltinEditor for FxEditorWindow {
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

impl Render for FxEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let entity = cx.entity().clone();
        let on_close = self.on_close.clone();
        let content = fx_panel(self, cx);

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
            .child(native_plugin_shell(
                "fx-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    // End the telemetry timer, drop the window from Studio's
                    // map, then actually close it.
                    let _ = entity.update(cx, |this, _cx| this.close());
                    on_close(window, cx);
                    window.remove_window();
                },
                content,
            ))
            .children(menu)
    }
}

pub const FX_WINDOW_MIN_WIDTH: f32 = 720.0;
pub const FX_WINDOW_MIN_HEIGHT: f32 = 520.0;

/// The size each editor opens at.
pub fn fx_window_size(kind: FxKind) -> (f32, f32) {
    match kind {
        // Wide enough for every knob card on one row.
        FxKind::Verb => (980.0, 600.0),
        FxKind::Echo => (1_040.0, 620.0),
    }
}

fn open_fx_editor(
    kind: FxKind,
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<FxEditorWindow>, String> {
    let (width, height) = fx_window_size(kind);
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
    options.window_min_size = Some(size(px(FX_WINDOW_MIN_WIDTH), px(FX_WINDOW_MIN_HEIGHT)));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| FxEditorWindow::new(kind, key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

/// Opens with a track's VerbSpace insert.
pub fn open_verbspace_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<FxEditorWindow>, String> {
    open_fx_editor(
        FxKind::Verb,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

/// Opens with a track's EchoSpace insert.
pub fn open_echospace_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<FxEditorWindow>, String> {
    open_fx_editor(
        FxKind::Echo,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}
