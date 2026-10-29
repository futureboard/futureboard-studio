//! Rodhareist's native editor view, after its design: the signal chain in
//! the well — input and output meters, the path's blocks, the selected
//! block's caption — and four pages on the deck under it:
//!
//! * **Home** — the factory presets, and a performance view: the preset,
//!   four footswitches over chain blocks, the tuner, tap tempo and master;
//! * **Explore** — every block model by family, searchable, placed into
//!   the selected slot;
//! * **Pedalboard** — the selected slot: its model, its knobs, moving or
//!   removing it, its bypass switch, and the NAM capture or IR it loads;
//! * **Settings** — the host's audio format, input trim and power, the
//!   footswitch assignments, display options, and what is loaded.
//!
//! Owns no plug-in state: it reads the window's [`Params`] and telemetry and
//! hands every edit back through the window as wire edits, exactly as the
//! previous editor did. What the design shows for a hardware unit and the
//! plug-in does not have — sample-rate, buffer and impedance choices, I/O
//! routing, MIDI — is not drawn; the host's format is shown read-only.
//!
//! The design is laid out at 1024 × 768 and scaled to the window by the
//! plate kit (`plate`), in its own palette and in Outfit / IBM Plex Mono.

use std::time::{Duration, Instant};

use gpui::{
    div, px, AnyElement, App, Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Rgba, StatefulInteractiveElement, Styled, Window,
};
use rodharerist::{CabModel, Params, StageKind, ToneEngineKind};

use crate::components::controls::fb_tooltip;
use crate::components::plate::{
    self, caption, color, color_a, icon, knob, numbers, plate_frame, plate_scale, type_px, words,
    Dial, DialUnit, KnobCell, KnobLook, FAINT, INK, PALE, SOFT, WHITE,
};
use crate::components::rodhareist_blocks::{self as blocks, Category, ParamSpec};
use crate::components::rodhareist_window::RodhareistEditorWindow;

type Cx<'a> = Context<'a, RodhareistEditorWindow>;

/// Rodhareist's accent.
const ACCENT: u32 = 0xf25c54;
/// Chain geometry, at the design size: where the path starts, its width,
/// one block and the pitch between blocks.
const CHAIN_LEFT: f32 = 108.0;
const CHAIN_W: f32 = 795.0;
const BLOCK: f32 = 99.0;
const PITCH: f32 = 116.0;
/// Positions the chain shows before it scrolls.
const CHAIN_VISIBLE: usize = 7;
/// Taps further apart than this start a new tempo.
const TAP_WINDOW: Duration = Duration::from_millis(2_000);
const TAP_MIN: Duration = Duration::from_millis(250);

fn accent() -> Rgba {
    color(ACCENT)
}

// ── Editor-local view state ────────────────────────────────────────────────

/// Which chain slot the edit page shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainFocus {
    /// The block in this chain position.
    Block(StageKind),
    /// An empty position, waiting for a block.
    Empty(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Home,
    Explore,
    Pedalboard,
    Settings,
}

impl Page {
    const ALL: [(Page, &'static str); 4] = [
        (Page::Home, "Home"),
        (Page::Explore, "Explore"),
        (Page::Pedalboard, "Pedalboard"),
        (Page::Settings, "Settings"),
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum SettingsSection {
    #[default]
    Audio,
    Footswitches,
    Display,
    About,
}

impl SettingsSection {
    const ALL: [(SettingsSection, &'static str); 4] = [
        (SettingsSection::Audio, "Audio"),
        (SettingsSection::Footswitches, "Footswitches"),
        (SettingsSection::Display, "Display"),
        (SettingsSection::About, "About"),
    ];
}

/// What the editor shows and how — never saved, never sent to the plug-in.
#[derive(Clone, Debug)]
pub(crate) struct RodUi {
    pub page: Page,
    /// The Explore page's family, `None` for every block.
    pub family: Option<Family>,
    pub query: String,
    /// Typing goes to the search field.
    pub search_focus: bool,
    pub settings: SettingsSection,
    /// The slot each footswitch toggles; `None` follows the chain.
    pub footswitches: [Option<usize>; 4],
    pub show_names: bool,
    /// Meters read RMS rather than peak.
    pub meter_rms: bool,
    pub last_tap: Option<Instant>,
}

impl Default for RodUi {
    fn default() -> Self {
        Self {
            page: Page::Home,
            family: None,
            query: String::new(),
            search_focus: false,
            settings: SettingsSection::Audio,
            footswitches: [None; 4],
            show_names: true,
            meter_rms: false,
            last_tap: None,
        }
    }
}

// ── Families: the design's grouping of the plug-in's stages ────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Dynamics,
    Drive,
    Amp,
    Cab,
    Modulation,
    Delay,
    Reverb,
    Eq,
    Filter,
}

impl Family {
    const ALL: [Family; 9] = [
        Family::Dynamics,
        Family::Drive,
        Family::Amp,
        Family::Cab,
        Family::Modulation,
        Family::Delay,
        Family::Reverb,
        Family::Eq,
        Family::Filter,
    ];

    fn of(category: Category) -> Family {
        match category {
            Category::Gate | Category::Comp => Family::Dynamics,
            Category::Wah => Family::Filter,
            Category::Drive => Family::Drive,
            Category::Amp => Family::Amp,
            Category::Cab => Family::Cab,
            Category::Eq => Family::Eq,
            Category::Mod => Family::Modulation,
            Category::Delay => Family::Delay,
            Category::Reverb => Family::Reverb,
        }
    }

    fn categories(self) -> &'static [Category] {
        match self {
            Family::Dynamics => &[Category::Gate, Category::Comp],
            Family::Drive => &[Category::Drive],
            Family::Amp => &[Category::Amp],
            Family::Cab => &[Category::Cab],
            Family::Modulation => &[Category::Mod],
            Family::Delay => &[Category::Delay],
            Family::Reverb => &[Category::Reverb],
            Family::Eq => &[Category::Eq],
            Family::Filter => &[Category::Wah],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Family::Dynamics => "Dynamics",
            Family::Drive => "Drive",
            Family::Amp => "Amp",
            Family::Cab => "Cab",
            Family::Modulation => "Modulation",
            Family::Delay => "Delay",
            Family::Reverb => "Reverb",
            Family::Eq => "EQ",
            Family::Filter => "Filter",
        }
    }

    /// The short mark on a chain block.
    fn tag(self) -> &'static str {
        match self {
            Family::Dynamics => "DYN",
            Family::Drive => "DRIVE",
            Family::Amp => "AMP",
            Family::Cab => "CAB",
            Family::Modulation => "MOD",
            Family::Delay => "DELAY",
            Family::Reverb => "REVERB",
            Family::Eq => "EQ",
            Family::Filter => "FILTER",
        }
    }

    fn color(self) -> u32 {
        match self {
            Family::Dynamics => 0x9be15d,
            Family::Drive => 0xf2a23a,
            Family::Amp => 0xf25c54,
            Family::Cab => 0xc4c4c4,
            Family::Modulation => 0xb9a6ff,
            Family::Delay => 0x5db7f5,
            Family::Reverb => 0x4fd1c5,
            Family::Eq => 0xf6d55c,
            Family::Filter => 0xff8a3d,
        }
    }

    /// The design's icon, in a 24-unit box.
    fn icon(self) -> &'static str {
        match self {
            Family::Dynamics => "M5 17 A7 7 0 1 1 19 17 M12 16 L15.5 10",
            Family::Drive => "M13 3 L6 13 H11 L10 21 L18 10 H13 Z",
            Family::Amp => "M3 6 H21 V18 H3 Z M3 13 H21 M7 9.5 V9.6 M11 9.5 V9.6 M15 9.5 V9.6",
            Family::Cab => "M4 4 H20 V20 H4 Z M12 8 A4 4 0 1 1 11.99 8 M12 11.5 V12.5",
            Family::Modulation => "M2 12 C5 5 8 5 12 12 C16 19 19 19 22 12",
            Family::Delay => "M3 12 H5 M8 8 V16 M12 6 V18 M16 9 V15 M20 11 V13",
            Family::Reverb => {
                "M4 18 A8 8 0 0 1 20 18 M7.5 18 A4.5 4.5 0 0 1 16.5 18 M11 18 A1 1 0 0 1 13 18"
            }
            Family::Eq => "M6 4 V20 M12 4 V20 M18 4 V20 M4 9 H8 M10 15 H14 M16 7 H20",
            Family::Filter => "M3 16 C8 16 9 6 12 6 C15 6 16 16 21 16",
        }
    }
}

const PLUS_ICON: &str = "M12 7 V17 M7 12 H17";
const TUNER_ICON: &str = "M2 10 A5 5 0 0 1 10 10 M6 10 L8 4";

/// A model's line on its library card.
fn model_desc(label: &str) -> &'static str {
    match label {
        "Noise Gate" => "Keeps high-gain rigs silent between notes.",
        "Studio Compressor" => "Even sustain with threshold, ratio and makeup.",
        "Green Screamer" => "Mid-boosted overdrive that tightens the low end.",
        "Minotaur Boost" => "Clean, near-transparent push into the amp.",
        "Rats Nest" => "Hard-clipping, filthy distortion.",
        "Breaker Blues" => "Soft, low-gain overdrive.",
        "Face Fuzz" => "Gated, asymmetric germanium fuzz.",
        "Centurion" => "Transparent, mid-forward overdrive.",
        "DS Classic" => "Raw orange-box hard clipper.",
        "Super Drive" => "Smooth asymmetric-diode overdrive.",
        "Metal Core" => "Huge-gain, scooped metal distortion.",
        "Tight Rift" => "Modern multi-stage tight high gain.",
        "Amber Crunch" => "Bright silicon clip for blues and rock rhythm.",
        "Copper Fuzz" => "Tighter, more aggressive silicon fuzz.",
        "Mandarin 80" => "Warm, mid-forward British tube head.",
        "Brit Plexi 100" => "Bright, open plexi Super Lead.",
        "Twin Clean" => "High-headroom American clean.",
        "Top Boost" => "Chiming British combo.",
        "Recto Modern" => "Tight, modern high gain.",
        "JCM Crunch" => "Classic British crunch.",
        "Lead Slate" => "Saturated, hot-rodded lead.",
        "Bassman" => "Loose, bass-heavy American tweed.",
        "Overdrive Special" => "Smooth, singing boutique sustain.",
        "Invader 5150" => "Tight, scooped modern full stack.",
        "Tweed Deluxe" => "Small combo with early breakup.",
        "NAM Capture" => "Runs a Neural Amp Modeler capture you load.",
        "1960v Vintage 4x12" => "Vintage British closed 4x12.",
        "American 2x12" => "Brighter, tighter open-back combo.",
        "Tweed 1x12" => "Small, boxy, rolled-off single speaker.",
        "Modern 4x12" => "Tight, scooped, extended highs.",
        "Open Back" => "Wide open-back with front/rear cancellation.",
        "Vintage 2x12" => "Warm, moderately damped 2x12.",
        "Oversized 4x12" => "Deep, tightly damped oversized 4x12.",
        "Bass Cabinet" => "Extended-low bass cabinet.",
        "British Stack 4x12" => "Mid-forward greenback-voiced 4x12.",
        "Uberkab 4x12" => "Tight, scooped German high-gain 4x12.",
        "SLO Custom 4x12" => "Smooth, singing high-gain 4x12.",
        "Impulse Response" => "Convolves with a cabinet IR you load.",
        "Modern 2x12" => "Tight closed-back pair, extended top.",
        "American 1x12 Combo" => "Small, bright, open single speaker.",
        "70s Analog Chorus" => "Lush doubling with gentle movement.",
        "Vibe Phase 90" => "Four-stage swirl, from subtle to jet.",
        "Jet Flanger" => "Metallic sweep with resonant peaks.",
        "Opto Tremolo" => "Amp-style volume pulse.",
        "Molam Swirl" => "Staggered throb for Isan and luk-thung leads.",
        "Phin Vibe" => "Feedback-free Univibe-style throb.",
        "Khaen Swirl" => "Eight stages, lush and continuous.",
        "Bi-Lam" => "Twelve-stage cascade, slow and very wide.",
        "Isan Jet" => "Six stages, hard regeneration, fast and narrow.",
        "Soft Phase" => "One gentle notch: the subtle end.",
        "Wide Vibe" => "Linked stereo spread for a wide swirl.",
        "Tape Echo" => "Warm repeats with wow and flutter.",
        "Digital Delay" => "Clean, precise repeats.",
        "Analog Delay" => "Dark repeats that sit behind the guitar.",
        "Ping-Pong" => "Repeats alternating across the stereo image.",
        "Dual Delay" => "Quarter note left against a dotted eighth right.",
        "Studio Plate" => "Bright, dense studio plate.",
        "Tracking Room" => "Short, damped room that adds depth.",
        "Concert Hall" => "Big, slow-blooming space.",
        "Shimmer" => "Hall with an octave-up voice in the tail.",
        "Studio EQ" => "Shelves and two sweepable bells.",
        "Vintage EQ" => "Passive-console shelves and wide bells.",
        "Modern EQ" => "Extended shelves and narrow, precise bells.",
        "Cry Wah" => "Classic sweep: park it or rock it.",
        "Touch Wah" => "Envelope filter that follows your pick.",
        _ => "",
    }
}

/// One model on the Explore page: its category and its index in the
/// category's model list.
#[derive(Clone, Copy, Debug)]
struct Entry {
    category: Category,
    index: usize,
    label: &'static str,
}

fn entries(p: &Params) -> Vec<Entry> {
    Family::ALL
        .iter()
        .flat_map(|family| family.categories().iter().copied())
        .flat_map(|category| {
            let (list, _) = blocks::models(p, category.kinds()[0]);
            list.into_iter()
                .enumerate()
                .map(move |(index, choice)| Entry {
                    category,
                    index,
                    label: choice.label,
                })
        })
        .collect()
}

// ── Chain helpers ──────────────────────────────────────────────────────────

/// What the editor actually shows: the requested focus while it still
/// matches the chain, else the first block (or the first slot of an empty
/// chain) — so an undo or preset that removes the focused block never
/// leaves the editor pointing at nothing.
pub(crate) fn resolve_focus(
    order: &[Option<StageKind>],
    requested: Option<ChainFocus>,
) -> ChainFocus {
    match requested {
        Some(ChainFocus::Block(kind)) if order.contains(&Some(kind)) => ChainFocus::Block(kind),
        Some(ChainFocus::Empty(slot)) if order.get(slot) == Some(&None) => ChainFocus::Empty(slot),
        _ => order
            .iter()
            .flatten()
            .next()
            .map(|kind| ChainFocus::Block(*kind))
            .unwrap_or(ChainFocus::Empty(0)),
    }
}

fn focus_slot(order: &[Option<StageKind>], focus: ChainFocus) -> usize {
    match focus {
        ChainFocus::Block(kind) => order.iter().position(|s| *s == Some(kind)).unwrap_or(0),
        ChainFocus::Empty(slot) => slot,
    }
}

/// A chain of `shown` positions across the path: the design's 99 px blocks
/// 116 px apart up to seven, narrower blocks 12 px apart beyond, so the
/// whole chain always fits. Returns the block size and the pitch.
fn chain_geometry(shown: usize) -> (f32, f32) {
    if shown <= CHAIN_VISIBLE {
        (BLOCK, PITCH)
    } else {
        let gap = 12.0;
        let block = (CHAIN_W - gap * (shown - 1) as f32) / shown as f32;
        (block, block + gap)
    }
}

/// How many chain positions to draw: the path up to its last block, one
/// free position after it, never fewer than the design's seven.
fn shown_slots(order: &[Option<StageKind>]) -> usize {
    let last = order.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
    (last + 1).clamp(CHAIN_VISIBLE, rodharerist::PATH_SLOTS)
}

/// Puts model `index` of `category` into chain `slot` and switches it on:
/// the block already there if it is of the category, else a free instance,
/// else the category's first instance moved here from where it was.
/// Returns the stage placed.
pub(crate) fn place(p: &mut Params, slot: usize, category: Category, index: usize) -> StageKind {
    let kind = match p.stage_order[slot] {
        Some(kind) if Category::of(kind) == category => kind,
        _ => match category
            .kinds()
            .iter()
            .copied()
            .find(|kind| !p.stage_order.contains(&Some(*kind)))
        {
            Some(free) => free,
            None => {
                let first = category.kinds()[0];
                if let Some(old) = p.stage_order.iter().position(|s| *s == Some(first)) {
                    p.stage_order[old] = None;
                }
                first
            }
        },
    };
    p.stage_order[slot] = Some(kind);
    let (list, _) = blocks::models(p, kind);
    if let Some(choice) = list.get(index) {
        if !choice.wire_id.is_empty() {
            rodharerist::apply_to_params(p, choice.wire_id, choice.value);
        }
    }
    rodharerist::apply_to_params(p, blocks::enable_id(kind), 1.0);
    kind
}

/// The slot each footswitch toggles: its assignment, or the chain's blocks
/// in order with the amp and cabinet left to the player's rig.
fn footswitch_slots(ui: &RodUi, p: &Params) -> [Option<usize>; 4] {
    let mut auto = p.stage_order.iter().enumerate().filter_map(|(slot, s)| {
        s.filter(|kind| !matches!(Category::of(*kind), Category::Amp | Category::Cab))
            .map(|_| slot)
    });
    let mut out = [None; 4];
    for (i, assigned) in ui.footswitches.iter().enumerate() {
        out[i] = assigned.or_else(|| auto.next());
    }
    out
}

fn note_name(hz: f32) -> Option<(&'static str, f32)> {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    if hz <= 0.0 {
        return None;
    }
    let n = 12.0 * (hz / 440.0).log2() + 69.0;
    let r = n.round();
    Some((NAMES[(r as i32).rem_euclid(12) as usize], (n - r) * 100.0))
}

// ── Small pieces ───────────────────────────────────────────────────────────

/// A click handler that runs `f` on the window.
fn act(
    cx: &Cx,
    f: impl Fn(&mut RodhareistEditorWindow, &mut Cx) + 'static,
) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
    let entity = cx.entity().clone();
    move |_event, _window, app: &mut App| {
        let _ = entity.update(app, |this, cx| f(this, cx));
    }
}

/// A list row or choice: lit with the accent's border, or at rest.
fn choice_colors(on: bool, list: bool) -> (Rgba, Rgba, Rgba) {
    match (on, list) {
        (true, _) => (color(0x1f1f1f), accent(), color(WHITE)),
        (false, true) => (color_a(0, 0.0), color_a(0, 0.0), color(INK)),
        (false, false) => (color(0x2a2a2a), color(0x454545), color(PALE)),
    }
}

fn section(s: f32, left: f32, width: f32) -> gpui::Div {
    div()
        .absolute()
        .left(px(left * s))
        .top(px(341.0 * s))
        .w(px(width * s))
        .h(px(420.0 * s))
        .flex()
        .flex_col()
        .bg(color(plate::DECK))
}

fn heading(text: &str, s: f32) -> gpui::Div {
    words(plate::tracked(text, 0.1), 12.5, WHITE, s).font_weight(FontWeight::SEMIBOLD)
}

fn tile(s: f32) -> gpui::Div {
    div()
        .bg(color(0x2a2a2a))
        .border(px(1.0))
        .border_color(color(0x454545))
        .rounded(px(4.0 * s))
}

/// A card's coloured top edge, `height` thick under its `radius` corners.
fn top_strip(ink: u32, height: f32, radius: f32, s: f32) -> gpui::Div {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top_0()
        .h(px(height * s))
        .rounded_t(px(radius * s))
        .bg(color(ink))
}

/// A knob for a block parameter, in its family's colour.
fn block_knob(
    cx: &mut Cx,
    spec: ParamSpec,
    value: f32,
    ink: Rgba,
    size: f32,
    s: f32,
) -> AnyElement {
    let unit = match spec.unit {
        "dB" => DialUnit::Db,
        "%" => DialUnit::Percent,
        "ms" => DialUnit::Ms,
        "s" => DialUnit::Seconds,
        "Hz" => DialUnit::Hz,
        ":1" => DialUnit::Ratio,
        _ => DialUnit::Plain,
    };
    let default =
        crate::components::rodhareist_window::wire_value(&rodharerist::default_params(), spec.id);
    let mut dial = Dial::new(spec.id, spec.label, spec.min, spec.max, default, unit);
    if spec.min > 0.0 && matches!(unit, DialUnit::Hz | DialUnit::Ms | DialUnit::Seconds) {
        dial = dial.log();
    }
    if spec.min < 0.0 && spec.max > 0.0 {
        dial = dial.bipolar();
    }
    let look = KnobLook::small(size).styled(3.6, 18.0, 0x1a1a1a, 0x555555, 1.0, 6.0, 2.0);
    let cell = KnobCell::new(size + 20.0, 11.0, 12.5)
        .gap(3.0)
        .label(0.08, PALE)
        .label_below();
    let id = spec.id;
    let element = knob(
        cx,
        format!("rodhareist-knob-{id}"),
        dial,
        value,
        look,
        cell,
        ink,
        true,
        s,
        move |this: &mut RodhareistEditorWindow, v, cx| this.set_wire_value(id, v, cx),
    );
    if spec.inactive {
        div().opacity(0.4).child(element).into_any_element()
    } else {
        element
    }
}

// ── The well: header and chain ─────────────────────────────────────────────

fn header(w: &RodhareistEditorWindow, p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
    let page = w.ui.page;
    let nav = div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(4.0 * s))
        .flex()
        .flex_row()
        .justify_center()
        .gap(px(2.0 * s))
        .children(Page::ALL.iter().map(|&(item, label)| {
            let current = item == page;
            div()
                .id(("rodhareist-nav", item as usize))
                .flex()
                .items_center()
                .h(px(32.0 * s))
                .px(px(9.0 * s))
                .border_b(px(2.0 * s))
                .border_color(if current { accent() } else { color_a(0, 0.0) })
                .text_size(type_px(15.0, s))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color(if current { WHITE } else { SOFT }))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.text_color(color(WHITE)))
                .child(label)
                .on_click(act(cx, move |this, cx| {
                    this.ui.page = item;
                    this.ui.search_focus = false;
                    cx.notify();
                }))
        }));
    let tempo = w.host_tempo().map_or_else(
        || "\u{2014} BPM".to_string(),
        |bpm| format!("{} BPM", bpm.round()),
    );
    let tuner_on = p.tuner_on;
    let (bg, border, fg) = if tuner_on {
        (accent(), accent(), color(plate::WELL))
    } else {
        (color(plate::GROUND), color(plate::WELL_EDGE), color(INK))
    };
    let tuner = div()
        .id("rodhareist-tuner-toggle")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .h(px(26.0 * s))
        .px(px(9.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(type_px(12.0, s))
        .cursor(gpui::CursorStyle::PointingHand)
        .child(icon(TUNER_ICON, (12.0, 12.0), (12.0, 12.0), 1.4, fg, s))
        .child("Tuner")
        .on_click(act(cx, |this, cx| this.toggle_wire("tuner_on", cx)));
    div()
        .absolute()
        .left_0()
        .top_0()
        .right_0()
        .h(px(40.0 * s))
        .flex()
        .flex_row()
        .items_center()
        .pl(px(13.0 * s))
        .pr(px(12.0 * s))
        .child(nav)
        .child(
            words(plate::spaced("RODHAREIST", 0.0), 16.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD),
        )
        .child(div().flex_1())
        .child(numbers(tempo, 12.0, SOFT, s).mr(px(10.0 * s)))
        .child(tuner)
        .into_any_element()
}

/// A meter bar's lit height: −60 dB to 0 dB over 145 design pixels.
fn meter_h(level: f32) -> f32 {
    let db = 20.0 * level.max(1.0e-6).log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0) * 145.0
}

fn meters(w: &RodhareistEditorWindow, s: f32) -> Vec<AnyElement> {
    use rodharerist::telemetry as tm;
    let io = w.io_meter();
    let rms = w.ui.meter_rms;
    let read = |peak_slot: usize, rms_slot: usize, fallback: f32| {
        w.pads()
            .map(|block| block[if rms { rms_slot } else { peak_slot }])
            .unwrap_or(fallback)
    };
    let in_both = if rms { io.in_rms } else { io.in_peak };
    let out_both = if rms { io.out_rms } else { io.out_peak };
    let bars = [
        (21.0, read(tm::IN_L, tm::IN_RMS_L, in_both), false),
        (34.0, read(tm::IN_R, tm::IN_RMS_R, in_both), false),
        (954.0, read(tm::OUT_L, tm::OUT_RMS_L, out_both), true),
        (967.0, read(tm::OUT_R, tm::OUT_RMS_R, out_both), true),
    ];
    bars.into_iter()
        .map(|(left, level, output)| {
            let h = meter_h(level);
            let hot = output && 20.0 * level.max(1.0e-6).log10() > -3.0;
            div()
                .absolute()
                .left(px(left * s))
                .top(px(87.0 * s))
                .w(px(7.0 * s))
                .h(px(145.0 * s))
                .bg(color(0x262626))
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(h * s))
                        .bg(if hot { accent() } else { color(INK) }),
                )
                .into_any_element()
        })
        .collect()
}

fn chain_block(
    w: &RodhareistEditorWindow,
    p: &Params,
    slot: usize,
    selected: bool,
    (size, pitch): (f32, f32),
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    // Narrower blocks keep their type legible and shrink the rest.
    let k = size / BLOCK;
    let type_k = k.max(0.82);
    let entry = p.stage_order[slot];
    let names = w.ui.show_names;
    let (bg, border, fg, tag, led, icon_path, name, dashed) = match entry {
        Some(kind) => {
            let family = Family::of(Category::of(kind));
            let on = blocks::is_on(p, kind);
            (
                color(if on { 0xd9d9d9 } else { 0x3a3a3a }),
                color(if on { 0xd9d9d9 } else { 0x4a4a4a }),
                color(if on { 0x111111 } else { SOFT }),
                family.tag(),
                color(if on { family.color() } else { 0x555555 }),
                family.icon(),
                blocks::model_name(p, kind).unwrap_or("No amp"),
                false,
            )
        }
        None => (
            color(0x161616),
            color(0x5a5a5a),
            color(FAINT),
            "EMPTY",
            color_a(0, 0.0),
            PLUS_ICON,
            "Add block",
            true,
        ),
    };
    let mut block = div()
        .id(("rodhareist-block", slot))
        .flex()
        .flex_col()
        .justify_between()
        .size(px(size * s))
        .p(px(8.0 * k * s))
        .rounded(px(2.0 * s))
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .cursor(gpui::CursorStyle::PointingHand)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    words(plate::tracked(tag, 0.1), 9.5 * type_k, WHITE, s)
                        .text_color(fg)
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(div().size(px(8.0 * k * s)).rounded_full().bg(led)),
        )
        .child(div().flex().justify_center().child(icon(
            icon_path,
            (24.0, 24.0),
            (26.0 * k, 26.0 * k),
            1.6,
            fg,
            s,
        )))
        .child(
            div()
                .h(px(14.0 * s))
                .min_w(px(0.0))
                .truncate()
                .text_size(type_px(11.5 * type_k, s))
                .line_height(px(13.2 * type_k * s))
                .font_weight(FontWeight::SEMIBOLD)
                .child(if names { name } else { "" }),
        );
    if dashed {
        block = block.border_dashed();
    }
    let block = block
        .tooltip(fb_tooltip(match entry {
            Some(kind) => format!(
                "Slot {} \u{00B7} {} \u{2014} double-click to bypass",
                slot + 1,
                blocks::block_name(kind)
            ),
            None => format!("Slot {} \u{00B7} empty", slot + 1),
        }))
        .on_click({
            let entity = cx.entity().clone();
            move |event, _window, app| {
                let double = event.click_count() >= 2;
                let _ = entity.update(app, |this, cx| {
                    this.chain_focus = Some(match entry {
                        Some(kind) => ChainFocus::Block(kind),
                        None => ChainFocus::Empty(slot),
                    });
                    if double {
                        if let Some(kind) = entry {
                            this.toggle_wire(blocks::enable_id(kind), cx);
                        }
                    }
                    cx.notify();
                });
            }
        });
    let left = CHAIN_LEFT + slot as f32 * pitch;
    let top = 113.0;
    if selected {
        // The design's ring: a dark gap, then the accent.
        div()
            .absolute()
            .left(px((left - 5.0) * s))
            .top(px((top - 5.0) * s))
            .size(px((size + 10.0) * s))
            .p(px(3.0 * s))
            .rounded(px(5.0 * s))
            .border(px(2.0 * s))
            .border_color(accent())
            .bg(color(plate::WELL))
            .child(block)
            .into_any_element()
    } else {
        div()
            .absolute()
            .left(px(left * s))
            .top(px(top * s))
            .child(block)
            .into_any_element()
    }
}

fn caption_line(p: &Params, focus: ChainFocus) -> String {
    let slot = focus_slot(&p.stage_order, focus);
    match focus {
        ChainFocus::Block(kind) => {
            let family = Family::of(Category::of(kind));
            format!(
                "Slot {} \u{00B7} {} \u{00B7} {}{}",
                slot + 1,
                family.label(),
                blocks::model_name(p, kind).unwrap_or("No amp"),
                if blocks::is_on(p, kind) {
                    ""
                } else {
                    " (bypassed)"
                }
            )
        }
        ChainFocus::Empty(_) => format!("Slot {} \u{00B7} empty", slot + 1),
    }
}

fn display(
    w: &RodhareistEditorWindow,
    p: &Params,
    focus: ChainFocus,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let shown = shown_slots(&p.stage_order);
    let selected_slot = focus_slot(&p.stage_order, focus);
    let geometry = chain_geometry(shown);
    let wire = div()
        .absolute()
        .left(px(CHAIN_LEFT * s))
        .top(px(162.0 * s))
        .w(px(CHAIN_W * s))
        .h(px(3.0 * s))
        .bg(color(0xb5b5b5));
    let chain: Vec<AnyElement> = (0..shown)
        .map(|slot| chain_block(w, p, slot, slot == selected_slot, geometry, cx, s))
        .collect();
    let label = |text: &str, left: f32, width: f32| {
        div()
            .absolute()
            .left(px(left * s))
            .top(px(238.0 * s))
            .w(px(width * s))
            .flex()
            .justify_center()
            .child(words(plate::spaced(text, 0.08), 10.0, FAINT, s))
    };
    div()
        .absolute()
        .left(px(6.0 * s))
        .top(px(7.0 * s))
        .w(px(1011.0 * s))
        .h(px(327.0 * s))
        .bg(color(plate::WELL))
        .border(px(1.0))
        .border_color(color(plate::WELL_EDGE))
        .child(header(w, p, cx, s))
        .children(meters(w, s))
        .child(label("IN", 21.0, 20.0))
        .child(label("OUT", 950.0, 28.0))
        .child(wire)
        .children(chain)
        .child(
            div()
                .absolute()
                .left(px(CHAIN_LEFT * s))
                .top(px(232.0 * s))
                .w(px(CHAIN_W * s))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(4.0 * s))
                .child(words(caption_line(p, focus), 13.0, WHITE, s))
                .child(words(
                    "Click a block to select it \u{00B7} double-click to bypass",
                    11.5,
                    FAINT,
                    s,
                )),
        )
        .into_any_element()
}

// ── Home ───────────────────────────────────────────────────────────────────

fn home(w: &RodhareistEditorWindow, p: &Params, cx: &mut Cx, s: f32) -> Vec<AnyElement> {
    let current = w.preset_status();
    let presets = rodharerist::factory_presets();
    let rows: Vec<AnyElement> = presets
        .iter()
        .enumerate()
        .map(|(index, preset)| {
            let on = current.map(|(i, _)| i) == Some(index);
            let (bg, border, _) = choice_colors(on, true);
            div()
                .id(("rodhareist-preset", index))
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap(px(10.0 * s))
                .h(px(40.0 * s))
                .px(px(10.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(border)
                .bg(bg)
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(color(0x2e2e2e)))
                .child(numbers(preset.id, 12.0, if on { ACCENT } else { SOFT }, s).w(px(30.0 * s)))
                .child(words(preset.name, 13.0, WHITE, s))
                .on_click(w.load_preset_cb(cx, index))
                .into_any_element()
        })
        .collect();
    let list = section(s, 6.0, 262.0)
        .p(px(14.0 * s))
        .gap(px(6.0 * s))
        .child(heading("Presets", s).mb(px(4.0 * s)))
        .child(
            div()
                .id("rodhareist-presets")
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .overflow_y_scroll()
                .children(rows),
        );

    let (code, name, edited) =
        match current.and_then(|(i, edited)| presets.get(i).map(|pr| (pr, edited))) {
            Some((preset, edited)) => (preset.id, preset.name, edited),
            None => ("", "Custom", false),
        };
    let occupied: Vec<usize> = p
        .stage_order
        .iter()
        .enumerate()
        .filter_map(|(slot, s)| s.map(|_| slot))
        .collect();
    let active = occupied
        .iter()
        .filter(|&&slot| p.stage_order[slot].is_some_and(|kind| blocks::is_on(p, kind)))
        .count();

    let footswitches: Vec<AnyElement> = footswitch_slots(&w.ui, p)
        .into_iter()
        .enumerate()
        .map(|(i, slot)| footswitch(p, i, slot, cx, s))
        .collect();

    let top = div()
        .flex()
        .flex_row()
        .items_end()
        .justify_between()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0 * s))
                .child(numbers(
                    match code.get(0..2) {
                        Some(bank) => format!(
                            "{code} \u{00B7} BANK {bank}{}",
                            if edited { " \u{00B7} EDITED" } else { "" }
                        ),
                        None => "NO PRESET LOADED".to_string(),
                    },
                    12.0,
                    ACCENT,
                    s,
                ))
                .child(
                    words(name, 38.0, WHITE, s)
                        .font_weight(FontWeight::SEMIBOLD)
                        .line_height(px(40.0 * s)),
                ),
        )
        .child(words(
            format!("{active} of {} blocks active", occupied.len()),
            12.0,
            SOFT,
            s,
        ));

    let performance = section(s, 274.0, 743.0)
        .px(px(18.0 * s))
        .py(px(16.0 * s))
        .justify_between()
        .child(top)
        .child(plate::columns(10.0, s, footswitches))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(10.0 * s))
                .h(px(96.0 * s))
                .child(tuner_box(w, p, s))
                .child(tempo_box(p, cx, s))
                .child(master_box(p, cx, s)),
        );
    vec![list.into_any_element(), performance.into_any_element()]
}

fn footswitch(p: &Params, i: usize, slot: Option<usize>, cx: &mut Cx, s: f32) -> AnyElement {
    let kind = slot.and_then(|slot| p.stage_order.get(slot).copied().flatten());
    let on = kind.is_some_and(|kind| blocks::is_on(p, kind));
    let family = kind.map(|kind| Family::of(Category::of(kind)));
    let led = match (on, family) {
        (true, Some(family)) => color(family.color()),
        _ => color(0x3a3a3a),
    };
    let name = kind.map_or("Empty", |kind| {
        blocks::model_name(p, kind).unwrap_or("No amp")
    });
    let sub = match (slot, family) {
        (Some(slot), Some(family)) => format!("SLOT {} \u{00B7} {}", slot + 1, family.tag()),
        (Some(slot), None) => format!("SLOT {}", slot + 1),
        (None, _) => "UNASSIGNED".to_string(),
    };
    let mut button = div()
        .id(("rodhareist-fs", i))
        .flex()
        .flex_col()
        .items_center()
        .justify_between()
        .h(px(150.0 * s))
        .p(px(12.0 * s))
        .rounded(px(6.0 * s))
        .border(px(1.0))
        .border_color(color(0x454545))
        .bg(color(0x2a2a2a))
        .child(
            div()
                .flex()
                .flex_row()
                .w_full()
                .items_center()
                .justify_between()
                .child(numbers(format!("FS{}", i + 1), 11.0, SOFT, s))
                .child({
                    let mut dot = div().size(px(10.0 * s)).rounded_full().bg(led);
                    if on {
                        dot = dot.shadow(vec![gpui::BoxShadow {
                            color: led.into(),
                            offset: gpui::point(px(0.0), px(0.0)),
                            blur_radius: px(10.0 * s),
                            spread_radius: px(0.0),
                            inset: false,
                        }]);
                    }
                    dot
                }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(2.0 * s))
                .max_w_full()
                .child(
                    words(name, 14.0, if on { WHITE } else { SOFT }, s)
                        .font_weight(FontWeight::SEMIBOLD)
                        .max_w_full()
                        .truncate(),
                )
                .child(words(plate::spaced(&sub, 0.08), 10.5, SOFT, s)),
        )
        .child(
            div()
                .size(px(52.0 * s))
                .rounded_full()
                .bg(color(0x1a1a1a))
                .border(px(3.0 * s))
                .border_color(color(0x5a5a5a)),
        );
    if let Some(kind) = kind {
        button = button
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x303030)))
            .on_click(act(cx, move |this, cx| {
                this.toggle_wire(blocks::enable_id(kind), cx)
            }));
    }
    button.into_any_element()
}

fn tuner_box(w: &RodhareistEditorWindow, p: &Params, s: f32) -> AnyElement {
    let on = p.tuner_on;
    let hz = w
        .pads()
        .map(|block| block[rodharerist::telemetry::TUNER_HZ])
        .filter(|_| on)
        .unwrap_or(0.0);
    let reading = note_name(hz);
    let (note, text, ink, position) = match (on, reading) {
        (false, _) => ("\u{2014}".to_string(), "Off".to_string(), FAINT, 50.0),
        (true, None) => ("\u{2014}".to_string(), "No pitch".to_string(), WHITE, 50.0),
        (true, Some((name, cents))) => {
            let in_tune = cents.abs() < 3.0;
            let text = if in_tune {
                "In tune".to_string()
            } else {
                format!(
                    "{}{:.0} cents",
                    if cents > 0.0 { "+" } else { "\u{2212}" },
                    cents.abs()
                )
            };
            (
                name.to_string(),
                text,
                if in_tune { 0x9be15d } else { WHITE },
                (50.0 + cents).clamp(2.0, 98.0),
            )
        }
    };
    tile(s)
        .flex_1()
        .flex()
        .flex_col()
        .justify_between()
        .px(px(14.0 * s))
        .py(px(10.0 * s))
        .opacity(if on { 1.0 } else { 0.45 })
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .items_center()
                .child(caption("Tuner", 10.5, 0.1, SOFT, s))
                .child(numbers(text, 11.5, ink, s)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(14.0 * s))
                .child(
                    words(note, 30.0, ink, s)
                        .w(px(48.0 * s))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .h(px(16.0 * s))
                        .rounded(px(2.0 * s))
                        .bg(color(0x1a1a1a))
                        .child(
                            div()
                                .absolute()
                                .left(gpui::relative(0.5))
                                .ml(px(-1.0 * s))
                                .top(px(-3.0 * s))
                                .bottom(px(-3.0 * s))
                                .w(px(2.0 * s))
                                .bg(color(0x6a6a6a)),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(gpui::relative(position / 100.0))
                                .ml(px(-3.0 * s))
                                .top(px(2.0 * s))
                                .bottom(px(2.0 * s))
                                .w(px(6.0 * s))
                                .rounded(px(2.0 * s))
                                .bg(color(ink)),
                        ),
                ),
        )
        .into_any_element()
}

fn tempo_box(p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
    let bpm = 60_000.0 / p.delay_time_ms.max(1.0);
    tile(s)
        .w(px(190.0 * s))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px(px(14.0 * s))
        .py(px(10.0 * s))
        .child(
            div()
                .flex()
                .flex_col()
                .child(caption("Tempo", 10.5, 0.1, SOFT, s))
                .child(numbers(format!("{}", bpm.round()), 28.0, WHITE, s)),
        )
        .child(
            div()
                .id("rodhareist-tap")
                .flex()
                .items_center()
                .justify_center()
                .size(px(64.0 * s))
                .rounded_full()
                .bg(color(0x1a1a1a))
                .border(px(3.0 * s))
                .border_color(accent())
                .text_size(type_px(12.0, s))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color(WHITE))
                .cursor(gpui::CursorStyle::PointingHand)
                .child(plate::tracked("Tap", 0.08))
                .tooltip(fb_tooltip("Tap a quarter note: sets Delay A's time"))
                .on_click(act(cx, |this, cx| this.tap_tempo(cx))),
        )
        .into_any_element()
}

fn master_box(p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
    let Some(spec) = blocks::trim_spec("output_trim", "MASTER") else {
        return div().into_any_element();
    };
    let dial = Dial::new(spec.id, "MASTER", spec.min, spec.max, 0.0, DialUnit::Db).bipolar();
    let look =
        KnobLook::small(48.0).styled(4.5, 16.0, plate::FACE, plate::FACE_EDGE, 1.5, 6.0, 2.6);
    let cell = KnobCell::new(98.0, 10.0, 11.0).gap(1.0).label(0.08, SOFT);
    tile(s)
        .w(px(110.0 * s))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .p(px(6.0 * s))
        .child(knob(
            cx,
            "rodhareist-master",
            dial,
            p.output_trim_db,
            look,
            cell,
            accent(),
            true,
            s,
            |this: &mut RodhareistEditorWindow, v, cx| this.set_wire_value("output_trim", v, cx),
        ))
        .into_any_element()
}

// ── Explore ────────────────────────────────────────────────────────────────

fn explore(
    w: &RodhareistEditorWindow,
    p: &Params,
    focus: ChainFocus,
    cx: &mut Cx,
    s: f32,
) -> Vec<AnyElement> {
    let all = entries(p);
    let current = w.ui.family;
    let mut rows: Vec<(Option<Family>, &'static str, u32, usize)> =
        vec![(None, "All blocks", INK, all.len())];
    rows.extend(Family::ALL.iter().map(|&family| {
        let count = all
            .iter()
            .filter(|e| Family::of(e.category) == family)
            .count();
        (Some(family), family.label(), family.color(), count)
    }));
    let families = section(s, 6.0, 262.0)
        .p(px(14.0 * s))
        .gap(px(3.0 * s))
        .child(heading("Categories", s).mb(px(6.0 * s)))
        .children(
            rows.into_iter()
                .enumerate()
                .map(|(index, (family, label, dot, count))| {
                    let on = family == current;
                    let (bg, border, _) = choice_colors(on, true);
                    div()
                        .id(("rodhareist-family", index))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.0 * s))
                        .h(px(30.0 * s))
                        .px(px(10.0 * s))
                        .rounded(px(3.0 * s))
                        .border(px(1.0))
                        .border_color(border)
                        .bg(bg)
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.bg(color(0x2e2e2e)))
                        .child(div().size(px(8.0 * s)).rounded_full().bg(color(dot)))
                        .child(words(label, 13.0, WHITE, s).flex_1())
                        .child(numbers(count.to_string(), 11.0, SOFT, s))
                        .on_click(act(cx, move |this, cx| {
                            this.ui.family = family;
                            cx.notify();
                        }))
                }),
        );

    let target = focus_slot(&p.stage_order, focus);
    let query = w.ui.query.trim().to_lowercase();
    let matches: Vec<Entry> = all
        .into_iter()
        .filter(|e| current.is_none_or(|family| Family::of(e.category) == family))
        .filter(|e| {
            query.is_empty()
                || e.label.to_lowercase().contains(&query)
                || model_desc(e.label).to_lowercase().contains(&query)
        })
        .collect();
    let cards: Vec<AnyElement> = matches
        .iter()
        .map(|&entry| library_card(p, entry, target, cx, s))
        .collect();
    let mut grid = div()
        .id("rodhareist-library")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(8.0 * s));
    let mut cards = cards.into_iter();
    loop {
        let row: Vec<AnyElement> = cards.by_ref().take(3).collect();
        if row.is_empty() {
            break;
        }
        let missing = 3 - row.len();
        let mut row = row;
        row.extend((0..missing).map(|_| div().into_any_element()));
        grid = grid.child(plate::columns(8.0, s, row).flex_shrink_0().h(px(104.0 * s)));
    }
    if matches.is_empty() {
        grid = grid.child(words("No block matches the search.", 13.0, SOFT, s));
    }

    let focused = w.ui.search_focus;
    let search = div()
        .id("rodhareist-search")
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .h(px(30.0 * s))
        .px(px(10.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(if focused { accent() } else { color(0x4a4a4a) })
        .bg(color(0x1f1f1f))
        .cursor(gpui::CursorStyle::IBeam)
        .child(if w.ui.query.is_empty() && !focused {
            words("Search blocks", 13.0, FAINT, s).into_any_element()
        } else {
            words(
                format!("{}{}", w.ui.query, if focused { "|" } else { "" }),
                13.0,
                WHITE,
                s,
            )
            .into_any_element()
        })
        .on_click(act(cx, |this, cx| {
            this.ui.search_focus = true;
            cx.notify();
        }));
    let library = section(s, 274.0, 743.0)
        .px(px(16.0 * s))
        .py(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0 * s))
                .child(heading("Explore", s))
                .child(search)
                .child(words(format!("Target: slot {}", target + 1), 12.0, PALE, s)),
        )
        .child(grid);
    vec![families.into_any_element(), library.into_any_element()]
}

fn library_card(p: &Params, entry: Entry, target: usize, cx: &mut Cx, s: f32) -> AnyElement {
    let family = Family::of(entry.category);
    let here = p.stage_order[target].filter(|kind| Category::of(*kind) == entry.category);
    let placed = here.is_some_and(|kind| blocks::models(p, kind).1 == Some(entry.index));
    let label = if placed {
        format!("In slot {}", target + 1)
    } else {
        format!("Place in slot {}", target + 1)
    };
    let (category, index) = (entry.category, entry.index);
    let mut button = div()
        .id((
            "rodhareist-place",
            entry.category as usize * 64 + entry.index,
        ))
        .flex()
        .items_center()
        .h(px(24.0 * s))
        .px(px(9.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(accent())
        .bg(color(0x1f1f1f))
        .text_size(type_px(11.5, s))
        .text_color(color(WHITE))
        .child(label);
    if !placed {
        button = button
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x2e2e2e)))
            .on_click(act(cx, move |this, cx| {
                let mut next = this.params_snapshot();
                let kind = place(&mut next, target, category, index);
                this.chain_focus = Some(ChainFocus::Block(kind));
                this.set_params(next, cx);
            }));
    }
    tile(s)
        .relative()
        .h_full()
        .overflow_hidden()
        .child(top_strip(family.color(), 3.0, 4.0, s))
        .flex()
        .flex_col()
        .justify_between()
        .px(px(12.0 * s))
        .py(px(10.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(icon(
                    family.icon(),
                    (24.0, 24.0),
                    (18.0, 18.0),
                    1.7,
                    color(family.color()),
                    s,
                ))
                .child(
                    words(entry.label, 14.0, WHITE, s)
                        .font_weight(FontWeight::SEMIBOLD)
                        .min_w(px(0.0))
                        .truncate(),
                ),
        )
        .child(
            div()
                .text_size(type_px(11.5, s))
                .line_height(px(15.0 * s))
                .text_color(color(PALE))
                .child(model_desc(entry.label)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    words(plate::tracked(family.label(), 0.1), 9.5, SOFT, s)
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(button),
        )
        .into_any_element()
}

// ── Pedalboard ─────────────────────────────────────────────────────────────

fn pedalboard(
    w: &RodhareistEditorWindow,
    p: &Params,
    focus: ChainFocus,
    cx: &mut Cx,
    s: f32,
) -> Vec<AnyElement> {
    let slot = focus_slot(&p.stage_order, focus);
    let kind = match focus {
        ChainFocus::Block(kind) => Some(kind),
        ChainFocus::Empty(_) => None,
    };
    let family = kind.map(|kind| Family::of(Category::of(kind)));
    let tag_color = family.map_or(0x5a5a5a, Family::color);
    let mut left = section(s, 6.0, 262.0)
        .p(px(14.0 * s))
        .gap(px(12.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(heading(&format!("Slot {}", slot + 1), s))
                .child(
                    div()
                        .px(px(6.0 * s))
                        .py(px(2.0 * s))
                        .rounded(px(2.0 * s))
                        .bg(color(tag_color))
                        .text_size(type_px(10.0, s))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(0x111111))
                        .child(plate::tracked(family.map_or("Empty", Family::tag), 0.1)),
                ),
        );
    let Some(kind) = kind else {
        left = left
            .child(words("This slot is empty.", 13.0, PALE, s))
            .child(
                div()
                    .id("rodhareist-browse")
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(34.0 * s))
                    .rounded(px(3.0 * s))
                    .border(px(1.0))
                    .border_color(accent())
                    .bg(color(0x1f1f1f))
                    .text_size(type_px(13.0, s))
                    .text_color(color(WHITE))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child("Browse blocks")
                    .on_click(act(cx, |this, cx| {
                        this.ui.page = Page::Explore;
                        cx.notify();
                    })),
            );
        return vec![
            left.into_any_element(),
            section(s, 274.0, 743.0).p(px(18.0 * s)).into_any_element(),
        ];
    };

    let (list, selected) = blocks::models(p, kind);
    let models = div()
        .id("rodhareist-models")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(4.0 * s))
        .children(list.into_iter().enumerate().map(|(index, choice)| {
            let on = selected == Some(index);
            let (bg, border, fg) = choice_colors(on, false);
            let mut row = div()
                .id(("rodhareist-model", index))
                .flex()
                .items_center()
                .flex_shrink_0()
                .h(px(32.0 * s))
                .px(px(10.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(border)
                .bg(bg)
                .text_color(fg)
                .text_size(type_px(13.0, s))
                .child(choice.label);
            if !on && !choice.wire_id.is_empty() {
                row = row
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| style.bg(color(0x303030)))
                    .on_click(w.set_cb(cx, choice.wire_id, choice.value));
            }
            row
        }));
    let last = rodharerist::PATH_SLOTS - 1;
    let action = |id: &'static str,
                  label: &'static str,
                  enabled: bool,
                  apply: Box<dyn Fn(&mut Params)>,
                  then: ChainFocus,
                  cx: &mut Cx| {
        let mut button = div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .flex_1()
            .h(px(32.0 * s))
            .rounded(px(3.0 * s))
            .border(px(1.0))
            .border_color(color(0x484848))
            .bg(color(0x262626))
            .text_size(type_px(12.0, s))
            .text_color(color(INK))
            .child(label);
        if enabled {
            button = button
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(color(0x303030)))
                .on_click(w.chain_edit_cb(cx, move |p| apply(p), then));
        } else {
            button = button.opacity(0.4);
        }
        button
    };
    let moves = div()
        .flex()
        .flex_row()
        .gap(px(4.0 * s))
        .child(action(
            "rodhareist-move-left",
            "\u{2190} Move",
            slot > 0,
            Box::new(move |p: &mut Params| p.stage_order.swap(slot, slot.saturating_sub(1))),
            ChainFocus::Block(kind),
            cx,
        ))
        .child(action(
            "rodhareist-move-right",
            "Move \u{2192}",
            slot < last,
            Box::new(move |p: &mut Params| p.stage_order.swap(slot, (slot + 1).min(last))),
            ChainFocus::Block(kind),
            cx,
        ))
        .child(action(
            "rodhareist-remove",
            "Remove",
            true,
            Box::new(move |p: &mut Params| p.stage_order[slot] = None),
            ChainFocus::Empty(slot),
            cx,
        ));
    left = left
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .gap(px(4.0 * s))
                .child(caption("Model", 10.5, 0.1, SOFT, s))
                .child(models),
        )
        .child(moves);

    let right = section(s, 274.0, 743.0)
        .p(px(18.0 * s))
        .child(block_card(w, p, kind, cx, s));
    vec![left.into_any_element(), right.into_any_element()]
}

fn block_card(
    w: &RodhareistEditorWindow,
    p: &Params,
    kind: StageKind,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let family = Family::of(Category::of(kind));
    let ink = color(family.color());
    let on = blocks::is_on(p, kind);
    let values: std::collections::HashMap<&'static str, f32> =
        rodharerist::ui_values(p).into_iter().collect();
    let specs = blocks::params(p, kind);
    let size = if specs.len() > 6 { 56.0 } else { 76.0 };
    let knobs: Vec<AnyElement> = specs
        .into_iter()
        .map(|spec| {
            let value = values.get(spec.id).copied().unwrap_or(spec.min);
            block_knob(cx, spec, value, ink, size, s)
        })
        .collect();

    // The cabinet's microphone and IR, and the amp's NAM capture.
    let mut extras: Vec<AnyElement> = Vec::new();
    if kind == StageKind::Cab {
        let current = blocks::mic_index(p);
        extras.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0 * s))
                .child(caption("Microphone", 10.5, 0.1, SOFT, s).mr(px(4.0 * s)))
                .children(
                    blocks::mic_choices()
                        .into_iter()
                        .enumerate()
                        .map(|(index, choice)| {
                            plate::pill(
                                ("rodhareist-mic", index),
                                choice.label,
                                current == Some(index),
                                accent(),
                                plate::PillLook::words(28.0, 10.0, 12.5),
                                true,
                                s,
                                w.set_cb(cx, choice.wire_id, choice.value),
                            )
                        }),
                )
                .into_any_element(),
        );
        if p.cab_model == CabModel::Ir {
            extras.push(load_row(
                "rodhareist-load-ir",
                "Load IR\u{2026}",
                w.ir_loaded_info().map(|(name, secs)| {
                    if secs > 0.0 {
                        format!("{name} ({secs:.2} s)")
                    } else {
                        name
                    }
                }),
                w.ir_loading(),
                w.ir_error(),
                w.load_ir_cb(cx),
                s,
            ));
        }
    }
    if kind == StageKind::Amp && p.tone_engine == ToneEngineKind::NamCapture {
        extras.push(load_row(
            "rodhareist-load-nam",
            "Load capture\u{2026}",
            w.nam_loaded_info(),
            w.nam_loading(),
            w.nam_error(),
            w.load_nam_cb(cx),
            s,
        ));
        extras.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0 * s))
                .child(caption("Loudness", 10.5, 0.1, SOFT, s).mr(px(4.0 * s)))
                .child(plate::pill(
                    "rodhareist-nam-raw",
                    "Raw",
                    !p.nam_loudness_norm,
                    accent(),
                    plate::PillLook::words(28.0, 10.0, 12.5),
                    true,
                    s,
                    w.set_cb(cx, "nam_loudness_norm", 0.0),
                ))
                .child(plate::pill(
                    "rodhareist-nam-norm",
                    "Normalized",
                    p.nam_loudness_norm,
                    accent(),
                    plate::PillLook::words(28.0, 10.0, 12.5),
                    true,
                    s,
                    w.set_cb(cx, "nam_loudness_norm", 1.0),
                ))
                .into_any_element(),
        );
    }

    let mut led =
        div()
            .size(px(12.0 * s))
            .rounded_full()
            .bg(if on { ink } else { color(0x3a3a3a) });
    if on {
        led = led.shadow(vec![gpui::BoxShadow {
            color: ink.into(),
            offset: gpui::point(px(0.0), px(0.0)),
            blur_radius: px(12.0 * s),
            spread_radius: px(0.0),
            inset: false,
        }]);
    }
    let name = blocks::model_name(p, kind).unwrap_or("No amp");
    tile(s)
        .relative()
        .size_full()
        .rounded(px(8.0 * s))
        .overflow_hidden()
        .child(top_strip(family.color(), 4.0, 8.0, s))
        .px(px(22.0 * s))
        .py(px(18.0 * s))
        .flex()
        .flex_col()
        .justify_between()
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0 * s))
                        .opacity(if on { 1.0 } else { 0.6 })
                        .child(words(name, 26.0, WHITE, s).font_weight(FontWeight::SEMIBOLD))
                        .child(words(
                            format!("{} \u{00B7} {}", blocks::block_name(kind), model_desc(name)),
                            13.0,
                            PALE,
                            s,
                        )),
                )
                .child(led),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .justify_center()
                .gap(px(14.0 * s))
                .opacity(if on { 1.0 } else { 0.6 })
                .children(knobs),
        )
        .children(
            extras
                .into_iter()
                .map(|row| div().flex().justify_center().child(row)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(14.0 * s))
                .child(
                    div()
                        .id("rodhareist-block-bypass")
                        .size(px(66.0 * s))
                        .rounded_full()
                        .bg(color(0x1a1a1a))
                        .border(px(3.0 * s))
                        .border_color(color(0x6a6a6a))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.border_color(color(0x8a8a8a)))
                        .on_click(w.toggle_cb(cx, blocks::enable_id(kind))),
                )
                .child(
                    words(
                        plate::tracked(if on { "On" } else { "Bypassed" }, 0.1),
                        12.0,
                        if on { WHITE } else { SOFT },
                        s,
                    )
                    .font_weight(FontWeight::SEMIBOLD),
                ),
        )
        .into_any_element()
}

/// A file slot (IR or NAM capture): its load button and what is loaded.
#[allow(clippy::too_many_arguments)]
fn load_row(
    id: &'static str,
    button: &'static str,
    loaded: Option<String>,
    loading: bool,
    error: Option<String>,
    on_load: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    s: f32,
) -> AnyElement {
    let (status, ink) = if loading {
        ("Loading\u{2026}".to_string(), SOFT)
    } else if let Some(error) = error {
        (error, ACCENT)
    } else if let Some(name) = loaded {
        (name, WHITE)
    } else {
        ("Nothing loaded".to_string(), SOFT)
    };
    let mut load = div()
        .id(id)
        .flex()
        .items_center()
        .h(px(28.0 * s))
        .px(px(12.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(accent())
        .bg(color(0x1f1f1f))
        .text_size(type_px(12.5, s))
        .text_color(color(WHITE))
        .child(button);
    if !loading {
        load = load
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x2e2e2e)))
            .on_click(on_load);
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0 * s))
        .child(load)
        .child(words(status, 12.5, ink, s).max_w(px(360.0 * s)).truncate())
        .into_any_element()
}

// ── Settings ───────────────────────────────────────────────────────────────

enum Control {
    Pills(
        Vec<(
            String,
            bool,
            Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>,
        )>,
    ),
    Toggle(bool, Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>),
    Stepper(
        String,
        Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>,
        Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>,
    ),
    Info(String),
}

struct Row {
    label: String,
    help: String,
    control: Control,
}

fn settings_rows(w: &RodhareistEditorWindow, p: &Params) -> Vec<Row> {
    match w.ui.settings {
        SettingsSection::Audio => {
            let status = w.host_status();
            let trim = p.input_trim_db;
            vec![
                Row {
                    label: "Sample rate".into(),
                    help: "Set by Futureboard's audio device".into(),
                    control: Control::Info(status.map_or("\u{2014}".into(), |(rate, _)| {
                        format!("{:.1} kHz", rate as f32 / 1_000.0)
                    })),
                },
                Row {
                    label: "Buffer size".into(),
                    help: "Set by Futureboard's audio device".into(),
                    control: Control::Info(
                        status.map_or("\u{2014}".into(), |(_, block)| format!("{block} samples")),
                    ),
                },
                Row {
                    label: "Input trim".into(),
                    help: "Level into the chain, before the first block".into(),
                    control: Control::Stepper(
                        plate::fmt_db(trim),
                        Box::new(move |this, cx| this.set_wire_value("input_trim", trim - 0.5, cx)),
                        Box::new(move |this, cx| this.set_wire_value("input_trim", trim + 0.5, cx)),
                    ),
                },
                Row {
                    label: "Plug-in power".into(),
                    help: "Off passes the guitar through untouched".into(),
                    control: Control::Toggle(
                        p.power,
                        Box::new(|this, cx| this.toggle_wire("power", cx)),
                    ),
                },
            ]
        }
        SettingsSection::Footswitches => {
            let shown = shown_slots(&p.stage_order);
            let current = footswitch_slots(&w.ui, p);
            (0..4)
                .map(|i| {
                    let slot = current[i];
                    let help = match slot {
                        Some(slot) => format!(
                            "Toggles slot {}{}",
                            slot + 1,
                            p.stage_order[slot].map_or(" \u{00B7} empty".to_string(), |kind| {
                                format!(
                                    " \u{00B7} {}",
                                    blocks::model_name(p, kind).unwrap_or("No amp")
                                )
                            })
                        ),
                        None => "Not assigned".into(),
                    };
                    Row {
                        label: format!("Footswitch {}", i + 1),
                        help,
                        control: Control::Pills(
                            (0..shown)
                                .map(|k| {
                                    (
                                        (k + 1).to_string(),
                                        slot == Some(k),
                                        Box::new(move |this: &mut RodhareistEditorWindow, cx: &mut Cx| {
                                            this.ui.footswitches[i] = Some(k);
                                            cx.notify();
                                        })
                                            as Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>,
                                    )
                                })
                                .collect(),
                        ),
                    }
                })
                .collect()
        }
        SettingsSection::Display => vec![
            Row {
                label: "Show block names".into(),
                help: "Model names under each block on the chain".into(),
                control: Control::Toggle(
                    w.ui.show_names,
                    Box::new(|this, cx| {
                        this.ui.show_names = !this.ui.show_names;
                        cx.notify();
                    }),
                ),
            },
            Row {
                label: "Meter style".into(),
                help: "Input and output meters".into(),
                control: Control::Pills(
                    [("Peak", false), ("RMS", true)]
                        .into_iter()
                        .map(|(label, rms)| {
                            (
                                label.to_string(),
                                w.ui.meter_rms == rms,
                                Box::new(move |this: &mut RodhareistEditorWindow, cx: &mut Cx| {
                                    this.ui.meter_rms = rms;
                                    cx.notify();
                                })
                                    as Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>,
                            )
                        })
                        .collect(),
                ),
            },
        ],
        SettingsSection::About => vec![
            Row {
                label: "Model".into(),
                help: String::new(),
                control: Control::Info("RODHAREIST".into()),
            },
            Row {
                label: "Version".into(),
                help: String::new(),
                control: Control::Info(rodharerist::descriptor().version.to_string()),
            },
            Row {
                label: "NAM capture".into(),
                help: "The capture the amp block runs in NAM mode".into(),
                control: Control::Info(w.nam_loaded_info().unwrap_or_else(|| "None".into())),
            },
            Row {
                label: "Cabinet IR".into(),
                help: "The impulse response the cab block convolves".into(),
                control: Control::Info(
                    w.ir_loaded_info()
                        .map_or_else(|| "None".into(), |(name, _)| name),
                ),
            },
        ],
    }
}

fn settings(w: &RodhareistEditorWindow, p: &Params, cx: &mut Cx, s: f32) -> Vec<AnyElement> {
    let current = w.ui.settings;
    let sections = section(s, 6.0, 262.0)
        .p(px(14.0 * s))
        .gap(px(4.0 * s))
        .child(heading("Settings", s).mb(px(6.0 * s)))
        .children(SettingsSection::ALL.iter().map(|&(item, label)| {
            let on = item == current;
            let (bg, border, fg) = choice_colors(on, true);
            div()
                .id(("rodhareist-settings", item as usize))
                .flex()
                .items_center()
                .h(px(36.0 * s))
                .px(px(12.0 * s))
                .rounded(px(3.0 * s))
                .border(px(1.0))
                .border_color(border)
                .bg(bg)
                .text_color(fg)
                .text_size(type_px(13.5, s))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(color(0x2e2e2e)))
                .child(label)
                .on_click(act(cx, move |this, cx| {
                    this.ui.settings = item;
                    cx.notify();
                }))
        }));
    let title = SettingsSection::ALL
        .iter()
        .find(|(item, _)| *item == current)
        .map_or("", |(_, label)| *label);
    let rows: Vec<AnyElement> = settings_rows(w, p)
        .into_iter()
        .enumerate()
        .map(|(index, row)| settings_row(index, row, cx, s))
        .collect();
    let body = section(s, 274.0, 743.0)
        .px(px(20.0 * s))
        .py(px(16.0 * s))
        .gap(px(10.0 * s))
        .child(
            words(title, 18.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD)
                .mb(px(4.0 * s)),
        )
        .children(rows);
    vec![sections.into_any_element(), body.into_any_element()]
}

fn settings_row(index: usize, row: Row, cx: &mut Cx, s: f32) -> AnyElement {
    let entity = cx.entity().clone();
    let run = move |f: std::rc::Rc<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>| {
        let entity = entity.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, app: &mut App| {
            let f = f.clone();
            let _ = entity.update(app, |this, cx| f(this, cx));
        }
    };
    let control: AnyElement = match row.control {
        Control::Pills(options) => div()
            .flex()
            .flex_row()
            .flex_wrap()
            .justify_end()
            .max_w(px(420.0 * s))
            .gap(px(3.0 * s))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(k, (label, on, pick))| {
                        plate::pill(
                            ("rodhareist-setting", index * 64 + k),
                            label,
                            on,
                            accent(),
                            plate::PillLook::words(30.0, 10.0, 12.5),
                            true,
                            s,
                            run(std::rc::Rc::from(pick)),
                        )
                        .min_w(px(40.0 * s))
                    }),
            )
            .into_any_element(),
        Control::Toggle(on, flip) => {
            let (bg, border, knob, left) = if on {
                (accent(), accent(), color(plate::WELL), 24.0)
            } else {
                (color(0x1f1f1f), color(0x555555), color(FAINT), 3.0)
            };
            div()
                .id(("rodhareist-toggle", index))
                .relative()
                .w(px(44.0 * s))
                .h(px(24.0 * s))
                .rounded(px(12.0 * s))
                .border(px(1.0))
                .border_color(border)
                .bg(bg)
                .cursor(gpui::CursorStyle::PointingHand)
                .child(
                    div()
                        .absolute()
                        .top(px(3.0 * s))
                        .left(px(left * s))
                        .size(px(16.0 * s))
                        .rounded_full()
                        .bg(knob),
                )
                .on_click(run(std::rc::Rc::from(flip)))
                .into_any_element()
        }
        Control::Stepper(value, down, up) => {
            let step =
                |id: usize,
                 label: &'static str,
                 f: Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>| {
                    div()
                        .id(("rodhareist-step", index * 2 + id))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(30.0 * s))
                        .rounded(px(3.0 * s))
                        .border(px(1.0))
                        .border_color(color(0x484848))
                        .bg(color(0x1f1f1f))
                        .text_size(type_px(15.0, s))
                        .text_color(color(INK))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.bg(color(0x2e2e2e)))
                        .child(label)
                        .on_click(run(std::rc::Rc::from(f)))
                };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0 * s))
                .child(step(0, "\u{2212}", down))
                .child(
                    numbers(value, 13.0, WHITE, s)
                        .w(px(72.0 * s))
                        .flex()
                        .justify_center(),
                )
                .child(step(1, "+", up))
                .into_any_element()
        }
        Control::Info(value) => numbers(value, 13.0, INK, s).into_any_element(),
    };
    tile(s)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(16.0 * s))
        .min_h(px(60.0 * s))
        .px(px(14.0 * s))
        .py(px(10.0 * s))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap(px(2.0 * s))
                .child(words(row.label, 14.0, WHITE, s))
                .children((!row.help.is_empty()).then(|| words(row.help, 11.5, SOFT, s))),
        )
        .child(control)
        .into_any_element()
}

// ── The whole editor ───────────────────────────────────────────────────────

pub(crate) fn rodhareist_panel(
    w: &RodhareistEditorWindow,
    window: &Window,
    cx: &mut Cx,
) -> AnyElement {
    let viewport = window.viewport_size();
    let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
    let p = w.params_snapshot();
    let focus = resolve_focus(&p.stage_order, w.chain_focus);
    let mut children = vec![display(w, &p, focus, cx, s)];
    children.extend(match w.ui.page {
        Page::Home => home(w, &p, cx, s),
        Page::Explore => explore(w, &p, focus, cx, s),
        Page::Pedalboard => pedalboard(w, &p, focus, cx, s),
        Page::Settings => settings(w, &p, cx, s),
    });
    plate_frame(s, children)
}

/// The tempo two taps `interval` apart give, if they are close enough to
/// count as one gesture.
pub(crate) fn tapped_interval(last: Option<Instant>, now: Instant) -> Option<Duration> {
    let interval = now.duration_since(last?);
    (TAP_MIN..=TAP_WINDOW)
        .contains(&interval)
        .then_some(interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_falls_back_when_its_block_leaves_the_path() {
        let mut order = [None; rodharerist::PATH_SLOTS];
        order[2] = Some(StageKind::Amp);
        order[5] = Some(StageKind::Cab);
        assert_eq!(
            resolve_focus(&order, Some(ChainFocus::Block(StageKind::Cab))),
            ChainFocus::Block(StageKind::Cab)
        );
        assert_eq!(
            resolve_focus(&order, Some(ChainFocus::Block(StageKind::Reverb))),
            ChainFocus::Block(StageKind::Amp)
        );
        assert_eq!(resolve_focus(&[None; 4], None), ChainFocus::Empty(0));
    }

    #[test]
    fn placing_a_model_uses_a_free_instance_then_moves_the_first() {
        let mut p = rodharerist::default_params();
        p.stage_order = [None; rodharerist::PATH_SLOTS];
        p.stage_order[0] = Some(StageKind::Drive);
        // A second drive goes in as Drive B.
        assert_eq!(place(&mut p, 1, Category::Drive, 2), StageKind::Drive2);
        assert_eq!(blocks::models(&p, StageKind::Drive2).1, Some(2));
        // A third: both are on the path, so Drive A moves here.
        assert_eq!(place(&mut p, 4, Category::Drive, 0), StageKind::Drive);
        assert_eq!(p.stage_order[0], None);
        assert_eq!(p.stage_order[4], Some(StageKind::Drive));
        // Same category in the slot: only the model changes.
        assert_eq!(place(&mut p, 4, Category::Drive, 5), StageKind::Drive);
        assert_eq!(blocks::models(&p, StageKind::Drive).1, Some(5));
    }

    #[test]
    fn every_library_model_has_a_line() {
        let p = rodharerist::default_params();
        for entry in entries(&p) {
            assert!(!model_desc(entry.label).is_empty(), "{}", entry.label);
        }
    }

    #[test]
    fn footswitches_follow_the_chain_without_amp_and_cab() {
        let p = rodharerist::default_params();
        let ui = RodUi::default();
        let slots = footswitch_slots(&ui, &p);
        for slot in slots.into_iter().flatten() {
            let kind = p.stage_order[slot].expect("an occupied slot");
            assert!(!matches!(Category::of(kind), Category::Amp | Category::Cab));
        }
    }

    #[test]
    fn notes_read_with_cents() {
        let (name, cents) = note_name(82.41).unwrap();
        assert_eq!(name, "E");
        assert!(cents.abs() < 1.0);
        assert!(note_name(0.0).is_none());
    }

    #[test]
    fn the_chain_shows_seven_positions_at_least() {
        let p = rodharerist::default_params();
        assert_eq!(shown_slots(&[None; rodharerist::PATH_SLOTS]), CHAIN_VISIBLE);
        assert!(shown_slots(&p.stage_order) >= p.stage_order.iter().flatten().count());
    }
}
