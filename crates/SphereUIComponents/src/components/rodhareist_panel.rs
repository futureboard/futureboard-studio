//! Rodhareist's native editor view, after its design: the signal chain in
//! the well — input and output meters, the path's blocks, the selected
//! block's caption — and four pages on the deck under it:
//!
//! * **Home** — factory and user presets, the preset name, the cabinet IR,
//!   and a performance view: four footswitches, the tuner, tap tempo,
//!   lane 1's wet/dry/wet send, and master;
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
    div, px, AnyElement, App, AppContext, Context, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, Rgba, StatefulInteractiveElement, Styled, Window,
};
use rodharerist::{
    AmpClass, AmpModel, CabModel, DelaySync, Line2Mode, Params, RowSend, StageChannel, StageKind,
    ToneEngineKind,
};

use crate::components::controls::fb_tooltip;
use crate::components::plate::{
    self, caption, color, color_a, icon, knob_typed, numbers, plate_frame, plate_scale, type_px,
    words, Dial, DialUnit, KnobCell, KnobLook, FAINT, INK, PALE, SOFT, WHITE,
};
use crate::components::rodhareist_blocks::{self as blocks, Category, ParamSpec};
use crate::components::rodhareist_window::RodhareistEditorWindow;

type Cx<'a> = Context<'a, RodhareistEditorWindow>;

/// Rodhareist's accent.
const ACCENT: u32 = 0xf25c54;
/// Chain geometry, at the design size. The well is 327 tall, so a block is
/// smaller than the original 99 px plate: two lanes fit at 72 px, four still
/// clear the header and the caption. The send control sits at the end of
/// the lane, past the blocks and short of the output meter.
const CHAIN_LEFT: f32 = 108.0;
const BLOCK_RUN: f32 = 620.0;
const SEND_LEFT: f32 = 740.0;
const LANE_TOP: f32 = 46.0;
const LANE_BOTTOM: f32 = 302.0;
const CAPTION_TOP: f32 = 308.0;
/// The block size the type and icon were drawn for.
const BLOCK_REF: f32 = 72.0;
/// Positions the chain shows before it scrolls.
const CHAIN_VISIBLE: usize = 7;
/// Taps further apart than this start a new tempo. Delay A's longest time
/// (1200 ms, 50 BPM): a slower tap would set a time the delay cannot play.
const TAP_WINDOW: Duration = Duration::from_millis(1_200);
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

/// Which preset list the Home sidebar shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum PresetTab {
    #[default]
    Factory,
    Users,
}

/// What the editor shows and how — never saved, never sent to the plug-in.
#[derive(Clone, Debug)]
pub(crate) struct RodUi {
    pub page: Page,
    /// The Home sidebar's preset list.
    pub preset_tab: PresetTab,
    /// The Explore page's family, `None` for every block.
    pub family: Option<Family>,
    pub settings: SettingsSection,
    /// The slot each footswitch toggles; `None` follows the chain.
    pub footswitches: [Option<usize>; 4],
    pub show_names: bool,
    /// Meters read RMS rather than peak.
    pub meter_rms: bool,
    pub last_tap: Option<Instant>,
    /// The block being dragged along the chain, while the drag lasts.
    pub drag_block: Option<StageKind>,
    /// The chain layout last asked for, and the move toward it on screen.
    pub chain_look: Option<ChainLook>,
    pub chain_motion: Option<ChainMotion>,
    /// The tuner page's moving parts.
    pub tuner: TunerLook,
    /// Explore's amp subtab (Clean, Crunch, High Gain, NAM); `None` for all.
    pub explore_amp: Option<Family>,
    /// The Pedalboard amp list's subtab; `None` shows the selected model's.
    pub board_amp: Option<Family>,
}

/// How long the tuner keeps the last note on screen between notes.
const TUNER_HOLD: Duration = Duration::from_millis(400);
/// A reading this close to the note counts as in tune.
const IN_TUNE_CENTS: f32 = 3.0;
/// Strobe drift: periods per second, per cent off the note.
const STROBE_RATE: f32 = 0.08;

/// The tuner page's needle, strobe and in-tune pulse, eased toward the DSP's
/// reading on each frame. Animation only while something is moving: a
/// reading off the note, the needle settling, the pulse, or a held note
/// fading out.
#[derive(Clone, Debug, Default)]
pub(crate) struct TunerLook {
    /// The needle, in cents (-50..50), as drawn.
    pub cents: f32,
    /// The strobe band's offset, in stripe periods (0..1).
    pub strobe: f32,
    /// The note on screen: name, octave, cents. Held for [`TUNER_HOLD`]
    /// after the pitch goes away.
    pub note: Option<(&'static str, i32, f32)>,
    /// When a pitch was last heard.
    pub heard: Option<Instant>,
    /// When the reading came into tune, for the pulse.
    pub in_tune_since: Option<Instant>,
    last: Option<Instant>,
}

/// The note `hz` is nearest to: name, octave (A4 is 440 Hz) and how far off
/// it is in cents.
pub(crate) fn pitch_of(hz: f32) -> Option<(&'static str, i32, f32)> {
    let (name, cents) = note_name(hz)?;
    let n = (12.0 * (hz / 440.0).log2() + 69.0).round() as i32;
    Some((name, n.div_euclid(12) - 1, cents))
}

/// Moves the tuner's look one frame toward `reading` (the DSP's pitch while
/// the tuner is on). Returns whether another frame is needed.
pub(crate) fn tuner_frame(
    look: &mut TunerLook,
    reading: Option<(&'static str, i32, f32)>,
    now: Instant,
) -> bool {
    let dt = look
        .last
        .map_or(0.0, |last| now.duration_since(last).as_secs_f32())
        .min(0.1);
    look.last = Some(now);
    let target = match reading {
        Some(note) => {
            look.note = Some(note);
            look.heard = Some(now);
            note.2.clamp(-50.0, 50.0)
        }
        None => {
            let holding = look
                .heard
                .is_some_and(|heard| now.duration_since(heard) < TUNER_HOLD);
            if !holding {
                look.note = None;
            }
            if holding {
                look.cents
            } else {
                0.0
            }
        }
    };
    // Settles in about the FAST motion time (three time constants).
    let tau = crate::theme::motion::FAST_MS as f32 / 1_000.0 / 3.0;
    look.cents += (target - look.cents) * (1.0 - (-dt / tau).exp());
    let live = reading.is_some();
    let off = reading.map_or(0.0, |note| note.2);
    if live {
        look.strobe = (look.strobe + off.clamp(-50.0, 50.0) * STROBE_RATE * dt).rem_euclid(1.0);
    }
    let in_tune = live && off.abs() < IN_TUNE_CENTS;
    // A note that stops while in tune keeps that state through the hold; one
    // heard off the pitch clears it.
    if in_tune {
        look.in_tune_since.get_or_insert(now);
    } else if live {
        look.in_tune_since = None;
    }
    let pulsing = look.in_tune_since.is_some_and(|since| {
        now.duration_since(since) < Duration::from_millis(crate::theme::motion::SLOW_MS)
    });
    let holding = look.note.is_some() && !live;
    (target - look.cents).abs() > 0.05 || (live && off.abs() >= 0.5) || pulsing || holding
}

/// The editor's two text fields, built by the window (they need its entity
/// for the OS input handler) and placed by the pages.
pub(crate) struct RodFields {
    pub search: AnyElement,
    pub preset_name: AnyElement,
    /// A knob's type-in field and the window point it opens at (where the
    /// knob or its value was double-clicked).
    pub knob_value: Option<(AnyElement, gpui::Point<gpui::Pixels>)>,
}

impl Default for RodUi {
    fn default() -> Self {
        Self {
            page: Page::Home,
            preset_tab: PresetTab::Factory,
            family: None,
            settings: SettingsSection::Audio,
            footswitches: [None; 4],
            show_names: true,
            meter_rms: false,
            last_tap: None,
            drag_block: None,
            chain_look: None,
            chain_motion: None,
            tuner: TunerLook::default(),
            explore_amp: None,
            board_amp: None,
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
    /// Browse group for clean amp voices. Not a chain-block family.
    Clean,
    /// Browse group for crunch amp voices.
    Crunch,
    /// Browse group for high-gain amp voices.
    HighGain,
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
            Category::Wah | Category::Pitch => Family::Filter,
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
            Family::Filter => &[Category::Wah, Category::Pitch],
            Family::Clean | Family::Crunch | Family::HighGain => &[Category::Amp],
        }
    }

    /// The category a browse group sits under: the amp's gain classes are
    /// subtabs of Amp (#407).
    fn parent(self) -> Family {
        match self {
            Family::Clean | Family::Crunch | Family::HighGain => Family::Amp,
            other => other,
        }
    }

    /// The amp's subtabs, in order. [`Family::Amp`] as a subtab is the NAM
    /// capture engine.
    const AMP_TABS: [(Family, &'static str); 4] = [
        (Family::Clean, "Clean"),
        (Family::Crunch, "Crunch"),
        (Family::HighGain, "High Gain"),
        (Family::Amp, "NAM"),
    ];

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
            Family::Clean => "Clean",
            Family::Crunch => "Crunch",
            Family::HighGain => "High Gain",
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
            Family::Clean => "CLEAN",
            Family::Crunch => "CRUNCH",
            Family::HighGain => "GAIN",
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
            Family::Clean => 0x8fd3a8,
            Family::Crunch => 0xe0a045,
            Family::HighGain => 0xf25c54,
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
            Family::Clean | Family::Crunch | Family::HighGain => {
                "M3 6 H21 V18 H3 Z M3 13 H21 M7 9.5 V9.6 M11 9.5 V9.6 M15 9.5 V9.6"
            }
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
        "Princeton" => "Small blackface combo, earlier breakup than a Twin.",
        "Custom 100" => "High-headroom British clean that stays tight.",
        "Mark Lead" => "Mid-forward high gain, tighter than Recto.",
        "Normal 15" => "Earlier British chime, less gain than Top Boost.",
        "Jazz 120" => "Solid-state clean. Stays clean when a Twin would break up.",
        "Deluxe 5" => "Loose small combo. Breaks up early and sags.",
        "JTM 45" => "Rounder, lower-gain plexi.",
        "Brown Panel" => "Hot plexi: more gain, less feedback, more sag.",
        "Fireball" => "Tight modern high gain with almost no sag.",
        "SLO Lead" => "Singing mids. Less scooped than the modern stack.",
        "Liverpool" => "EL84 chime with more clean than Normal 15.",
        "Bass 800" => "Deep, tight bass head.",
        "Stealth Lead" => "Later 5150 lead. More mids and a stiffer power section than Invader.",
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
        "Alnico 2x12" => "Open pair with a strong speaker bell.",
        "Greenback 4x12" => "Closed 4x12, mid honk, early rolloff.",
        "Jensen 1x12" => "Paper-cone single, darker than the American 1x12.",
        "Cream 4x12" => "Tight closed 4x12 with more top than a greenback.",
        "Bass 8x10" => "Deeper and tighter than the Bass Cabinet.",
        "Silver Bell 1x12" => "Bright open single with a high bell.",
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
        "Stereo Delay" => "Clean repeats, each side on its own time and feedback.",
        "Studio Plate" => "Bright, dense studio plate.",
        "Tracking Room" => "Short, damped room that adds depth.",
        "Concert Hall" => "Big, slow-blooming space.",
        "Shimmer" => "Hall with an octave-up voice in the tail.",
        "Studio EQ" => "Shelves and two sweepable bells.",
        "Vintage EQ" => "Passive-console shelves and wide bells.",
        "Modern EQ" => "Extended shelves and narrow, precise bells.",
        "Cry Wah" => "Classic sweep: park it or rock it.",
        "Touch Wah" => "Envelope filter that follows your pick.",
        "Glass Boost" => "Treble boost. The low strings stay clean.",
        "Velvet OD" => "Soft, low-gain overdrive with a warm mid.",
        "Octave Fuzz" => "Strong even harmonics. An octave sits on the note.",
        "Bass Grind" => "The low band reaches the clipper. Dark on top.",
        "Honk Box" => "A narrow mid peak into a medium clip.",
        "Smooth Lead" => "Compressed sustain with the top rolled off.",
        "Sputter Fuzz" => "Opens on the attack and chokes as the note decays.",
        "Clean Push" => "Almost clean level, with a hint of soft clip.",
        "Park Head" => "Rounder, lower-gain British head. More bass, more sag.",
        "Silver Twin" => "Brighter and stiffer than Twin Clean.",
        "Thick Stack" => "Huge low-mids, soft clip, a lot of sag.",
        "Mini Chime" => "Small combo. Early breakup, fizzy top.",
        "Hot Mod" => "A hot-rodded plexi. Tight mids, more feedback.",
        "Vintage Modern" => "High gain that sags. Looser than Recto Modern.",
        "Studio Bass" => "Mid-forward bass. Less sub than Bass 800.",
        "Express Lead" => "Little feedback. The power section lets go.",
        "Closed 1x12" => "Small sealed single. The top rolls off early.",
        "Ported 2x12" => "A tuned port under a closed pair.",
        "Open 4x10" => "Four tens, open back, strong rear cancellation.",
        "Ceramic 2x12" => "Tight pair with less bell than the alnico.",
        "Deep 4x12" => "Lower and looser than the oversized 4x12.",
        "Combo 1x15" => "Open fifteen. Dark and loose.",
        "Alnico 1x10" => "Small bright single. Boxy.",
        "Dark 1x15" => "Sealed fifteen with very little top.",
        "Dimension Double" => "Short, quiet doubling. Less swirl than the chorus.",
        "Tri Chorus" => "A longer, slower chorus with a wider offset.",
        "Thru Zero" => "The sweep passes close to the dry signal.",
        "Slow Jet" => "A slow flanger with more regeneration.",
        "Harmonic Trem" => "Lows and highs pulse against each other.",
        "Bias Trem" => "A slow, soft wobble. Less chop than the opto.",
        "Slap Echo" => "Bright repeats that stay put.",
        "Oil Can" => "Dark, saturated, the pitch wanders.",
        "Dark Echo" => "Repeats lose their top and their low end.",
        "Ambient Echo" => "Long, quiet repeats alternating across the image.",
        "Wow Echo" => "Tape movement turned up until the pitch drifts.",
        "Triplet Tap" => "Quarter note left against a triplet right.",
        "Spring Tank" => "Short and sparse, with a lot of early ring.",
        "Drum Chamber" => "Medium room, darker than the hall.",
        "Ambience" => "A very short early field. Almost no tail.",
        "Dark Hall" => "Hall length with the top rolled off.",
        "Air EQ" => "A very high shelf and wide bells.",
        "Bass Shelf" => "The low shelf sits lower. Bells stay broad.",
        "Presence EQ" => "The high shelf comes in around the presence band.",
        "Notch EQ" => "Narrow bells for small cuts.",
        "Razor Saw" => "Hard clip with the mids cut out.",
        "Djent Clamp" => "The low strings stay clean. What clips is tight.",
        "Chainsaw" => "Extreme gain and a narrow mid that screams.",
        "Doom Wall" => "The whole band clips. Dark and huge.",
        "Mid Spike" => "A narrow mid peak into a hard clip.",
        "Ice Pick" => "The top clips. The low mids are cut.",
        "Gate Grind" => "Opens hard and chokes between notes.",
        "Red Lead" => "Tight, mid-forward, high gain.",
        "Razor Modern" => "Four stages, scooped, almost no sag.",
        "Mid Cannon" => "Mid-forward high gain that stays tight.",
        "Djent Stack" => "The lows are filtered before the first stage.",
        "Chainsaw Lead" => "A lot of gain and a mid that cuts.",
        "Blackfire" => "Stiff modern high gain with more presence than scoop.",
        "Crunch Saw" => "British mids pushed until the power section grinds.",
        "Ultra Scoop" => "The mids are gone. The lows stay tight.",
        "Slam Head" => "Tight lows, a forward mid, a hard power section.",
        "Razor 4x12" => "Tight closed 4x12. The mids are cut, the top stays.",
        "Honk Stack 4x12" => "A hard mid and an early top rolloff.",
        "Djent 4x12" => "High-passed and scooped, with very little body.",
        "Saw Stack 4x12" => "A mid spike and a harsh top.",
        "Cannon 4x12" => "Mid-forward and tight, less scoop than a modern stack.",
        "Ice 4x12" => "Bright and thin. The low mids are gone.",
        "Doom 8x10" => "Huge low end, dark, compressed.",
        "Tight 2x12" => "A small sealed pair, scooped and fast.",
        "Razor Chorus" => "Deep and fast, the second line a half cycle away.",
        "Metal Jet" => "A very short flanger with hard regeneration.",
        "Chainsaw Phase" => "Six stages, high feedback, a fast narrow sweep.",
        "Chop Trem" => "A fast, hard volume chop.",
        "Panic Vibe" => "Staggered stages that snap instead of swirl.",
        "Stab Flange" => "Negative feedback, short and fast.",
        "Square Trem" => "The wave sits near a square.",
        "Shred Phase" => "Twelve stages across the mid, high feedback.",
        "Smash Box" => "A tight knee and a fast attack.",
        "Brick Limit" => "Almost no knee. The ratio is pushed toward a wall.",
        "Pick Attack" => "The sidechain ignores the low strings.",
        "Deep Pump" => "A long release and a threshold that sits lower.",
        "Mid Punch" => "The sidechain listens to the mids.",
        "Bass Hold" => "The low end triggers it. The attack is slower.",
        "Parallel Crush" => "A wide knee and a ratio that still crushes.",
        "Opto Slam" => "A slow attack that then clamps hard.",
        "Bass Wah" => "The sweep sits an octave lower.",
        "Vocal Wah" => "A narrow, more resonant vowel.",
        "Parked Filter" => "A short travel around one mid.",
        "Talk Filter" => "A wide envelope sweep with more dry body under it.",
        "Herbert Lead" => "Four stages, tight and mid-forward, with very little sag.",
        "VH4 Lead" => "Scooped modern lead. Stiffer than Herbert, less mid.",
        "Ecstasy Red" => "British high gain that blooms and sags.",
        "Uberschall" => "Huge gain, a lot of sag, a big low end.",
        "Savage Lead" => "Bright modern lead. The top stays forward.",
        "Archon Lead" => "Forward mids, tight, less scoop than a modern stack.",
        "Deliverance" => "Three stages. Articulate, less scooped than the four-stage leads.",
        "Jubilee Lead" => {
            "Three-stage British crunch. More gain than a plexi, less than a modern lead."
        }
        "DSL Lead" => "Tighter British lead. Less sag than Jubilee.",
        "Deluxe Reverb" => "Two-stage American clean. High headroom, a little sag.",
        "Tangerine 100" => "Modern British dirt. Thick low mids, tighter than Mandarin.",
        "Dark Citrus 15" => "A small EL84 head run hot. Dark, compressed, saturated.",
        "Plus Lead 120" => "Four-stage American lead. More mid than Invader.",
        "Quad Ten Reverb" => "Blackface clean into four tens. Breaks up before a Twin.",
        "Neo Clean 250" => "A tube preamp into a stiff power stage. Huge headroom.",
        "Iron Lead 100" => "Four tight British stages with a heavy mid.",
        "Greenback 2x12" => "A vintage pair. Mid honk, the top rolls off early.",
        "Alnico 4x12" => "Closed 4x12 with a strong speaker bell.",
        "Vintage 1x15" => "Open fifteen. Loose and dark.",
        "Bass 4x10" => "Four tens for bass. Tight, extended low.",
        "Dark 1x12" => "A small closed single with very little top.",
        "Modern 1x12" => "A tight closed single. More top than the dark 1x12.",
        "V30 Stack 4x12" => "Closed 4x12. Mid-forward, the top stays.",
        "Cream 2x12" => "A pair with more top than a greenback.",
        "Ported 4x12" => "A tuned port under a closed 4x12. Deeper than a sealed stack.",
        "Open 2x10" => "Two tens, open back, strong rear cancellation.",
        "Tangerine 4x12" => "A big closed 4x12. Thick low mids, a rounder top.",
        "Neo 1x12" => "A sealed single on a flat speaker. Tight and extended.",
        "Neo 2x12" => "A vintage and a flat speaker in one box. Airy, tight.",
        "Lead Straight 4x12" => "A straight closed 4x12. Tight, bright, the low mids cut.",
        "T75 Straight 4x12" => "Scooped 75 W speakers. A crisp top and a tight low.",
        "Octave Up" => "The note an octave above, mixed with the dry guitar.",
        "Octave Down" => "The note an octave below.",
        "Detune" => "Two voices, a little sharp and a little flat.",
        "Fifth Up" => "A fifth above the note.",
        "Fourth Down" => "A fourth below the note.",
        "Shift" => "The semitone and cents knobs set the interval.",
        _ => "",
    }
}

/// One model on the Explore page: its category and its index in the
/// category's model list.
#[derive(Clone, Copy, Debug)]
struct Entry {
    category: Category,
    /// Index into [`blocks::models`], which is what [`place`] writes.
    index: usize,
    label: &'static str,
    /// The Explore group. Amp voices carry their gain class (a subtab of
    /// Amp, see [`Family::parent`]); the chain block stays [`Family::Amp`].
    browse: Family,
}

fn amp_browse(choice_wire: &str, value: f32) -> Family {
    if choice_wire == "tone_engine" {
        return Family::Amp;
    }
    match AmpModel::from_index(value.round().max(0.0) as u32).class() {
        AmpClass::Clean => Family::Clean,
        AmpClass::Crunch => Family::Crunch,
        AmpClass::HighGain => Family::HighGain,
    }
}

fn entries(p: &Params) -> Vec<Entry> {
    let mut out = Vec::new();
    for family in Family::ALL {
        if family == Family::Amp {
            // In subtab order, so "All" lists Clean, Crunch, High Gain, NAM.
            let (list, _) = blocks::models(p, StageKind::Amp);
            for (class, _) in Family::AMP_TABS {
                for (index, choice) in list.iter().enumerate() {
                    if amp_browse(choice.wire_id, choice.value) != class {
                        continue;
                    }
                    out.push(Entry {
                        category: Category::Amp,
                        index,
                        label: choice.label,
                        browse: class,
                    });
                }
            }
        } else {
            for category in family.categories() {
                let (list, _) = blocks::models(p, category.kinds()[0]);
                for (index, choice) in list.into_iter().enumerate() {
                    out.push(Entry {
                        category: *category,
                        index,
                        label: choice.label,
                        browse: family,
                    });
                }
            }
        }
    }
    out
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

/// One lane of blocks: size and pitch at the design size, and how tall
/// the lane is.
struct LaneGeom {
    size: f32,
    pitch: f32,
    lane_h: f32,
}

/// How many lanes are in use: the main path, and every lane down to the
/// last one holding a block or sending. The rest stay closed until a drag
/// opens the next one ([`target_look`]); the well stops at four.
fn lane_count(p: &Params) -> usize {
    let mut highest = 0u8;
    for stage in p.stage_order.iter().flatten() {
        highest = highest.max(p.stage_row[*stage as usize].min(3));
    }
    for (index, send) in p.row_send.iter().enumerate() {
        if *send != RowSend::Off {
            highest = highest.max(index as u8 + 1);
        }
    }
    (highest as usize + 1).clamp(1, 4)
}

/// Whether any block on the path sits on lane `row` (0-based).
fn lane_holds_blocks(p: &Params, row: usize) -> bool {
    p.stage_order
        .iter()
        .flatten()
        .any(|kind| p.stage_row[*kind as usize] as usize == row)
}

/// Keeps each send lane's mode in step with its blocks after a move: a
/// lane that just got its first block starts Parallel (an Off lane does not
/// run, so the block would go silent), and a lane its last block left goes
/// back to Off, which closes it.
fn settle_lane_sends(before: &Params, next: &mut Params) {
    for index in 0..next.row_send.len() {
        let row = index + 1;
        let had = lane_holds_blocks(before, row);
        let has = lane_holds_blocks(next, row);
        if has && !had && next.row_send[index] == RowSend::Off {
            next.row_send[index] = RowSend::Parallel;
        } else if had && !has {
            next.row_send[index] = RowSend::Off;
        }
    }
}

/// Moves `kind` to lane `row` in place, settling the lane sends.
fn move_to_lane(p: &mut Params, kind: StageKind, row: u8) {
    let before = p.clone();
    rodharerist::apply_to_params(p, kind.row_id(), row as f32);
    settle_lane_sends(&before, p);
}

/// How long a chain change takes to settle on screen.
const CHAIN_MOTION: Duration = Duration::from_millis(300);

/// The chain as laid out, at the design size: the lanes drawn, the
/// positions shown, block size and pitch, lane height, and where each block
/// on the path sits (`kind`, lane, left, top).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChainLook {
    lanes: usize,
    shown: usize,
    size: f32,
    pitch: f32,
    lane_h: f32,
    blocks: Vec<(StageKind, u8, f32, f32)>,
}

impl ChainLook {
    fn geom(&self) -> LaneGeom {
        LaneGeom {
            size: self.size,
            pitch: self.pitch,
            lane_h: self.lane_h,
        }
    }

    /// The look `t` of the way from `self` to `to`. Blocks that are only in
    /// `to` sit at their place; the lane count is the larger of the two so a
    /// closing lane can fade.
    fn toward(&self, to: &ChainLook, t: f32) -> ChainLook {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        ChainLook {
            lanes: self.lanes.max(to.lanes),
            shown: to.shown,
            size: mix(self.size, to.size),
            pitch: mix(self.pitch, to.pitch),
            lane_h: mix(self.lane_h, to.lane_h),
            blocks: to
                .blocks
                .iter()
                .map(|&(kind, row, left, top)| {
                    match self.blocks.iter().find(|entry| entry.0 == kind) {
                        Some(&(_, _, from_left, from_top)) => {
                            (kind, row, mix(from_left, left), mix(from_top, top))
                        }
                        None => (kind, row, left, top),
                    }
                })
                .collect(),
        }
    }
}

/// A chain change in flight on screen. View state only.
#[derive(Clone, Debug)]
pub(crate) struct ChainMotion {
    from: ChainLook,
    to: ChainLook,
    start: Instant,
}

impl ChainMotion {
    fn progress(&self, now: Instant) -> f32 {
        (now.duration_since(self.start).as_secs_f32() / CHAIN_MOTION.as_secs_f32()).clamp(0.0, 1.0)
    }

    fn frame(&self, raw: f32) -> ChainFrame {
        let t = 1.0 - (1.0 - raw).powi(3);
        ChainFrame {
            look: self.from.toward(&self.to, t),
            t,
            from_lanes: self.from.lanes,
            to_lanes: self.to.lanes,
            changed_lane: self
                .to
                .blocks
                .iter()
                .filter(|(kind, row, _, _)| {
                    self.from
                        .blocks
                        .iter()
                        .any(|entry| entry.0 == *kind && entry.1 != *row)
                })
                .map(|entry| entry.0)
                .collect(),
        }
    }
}

/// What the well draws this frame.
pub(crate) struct ChainFrame {
    look: ChainLook,
    /// How far the current move has got, eased; 1 at rest.
    t: f32,
    from_lanes: usize,
    to_lanes: usize,
    /// Blocks that are changing lane in this move.
    changed_lane: Vec<StageKind>,
}

impl ChainFrame {
    fn at_rest(look: ChainLook) -> Self {
        let lanes = look.lanes;
        Self {
            look,
            t: 1.0,
            from_lanes: lanes,
            to_lanes: lanes,
            changed_lane: Vec::new(),
        }
    }

    /// A lane that is opening fades in, one that is closing fades out.
    fn lane_alpha(&self, row: usize) -> f32 {
        if row >= self.to_lanes {
            1.0 - self.t
        } else if row >= self.from_lanes {
            self.t
        } else {
            1.0
        }
    }
}

/// The chain's layout for `p`. While a block is being dragged the next
/// closed lane opens as a drop target.
fn target_look(p: &Params, dragging: bool) -> ChainLook {
    let used = lane_count(p);
    let lanes = if dragging { (used + 1).min(4) } else { used };
    let shown = shown_slots(&p.stage_order);
    let geom = lane_geom(lanes, shown);
    let blocks = p
        .stage_order
        .iter()
        .enumerate()
        .take(shown)
        .filter_map(|(slot, entry)| {
            let kind = (*entry)?;
            let row = p.stage_row[kind as usize].min(3);
            let (left, top) = slot_origin(&geom, slot, row as usize);
            Some((kind, row, left, top))
        })
        .collect();
    ChainLook {
        lanes,
        shown,
        size: geom.size,
        pitch: geom.pitch,
        lane_h: geom.lane_h,
        blocks,
    }
}

/// The well's layout for this frame. Any change of lanes or places — a
/// drop, a Move or Lane button, a preset, an undo, a drag opening a lane —
/// starts a move from wherever the blocks are drawn now. Returns the frame
/// and whether it is still moving (the caller asks for another frame).
pub(crate) fn chain_frame(ui: &mut RodUi, p: &Params, now: Instant) -> (ChainFrame, bool) {
    let target = target_look(p, ui.drag_block.is_some());
    if ui.chain_look.as_ref() != Some(&target) {
        if let Some(previous) = ui.chain_look.take() {
            let from = match &ui.chain_motion {
                Some(motion) => motion.frame(motion.progress(now)).look,
                None => previous,
            };
            ui.chain_motion = Some(ChainMotion {
                from,
                to: target.clone(),
                start: now,
            });
        }
        ui.chain_look = Some(target.clone());
    }
    match &ui.chain_motion {
        Some(motion) => {
            let raw = motion.progress(now);
            if raw >= 1.0 {
                ui.chain_motion = None;
                (ChainFrame::at_rest(target), false)
            } else {
                (motion.frame(raw), true)
            }
        }
        None => (ChainFrame::at_rest(target), false),
    }
}

fn lane_geom(lanes: usize, shown: usize) -> LaneGeom {
    let lanes = lanes.clamp(1, 4);
    let shown = shown.max(1);
    let lane_h = (LANE_BOTTOM - LANE_TOP) / lanes as f32;
    let gap = 6.0;
    let fit = (BLOCK_RUN - gap * (shown - 1) as f32) / shown as f32;
    let size = fit.min(lane_h - 8.0).min(BLOCK_REF);
    LaneGeom {
        size,
        pitch: size + gap,
        lane_h,
    }
}

fn slot_origin(geom: &LaneGeom, slot: usize, row: usize) -> (f32, f32) {
    let left = CHAIN_LEFT + slot as f32 * geom.pitch;
    let top = LANE_TOP + row as f32 * geom.lane_h + (geom.lane_h - geom.size) * 0.5;
    (left, top)
}

/// What a chain drag carries. Editor-local: the audio thread never sees it.
#[derive(Clone, Copy)]
struct ChainDrag {
    slot: usize,
    kind: StageKind,
}

struct ChainGhost {
    name: &'static str,
}

impl Render for ChainGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(2.0))
            .bg(color(0xd9d9d9))
            .text_color(color(0x111111))
            .text_size(px(12.0))
            .child(self.name)
    }
}

/// Drop writes `stage_order` and, when the lane changed, `stage_row`.
fn drop_chain(
    cx: &Cx,
    slot: usize,
    row: u8,
) -> impl Fn(&ChainDrag, &mut Window, &mut App) + 'static {
    let entity = cx.entity().clone();
    move |drag, _window, app| {
        let kind = drag.kind;
        let _ = entity.update(app, |this, cx| {
            let before = this.params_snapshot();
            let mut next = before.clone();
            let from = next
                .stage_order
                .iter()
                .position(|entry| *entry == Some(kind))
                .unwrap_or(drag.slot);
            rodharerist::reorder_chain(&mut next.stage_order, &mut next.stage_row, from, slot, row);
            settle_lane_sends(&before, &mut next);
            this.ui.drag_block = None;
            this.chain_focus = Some(ChainFocus::Block(kind));
            this.set_params(next, cx);
        });
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

pub(crate) fn note_name(hz: f32) -> Option<(&'static str, f32)> {
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

/// [`act`] for a handler that also needs the window (focus moves).
fn act_in(
    cx: &Cx,
    f: impl Fn(&mut RodhareistEditorWindow, &mut Window, &mut Cx) + 'static,
) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
    let entity = cx.entity().clone();
    move |_event, window, app: &mut App| {
        let _ = entity.update(app, |this, cx| f(this, window, cx));
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

/// A knob for a block parameter, in its family's colour, in a cell `width`
/// wide for its label.
#[allow(clippy::too_many_arguments)]
fn block_knob(
    cx: &mut Cx,
    spec: ParamSpec,
    value: f32,
    ink: Rgba,
    size: f32,
    width: f32,
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
    let cell = KnobCell::new(width, 11.0, 12.5)
        .gap(3.0)
        .label(0.08, PALE)
        .label_below();
    let id = spec.id;
    let spec_min = spec.min;
    let spec_max = spec.max;
    let spec_label = spec.label;
    let element = knob_typed(
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
        move |this, window, cx| {
            this.begin_knob_entry(id, spec_label, spec_min, spec_max, window, cx)
        },
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
    // No tab is current while the tuner has the deck.
    let tuner_up = p.tuner_on;
    // The tabs follow the wordmark, so the controls on the right never run
    // into them.
    let nav = div()
        .flex()
        .flex_row()
        .h_full()
        .ml(px(22.0 * s))
        .gap(px(2.0 * s))
        .children(Page::ALL.iter().map(|&(item, label)| {
            let current = item == page && !tuner_up;
            div()
                .id(("rodhareist-nav", item as usize))
                .flex()
                .items_center()
                .h_full()
                .px(px(10.0 * s))
                .border_b(px(2.0 * s))
                .border_color(if current { accent() } else { color_a(0, 0.0) })
                .text_size(type_px(14.5, s))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color(if current { WHITE } else { SOFT }))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.text_color(color(WHITE)))
                .child(label)
                .on_click(act_in(cx, move |this, window, cx| {
                    this.ui.page = item;
                    // A tab leaves the tuner, too.
                    this.close_tuner_page(cx);
                    // The field on the page being left stops taking keys.
                    this.release_text_focus(window, cx);
                    if item == Page::Home {
                        this.refresh_user_presets();
                    }
                    cx.notify();
                }))
        }));
    let tempo = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .h(px(28.0 * s))
        .px(px(10.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(color(plate::WELL_EDGE))
        .bg(color(plate::GROUND))
        .child(caption("BPM", 10.0, 0.1, FAINT, s))
        .child(numbers(
            w.host_tempo()
                .map_or_else(|| "\u{2014}".to_string(), |bpm| format!("{}", bpm.round())),
            13.5,
            INK,
            s,
        ));
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
        .h(px(28.0 * s))
        .px(px(11.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(type_px(12.5, s))
        .font_weight(FontWeight::MEDIUM)
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
        .pl(px(14.0 * s))
        .pr(px(10.0 * s))
        .gap(px(8.0 * s))
        .border_b(px(1.0))
        .border_color(color_a(0xffffff, 0.06))
        .child(
            words(plate::spaced("RODHAREIST", 0.0), 16.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD),
        )
        .child(nav)
        .child(div().flex_1())
        .child(key_control(p, cx, s))
        .child(tempo)
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

/// The input or output level under its meter pair, centred on `mid`: a
/// small knob on the global trim `id`, labelled like the meter.
fn level_knob(
    id: &'static str,
    label: &'static str,
    value: f32,
    mid: f32,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let Some(spec) = blocks::trim_spec(id, label) else {
        return div().into_any_element();
    };
    const WIDTH: f32 = 60.0;
    let dial = Dial::new(spec.id, label, spec.min, spec.max, 0.0, DialUnit::Db).bipolar();
    let look =
        KnobLook::small(40.0).styled(3.8, 14.0, plate::FACE, plate::FACE_EDGE, 1.3, 5.5, 2.2);
    let cell = KnobCell::new(WIDTH, 10.0, 11.0).gap(2.0).label(0.08, FAINT);
    div()
        .absolute()
        .left(px((mid - WIDTH / 2.0) * s))
        .top(px(236.0 * s))
        .child(knob_typed(
            cx,
            format!("rodhareist-level-{id}"),
            dial,
            value,
            look,
            cell,
            accent(),
            true,
            s,
            move |this: &mut RodhareistEditorWindow, v, cx| this.set_wire_value(id, v, cx),
            move |this, window, cx| {
                this.begin_knob_entry(id, label, spec.min, spec.max, window, cx)
            },
        ))
        .into_any_element()
}

fn chain_block(
    w: &RodhareistEditorWindow,
    p: &Params,
    slot: usize,
    row: u8,
    selected: bool,
    (left, top): (f32, f32),
    size: f32,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    // Narrower blocks keep their type legible and shrink the rest.
    let k = (size / BLOCK_REF).clamp(0.55, 1.0);
    let type_k = k.max(0.72);
    let entry = p.stage_order[slot];
    let names = w.ui.show_names;
    // A block on one side only says which.
    let side = entry
        .and_then(|kind| blocks::channel(p, kind))
        .filter(|channel| *channel != StageChannel::Both)
        .map(blocks::channel_label);
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
            "Open Explore",
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
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(3.0 * k * s))
                        .children(side.map(|label| {
                            div()
                                .px(px(2.0 * s))
                                .rounded(px(2.0 * s))
                                .bg(color(0x111111))
                                .text_size(type_px(8.5 * type_k, s))
                                .line_height(px(10.0 * type_k * s))
                                .font_weight(FontWeight::BOLD)
                                .text_color(color(WHITE))
                                .child(label)
                        }))
                        .child(div().size(px(8.0 * k * s)).rounded_full().bg(led)),
                ),
        )
        .children((size >= 48.0).then(|| {
            div().flex().justify_center().child(icon(
                icon_path,
                (24.0, 24.0),
                (22.0 * k, 22.0 * k),
                1.6,
                fg,
                s,
            ))
        }))
        .child(
            div()
                .h(px(14.0 * s))
                .min_w(px(0.0))
                .truncate()
                .text_size(type_px(11.5 * type_k, s))
                .line_height(px(13.2 * type_k * s))
                .font_weight(FontWeight::SEMIBOLD)
                .child(match entry {
                    Some(kind) if !blocks::is_on(p, kind) => "Bypassed",
                    Some(_) if names => name,
                    None => "Open Explore",
                    Some(_) => "",
                }),
        );
    if dashed {
        block = block.border_dashed();
    }
    if let Some(kind) = entry {
        let entity = cx.entity().clone();
        block = block.on_drag(ChainDrag { slot, kind }, move |drag, _, _, cx| {
            // The drag opens the next lane as a drop target. The render
            // that follows sees it; the flag clears once the drag ends.
            let _ = entity.update(cx, |this, cx| {
                this.ui.drag_block = Some(drag.kind);
                cx.notify();
            });
            cx.new(|_| ChainGhost {
                name: blocks::block_name(drag.kind),
            })
        });
    }
    let block = block
        .on_drop(drop_chain(cx, slot, row))
        .tooltip(fb_tooltip(match entry {
            Some(kind) => {
                let row = p.stage_row[kind as usize] as usize;
                let wet = lane_controls(p, row).wet;
                format!(
                    "Slot {} \u{00B7} {}{} \u{2014} double-click to bypass",
                    slot + 1,
                    blocks::block_name(kind),
                    if wet { " \u{00B7} wet path" } else { "" }
                )
            }
            None => format!(
                "Slot {} \u{00B7} empty \u{00B7} open Explore or Pedalboard for this slot",
                slot + 1
            ),
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
    let ring = if size < 64.0 { 3.0 } else { 4.0 };
    if selected {
        // The design's ring: a dark gap, then the accent.
        div()
            .absolute()
            .left(px((left - ring) * s))
            .top(px((top - ring) * s))
            .size(px((size + ring * 2.0) * s))
            .p(px((ring - 2.0).max(1.0) * s))
            .rounded(px(4.0 * s))
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
            let side = match blocks::channel(p, kind) {
                Some(StageChannel::Left) => " \u{00B7} left only",
                Some(StageChannel::Right) => " \u{00B7} right only",
                _ => "",
            };
            let row = p.stage_row[kind as usize] as usize;
            let lane = lane_controls(p, row);
            let row_note = format!(" \u{00B7} {}", lane.detail);
            format!(
                "Slot {} \u{00B7} {} \u{00B7} {}{}{}{}",
                slot + 1,
                family.label(),
                blocks::model_name(p, kind).unwrap_or("No amp"),
                side,
                row_note,
                if blocks::is_on(p, kind) {
                    ""
                } else {
                    " (bypassed)"
                }
            )
        }
        ChainFocus::Empty(_) => format!(
            "Slot {} \u{00B7} empty \u{00B7} open Explore or Pedalboard for this slot",
            slot + 1
        ),
    }
}

/// How a lane meets the main chain, for the plate at the end of the lane.
struct LaneControls {
    /// 1-based lane number.
    number: usize,
    /// Wet/dry/wet is the mode the lane is actually in.
    wet: bool,
    /// Short mode word for the chain caption.
    short: &'static str,
    /// Which lane, and what the blocks on it do.
    detail: String,
    choices: Vec<(&'static str, bool, f32)>,
    mode_id: &'static str,
    send_id: &'static str,
    send: f32,
    /// The send is in the sound. A stored amount that this mode ignores stays visible and grey.
    send_live: bool,
    send_default: f32,
}

fn lane_controls(p: &Params, row: usize) -> LaneControls {
    let row = row.min(3);
    let number = row + 1;
    if row == 0 {
        let wet = p.line2_mode == Line2Mode::WetDryWet;
        let (short, detail) = match p.line2_mode {
            Line2Mode::Wet => ("main", format!("Lane {number} · main path")),
            Line2Mode::Dry => ("dry", format!("Lane {number} · dry before time effects")),
            Line2Mode::WetDryWet => (
                "wet path",
                format!("Lane {number} · this lane is the wet path"),
            ),
        };
        LaneControls {
            number,
            wet,
            short,
            detail,
            choices: vec![
                ("Main", p.line2_mode == Line2Mode::Wet, 0.0),
                ("Dry", p.line2_mode == Line2Mode::Dry, 1.0),
                ("Wet path", wet, 2.0),
            ],
            mode_id: "line2_mode",
            send_id: "line2_mix",
            send: p.line2_mix,
            send_live: wet,
            send_default: 70.0,
        }
    } else {
        let current = p.row_send[row - 1];
        let wet = current == RowSend::WetDryWet;
        let (short, detail) = match current {
            RowSend::Off => ("off", format!("Lane {number} · off")),
            RowSend::Serial => (
                "replaces",
                format!("Lane {number} · this lane replaces the chain"),
            ),
            RowSend::Parallel => ("added", format!("Lane {number} · this lane is added")),
            RowSend::WetDryWet => (
                "wet path",
                format!("Lane {number} · this lane is the wet path"),
            ),
        };
        LaneControls {
            number,
            wet,
            short,
            detail,
            choices: vec![
                ("Off", current == RowSend::Off, 0.0),
                ("Replaces", current == RowSend::Serial, 1.0),
                ("Added", current == RowSend::Parallel, 2.0),
                ("Wet path", wet, 3.0),
            ],
            mode_id: ["row_send_1", "row_send_2", "row_send_3"][row - 1],
            send_id: ["row_level_1", "row_level_2", "row_level_3"][row - 1],
            send: p.row_level[row - 1],
            send_live: current != RowSend::Off,
            send_default: 100.0,
        }
    }
}

/// The lane's split, at its end. Lane 1 is the main path. Lanes 2–4 are
/// sends. Wet/dry/wet names the lane and says the blocks on it are the wet
/// path; the send knob is the amount, live only while that mode uses it.
fn lane_send(p: &Params, row: usize, lane_h: f32, cx: &mut Cx, s: f32) -> AnyElement {
    let info = lane_controls(p, row);
    let top = LANE_TOP + row as f32 * lane_h;
    // With only the main lane open, say how to get another.
    let hint = (row == 0 && lane_count(p) == 1)
        .then(|| words("Drag a block down for a new lane", 11.0, FAINT, s).mt(px(4.0 * s)));
    let mode_id = info.mode_id;
    let send_id = info.send_id;
    let pills = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(3.0 * s))
        .child(
            numbers(
                format!("{}", info.number),
                11.0,
                if info.wet { ACCENT } else { SOFT },
                s,
            )
            .w(px(12.0 * s)),
        )
        .children(info.choices.iter().copied().map(|(label, on, value)| {
            plate::pill(
                ("rodhareist-send", row * 8 + value as usize),
                label,
                on,
                accent(),
                plate::PillLook::words(16.0, 3.0, 10.5),
                true,
                s,
                act(cx, move |this, cx| this.set_wire_value(mode_id, value, cx)),
            )
        }));
    let amount = if row == 0 { "Blend" } else { "Level" };
    let dial = Dial::new(
        send_id,
        amount,
        0.0,
        100.0,
        info.send_default,
        DialUnit::Percent,
    );
    let look = KnobLook::small(20.0);
    let cell = KnobCell::new(44.0, 9.0, 10.0).gap(0.0).label(0.06, SOFT);
    let send = knob_typed(
        cx,
        format!("rodhareist-lane-send-{row}"),
        dial,
        info.send,
        look,
        cell,
        accent(),
        info.send_live,
        s,
        move |this: &mut RodhareistEditorWindow, v, cx| this.set_wire_value(send_id, v, cx),
        move |this, window, cx| this.begin_knob_entry(send_id, amount, 0.0, 100.0, window, cx),
    );
    div()
        .id(("rodhareist-lane", row))
        .absolute()
        .left(px(SEND_LEFT * s))
        .top(px(top * s))
        .w(px(190.0 * s))
        .h(px(lane_h * s))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(1.0 * s))
        .overflow_hidden()
        .tooltip(fb_tooltip(format!(
            "{}. Drag {amount} to set the amount. Double-click types it. Alt-click resets it.",
            info.detail
        )))
        .child(pills)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0 * s))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(type_px(11.0, s))
                        .line_height(px(13.0 * s))
                        .text_color(color(if info.wet { WHITE } else { PALE }))
                        .child(info.detail),
                )
                .child(send),
        )
        .children(hint)
        .into_any_element()
}

/// The end of a lane a drag has opened: what dropping there does.
fn opening_lane(row: usize, lane_h: f32, s: f32) -> AnyElement {
    div()
        .absolute()
        .left(px(SEND_LEFT * s))
        .top(px((LANE_TOP + row as f32 * lane_h) * s))
        .w(px(190.0 * s))
        .h(px(lane_h * s))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(2.0 * s))
        .child(numbers(
            format!("{} \u{00B7} NEW LANE", row + 1),
            11.0,
            ACCENT,
            s,
        ))
        .child(words("Drop a block here", 12.0, WHITE, s))
        .child(words("This lane is added", 11.0, SOFT, s))
        .into_any_element()
}

/// An empty column on a lane that is not showing the slot's block. A drop
/// here moves the block onto this lane without packing the path. While a
/// block is in the air the cells show, and the one under it lights.
fn drop_cell(
    slot: usize,
    row: u8,
    (left, top): (f32, f32),
    size: f32,
    dragging: bool,
    cx: &Cx,
    s: f32,
) -> AnyElement {
    let mut cell = div()
        .id(("rodhareist-drop", row as usize * 16 + slot))
        .absolute()
        .left(px(left * s))
        .top(px(top * s))
        .size(px(size * s))
        .rounded(px(2.0 * s))
        .border(px(1.0))
        .border_color(color_a(0, 0.0))
        .hover(|style| style.border_color(color(0x5a5a5a)));
    if dragging {
        cell = cell
            .border_dashed()
            .border_color(color(0x3c3c3c))
            .drag_over::<ChainDrag>(|style, _, _, _| {
                style.border_color(accent()).bg(color_a(ACCENT, 0.14))
            });
    }
    cell.on_drop(drop_chain(cx, slot, row)).into_any_element()
}

fn key_control(p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
    let semi = p.transpose_semi;
    let cents = p.transpose_cents;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0 * s))
        .h(px(28.0 * s))
        .px(px(6.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(color(plate::WELL_EDGE))
        .bg(color(plate::GROUND))
        .child(transpose_group(
            "key",
            "Key",
            "transpose_semi",
            semi,
            (-12.0, 12.0),
            signed_whole(semi, ""),
            "Semitones. Click the value to reset.",
            cx,
            s,
        ))
        .child(
            div()
                .w(px(1.0))
                .h(px(16.0 * s))
                .mx(px(6.0 * s))
                .bg(color(plate::WELL_EDGE)),
        )
        .child(transpose_group(
            "fine",
            "Fine",
            "transpose_cents",
            cents,
            (-100.0, 100.0),
            signed_whole(cents, "\u{00A2}"),
            "Cents. Click the value to reset.",
            cx,
            s,
        ))
        .into_any_element()
}

/// A whole number with its sign, and a plain 0 at zero: `+3`, `−12`, `0`.
fn signed_whole(value: f32, unit: &str) -> String {
    let whole = value.round();
    if whole == 0.0 {
        format!("0{unit}")
    } else if whole > 0.0 {
        format!("+{whole:.0}{unit}")
    } else {
        format!("\u{2212}{:.0}{unit}", -whole)
    }
}

/// One transpose stepper: its caption, −, the value (bright when it moves the
/// pitch, a click puts it back to zero), and +.
#[allow(clippy::too_many_arguments)]
fn transpose_group(
    key: &'static str,
    label: &'static str,
    wire: &'static str,
    value: f32,
    (min, max): (f32, f32),
    text: String,
    help: &'static str,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let step = |delta: f32, cx: &mut Cx| {
        let next = (value + delta).clamp(min, max);
        act(cx, move |this, cx| this.set_wire_value(wire, next, cx))
    };
    let moved = value.abs() > 1.0e-3;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0 * s))
        .child(caption(label, 10.0, 0.1, FAINT, s).mr(px(4.0 * s)))
        .child(key_step(
            ("rodhareist-transpose-down", key),
            "\u{2212}",
            value > min,
            s,
            step(-1.0, cx),
        ))
        .child(
            div()
                .id(gpui::ElementId::Name(
                    format!("rodhareist-transpose-value-{key}").into(),
                ))
                .flex()
                .items_center()
                .justify_center()
                .w(px(38.0 * s))
                .h(px(22.0 * s))
                .rounded(px(3.0 * s))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(color(0x2a2a2a)))
                .tooltip(fb_tooltip(help))
                .child(numbers(text, 13.5, if moved { WHITE } else { FAINT }, s))
                .on_click(act(cx, move |this, cx| this.set_wire_value(wire, 0.0, cx))),
        )
        .child(key_step(
            ("rodhareist-transpose-up", key),
            "+",
            value < max,
            s,
            step(1.0, cx),
        ))
        .into_any_element()
}

fn key_step(
    id: (&'static str, &'static str),
    label: &'static str,
    enabled: bool,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let button = div()
        .id(gpui::ElementId::Name(format!("{}-{}", id.0, id.1).into()))
        .flex()
        .items_center()
        .justify_center()
        .size(px(22.0 * s))
        .rounded(px(3.0 * s))
        .text_size(type_px(14.0, s))
        .text_color(color(if enabled { INK } else { 0x5a5a5a }))
        .child(label);
    if enabled {
        button
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x2e2e2e)))
            .on_click(on_click)
            .into_any_element()
    } else {
        button.into_any_element()
    }
}
fn display(
    w: &RodhareistEditorWindow,
    p: &Params,
    focus: ChainFocus,
    frame: &ChainFrame,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let look = &frame.look;
    let geom = look.geom();
    let shown = look.shown;
    let lanes = look.lanes;
    let used = lane_count(p);
    let dragging = w.ui.drag_block.is_some();
    let selected_slot = focus_slot(&p.stage_order, focus);
    let mut wires = Vec::with_capacity(lanes);
    let mut sends = Vec::with_capacity(lanes);
    let mut cells: Vec<AnyElement> = Vec::new();
    for row in 0..lanes {
        let alpha = frame.lane_alpha(row);
        let mid = LANE_TOP + row as f32 * geom.lane_h + geom.lane_h * 0.5;
        // A lane past the ones in use is the one a drag opened.
        let opening = row >= used;
        let wet_lane = !opening && lane_controls(p, row).wet;
        wires.push(
            div()
                .absolute()
                .left(px(CHAIN_LEFT * s))
                .top(px((mid - 1.0) * s))
                .w(px(BLOCK_RUN * s * if opening { alpha } else { 1.0 }))
                .h(px(2.0 * s))
                .opacity(alpha)
                .bg(if wet_lane {
                    accent()
                } else if opening {
                    color(0x4a4a4a)
                } else {
                    color(0xb5b5b5)
                })
                .into_any_element(),
        );
        let send = if opening {
            opening_lane(row, geom.lane_h, s)
        } else {
            lane_send(p, row, geom.lane_h, cx, s)
        };
        sends.push(div().opacity(alpha).child(send).into_any_element());
        for slot in 0..shown {
            let here = match p.stage_order[slot] {
                Some(kind) => p.stage_row[kind as usize] as usize == row,
                None => row == 0,
            };
            if here && p.stage_order[slot].is_none() {
                cells.push(chain_block(
                    w,
                    p,
                    slot,
                    0,
                    slot == selected_slot,
                    slot_origin(&geom, slot, 0),
                    geom.size,
                    cx,
                    s,
                ));
            } else if !here {
                cells.push(drop_cell(
                    slot,
                    row as u8,
                    slot_origin(&geom, slot, row),
                    geom.size,
                    dragging,
                    cx,
                    s,
                ));
            }
        }
    }
    // Blocks are drawn where the frame has them, so a move between lanes
    // or places glides; a block changing lane swells a little on the way.
    for &(kind, row, left, top) in &look.blocks {
        let Some(slot) = p.stage_order.iter().position(|e| *e == Some(kind)) else {
            continue;
        };
        let swell = if frame.changed_lane.contains(&kind) {
            1.0 + 0.12 * (frame.t * std::f32::consts::PI).sin()
        } else {
            1.0
        };
        let size = geom.size * swell;
        let grow = (size - geom.size) * 0.5;
        cells.push(chain_block(
            w,
            p,
            slot,
            row,
            slot == selected_slot,
            (left - grow, top - grow),
            size,
            cx,
            s,
        ));
    }
    let input = level_knob("input_trim", "IN", p.input_trim_db, 31.0, cx, s);
    let output = level_knob("output_trim", "OUT", p.output_trim_db, 964.0, cx, s);
    div()
        .absolute()
        .left(px(6.0 * s))
        .top(px(7.0 * s))
        .w(px(1011.0 * s))
        .h(px(327.0 * s))
        .overflow_hidden()
        .bg(color(plate::WELL))
        .border(px(1.0))
        .border_color(color(plate::WELL_EDGE))
        .child(header(w, p, cx, s))
        .children(meters(w, s))
        .child(input)
        .child(output)
        .children(wires)
        .children(cells)
        .children(sends)
        .child(
            div()
                .absolute()
                .left(px(CHAIN_LEFT * s))
                .top(px(CAPTION_TOP * s))
                .w(px(BLOCK_RUN * s))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0 * s))
                .child(words(caption_line(p, focus), 13.0, WHITE, s))
                .children((!w.ui.show_names).then(|| {
                    div()
                        .id("rodhareist-show-names")
                        .px(px(6.0 * s))
                        .py(px(2.0 * s))
                        .rounded(px(2.0 * s))
                        .border(px(1.0))
                        .border_color(color(0x484848))
                        .text_size(type_px(11.0, s))
                        .text_color(color(WHITE))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .child("Show names")
                        .on_click(act(cx, |this, cx| {
                            this.ui.show_names = true;
                            cx.notify();
                        }))
                })),
        )
        .into_any_element()
}

// ── Home ───────────────────────────────────────────────────────────────────

fn bank_row(
    id: (&'static str, usize),
    mark: &str,
    name: &str,
    on: bool,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (bg, border, _) = choice_colors(on, true);
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(8.0 * s))
        .h(px(32.0 * s))
        .px(px(8.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(color(0x2e2e2e)))
        .child(numbers(mark.to_string(), 12.0, if on { ACCENT } else { SOFT }, s).w(px(30.0 * s)))
        .child(words(name.to_string(), 13.0, WHITE, s).truncate())
        .on_click(on_click)
        .into_any_element()
}

fn preset_button(
    id: &'static str,
    label: &'static str,
    tip: &'static str,
    s: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .h(px(32.0 * s))
        .min_w(px(0.0))
        .px(px(6.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(0x454545))
        .bg(color(0x2a2a2a))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(color(0x2e2e2e)))
        .child(words(label, 12.5, WHITE, s))
        .tooltip(fb_tooltip(tip))
        .on_click(on_click)
        .into_any_element()
}

/// The window's preset name field, in its row cell. Enter saves a `.pst`.
fn preset_name_field(field: AnyElement, s: f32) -> AnyElement {
    div()
        .id("rodhareist-preset-name-cell")
        .flex()
        .items_center()
        .flex_1()
        .min_w(px(0.0))
        .h(px(32.0 * s))
        .tooltip(fb_tooltip("Name for the user preset. Enter saves a .pst."))
        .child(field)
        .into_any_element()
}

/// Where the cabinet stands, so a file name is not mistaken for a sounding IR.
fn ir_path_line(p: &Params) -> String {
    let on_path = p
        .stage_order
        .iter()
        .flatten()
        .any(|kind| *kind == StageKind::Cab);
    if !on_path {
        return "Cabinet is not on the path".to_string();
    }
    if !p.cab_on {
        return "Cabinet is bypassed".to_string();
    }
    if p.cab_model == CabModel::Ir {
        "Cabinet is the impulse response".to_string()
    } else {
        format!("Cabinet is {}", blocks::cab_model_label(p.cab_model))
    }
}

fn ir_loaded_label(w: &RodhareistEditorWindow) -> Option<String> {
    w.ir_loaded_info().map(|(name, secs)| {
        if secs > 0.0 {
            format!("{name} ({secs:.2} s)")
        } else {
            name
        }
    })
}

/// A project file the host has not accepted. Absent once the IR is really loaded.
fn ir_unconfirmed_note(w: &RodhareistEditorWindow) -> Option<String> {
    if w.ir_loaded_info().is_some() || w.ir_loading() || w.ir_error().is_some() {
        return None;
    }
    w.ir_project_name()
        .map(|name| format!("{name} is named, not loaded"))
}

/// Sidebar text that wraps at the sidebar's width. Plate `words` never wraps,
/// so a long status ran out of the 262 px sidebar into the deck beside it
/// (#404).
fn sidebar_text(text: impl Into<gpui::SharedString>, size: f32, ink: u32, s: f32) -> gpui::Div {
    words(text, size, ink, s)
        .whitespace_normal()
        .w_full()
        .min_w(px(0.0))
}

fn load_status(w: &RodhareistEditorWindow, p: &Params, s: f32) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w_full()
        .gap(px(3.0 * s))
        .child(sidebar_text(ir_status_line(w, p), 12.0, PALE, s))
        .child(sidebar_text(nam_status_line(w, p), 12.0, PALE, s))
        .into_any_element()
}

fn ir_status_line(w: &RodhareistEditorWindow, p: &Params) -> String {
    let unused = if !p
        .stage_order
        .iter()
        .flatten()
        .any(|kind| *kind == StageKind::Cab)
    {
        Some("unused: the cabinet is not on the path")
    } else if !p.cab_on {
        Some("unused: the cabinet is bypassed")
    } else if p.cab_model != CabModel::Ir {
        Some("unused: the cabinet is not an impulse response")
    } else {
        None
    };
    let file = if let Some((name, _)) = w.ir_loaded_info() {
        format!("IR loaded: {name}")
    } else if let Some(name) = w.ir_project_name() {
        format!("{name} is named, not loaded")
    } else {
        "IR: no file loaded".to_string()
    };
    match unused {
        Some(why) => format!("IR {why}. {file}"),
        None => file,
    }
}

fn nam_status_line(w: &RodhareistEditorWindow, p: &Params) -> String {
    let unused = if !p
        .stage_order
        .iter()
        .flatten()
        .any(|kind| *kind == StageKind::Amp)
    {
        Some("unused: the amp is not on the path")
    } else if !p.amp_on {
        Some("unused: the amp is bypassed")
    } else if p.tone_engine != ToneEngineKind::NamCapture {
        Some("unused: the amp is not a capture")
    } else {
        None
    };
    let file = if let Some(name) = w.nam_loaded_info() {
        format!("NAM loaded: {name}")
    } else if let Some(name) = w.nam_project_name() {
        format!("{name} is named, not loaded")
    } else {
        "NAM: no capture loaded".to_string()
    };
    match unused {
        Some(why) => format!("NAM {why}. {file}"),
        None => file,
    }
}

fn ir_block(w: &RodhareistEditorWindow, p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
    let mut block = div()
        .flex()
        .flex_col()
        .gap(px(4.0 * s))
        .pt(px(4.0 * s))
        .child(heading("Impulse response", s))
        .child(load_row(
            "rodhareist-home-ir",
            "Load .wav",
            ir_loaded_label(w),
            w.ir_loading(),
            w.ir_error(),
            w.load_ir_cb(cx),
            110.0,
            s,
        ))
        .child(sidebar_text(ir_path_line(p), 12.0, PALE, s));
    if let Some(note) = ir_unconfirmed_note(w) {
        block = block.child(sidebar_text(note, 12.0, ACCENT, s));
    }
    block.into_any_element()
}

/// A row of subtabs in the preset switch's style: `(value, label, count)`
/// per tab, the one equal to `current` lit. `pick` stores a click in the
/// editor's view state. Counts are left out where the row is narrow.
#[allow(clippy::too_many_arguments)]
fn subtabs(
    id: &'static str,
    tabs: Vec<(Option<Family>, &'static str, usize)>,
    current: Option<Family>,
    counts: bool,
    cx: &mut Cx,
    s: f32,
    pick: fn(&mut RodUi, Option<Family>),
) -> AnyElement {
    let tab =
        |index: usize, value: Option<Family>, label: &'static str, count: usize, cx: &mut Cx| {
            let on = value == current;
            let mut cell = div()
                .id((id, index))
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(5.0 * s))
                .h_full()
                .px(px(4.0 * s))
                .rounded(px(2.0 * s))
                .bg(if on { color(0x343434) } else { color_a(0, 0.0) })
                .border_b(px(2.0 * s))
                .border_color(if on { accent() } else { color_a(0, 0.0) })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(color(0x2e2e2e)))
                .child(
                    words(label, 12.0, if on { WHITE } else { SOFT }, s)
                        .font_weight(FontWeight::MEDIUM),
                );
            if counts {
                cell = cell.child(numbers(
                    count.to_string(),
                    10.5,
                    if on { ACCENT } else { FAINT },
                    s,
                ));
            }
            cell.on_click(act(cx, move |this, cx| {
                pick(&mut this.ui, value);
                cx.notify();
            }))
        };
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .h(px(30.0 * s))
        .p(px(2.0 * s))
        .gap(px(2.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(0x454545))
        .bg(color(0x1f1f1f))
        .children(
            tabs.into_iter()
                .enumerate()
                .map(|(index, (value, label, count))| tab(index, value, label, count, cx)),
        )
        .into_any_element()
}

/// The sidebar's two preset lists, as one segmented switch: the factory
/// bank and the `.pst` files the player saved, each with its count.
fn preset_tabs(
    current: PresetTab,
    factory: usize,
    users: usize,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let tab = |id: usize, item: PresetTab, label: &'static str, count: usize, cx: &mut Cx| {
        let on = item == current;
        div()
            .id(("rodhareist-preset-tab", id))
            .flex_1()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(6.0 * s))
            .h_full()
            .rounded(px(2.0 * s))
            .bg(if on { color(0x343434) } else { color_a(0, 0.0) })
            .border_b(px(2.0 * s))
            .border_color(if on { accent() } else { color_a(0, 0.0) })
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x2e2e2e)))
            .child(
                words(label, 13.0, if on { WHITE } else { SOFT }, s)
                    .font_weight(FontWeight::MEDIUM),
            )
            .child(numbers(
                count.to_string(),
                11.0,
                if on { ACCENT } else { FAINT },
                s,
            ))
            .on_click(act_in(cx, move |this, window, cx| {
                this.ui.preset_tab = item;
                this.release_text_focus(window, cx);
                if item == PresetTab::Users {
                    this.refresh_user_presets();
                }
                cx.notify();
            }))
    };
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .h(px(32.0 * s))
        .p(px(2.0 * s))
        .gap(px(2.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(0x454545))
        .bg(color(0x1f1f1f))
        .child(tab(0, PresetTab::Factory, "Factory", factory, cx))
        .child(tab(1, PresetTab::Users, "Users", users, cx))
        .into_any_element()
}

fn home(
    w: &RodhareistEditorWindow,
    p: &Params,
    name_field: AnyElement,
    cx: &mut Cx,
    s: f32,
) -> Vec<AnyElement> {
    let current = w.preset_status();
    let presets = rodharerist::factory_presets();
    let rows: Vec<AnyElement> = presets
        .iter()
        .enumerate()
        .map(|(index, preset)| {
            let on = current.map(|(i, _)| i) == Some(index);
            bank_row(
                ("rodhareist-preset", index),
                preset.id,
                preset.name,
                on,
                s,
                w.load_preset_cb(cx, index),
            )
        })
        .collect();
    let loaded_user = w.user_preset_status().map(|(name, _)| name.to_string());
    let user_rows: Vec<AnyElement> = w
        .user_presets()
        .iter()
        .enumerate()
        .map(|(index, (name, _))| {
            let on = loaded_user.as_deref() == Some(name.as_str());
            bank_row(
                ("rodhareist-user-preset", index),
                "PST",
                name,
                on,
                s,
                w.load_user_preset_cb(cx, index),
            )
        })
        .collect();
    let user_count = user_rows.len();
    let tab = w.ui.preset_tab;
    let note = w.preset_note().unwrap_or("").to_string();
    let has_note = !note.is_empty();
    let mut list = section(s, 6.0, 262.0)
        .p(px(12.0 * s))
        .gap(px(6.0 * s))
        .child(heading("Presets", s))
        .child(load_status(w, p, s))
        .child(preset_tabs(tab, presets.len(), user_count, cx, s));
    list = match tab {
        PresetTab::Factory => list.child(
            div()
                .id("rodhareist-factory-presets")
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .gap(px(4.0 * s))
                .overflow_y_scroll()
                .children(rows),
        ),
        PresetTab::Users => {
            let user_list = if user_rows.is_empty() {
                vec![
                    words("No saved presets yet.", 12.5, SOFT, s).into_any_element(),
                    words("Save the rig, or import a .pst.", 12.0, FAINT, s).into_any_element(),
                ]
            } else {
                user_rows
            };
            list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0 * s))
                    .child(preset_name_field(name_field, s))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.0 * s))
                            .child(preset_button(
                                "rodhareist-save-preset",
                                "Save",
                                "Writes a .pst under the name in the field",
                                s,
                                w.save_user_preset_cb(cx),
                            ))
                            .child(preset_button(
                                "rodhareist-rename-preset",
                                "Rename",
                                "Renames the loaded user preset to the name in the field",
                                s,
                                w.rename_user_preset_cb(cx),
                            ))
                            .child(preset_button(
                                "rodhareist-import-preset",
                                "Import",
                                "Copies a .pst into your presets and loads it",
                                s,
                                w.import_user_preset_cb(cx),
                            )),
                    ),
            )
            .child(
                div()
                    .id("rodhareist-user-presets")
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0 * s))
                    .overflow_y_scroll()
                    .children(user_list),
            )
        }
    };
    if has_note {
        list = list.child(sidebar_text(note, 12.0, ACCENT, s));
    }
    let list = list.child(ir_block(w, p, cx, s));

    let (kicker, title) = if let Some((user_name, edited)) = w.user_preset_status() {
        (
            format!("USER{}", if edited { " · EDITED" } else { "" }),
            user_name.to_string(),
        )
    } else {
        match current.and_then(|(i, edited)| presets.get(i).map(|pr| (pr, edited))) {
            Some((preset, edited)) => (
                format!(
                    "{} · BANK {}{}",
                    preset.id,
                    preset.id.get(..2).unwrap_or(""),
                    if edited { " · EDITED" } else { "" }
                ),
                preset.name.to_string(),
            ),
            None => ("NO PRESET LOADED".to_string(), "Custom".to_string()),
        }
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
        .map(|(i, slot)| footswitch(w, p, i, slot, cx, s))
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
                .child(numbers(kicker, 12.0, ACCENT, s))
                .child(
                    words(title, 38.0, WHITE, s)
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
                .child(tuner_box(w, p, cx, s))
                .child(tempo_box(w, p, cx, s))
                .child(line2_box(p, cx, s))
                .child(master_box(p, cx, s)),
        );
    vec![list.into_any_element(), performance.into_any_element()]
}

fn footswitch(
    w: &RodhareistEditorWindow,
    p: &Params,
    i: usize,
    slot: Option<usize>,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
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
    let pinned = w.ui.footswitches[i].is_some();
    let session = if pinned { " \u{00B7} this session" } else { "" };
    let sub = match (slot, family) {
        (Some(slot), Some(family)) => {
            format!("SLOT {} \u{00B7} {}{session}", slot + 1, family.tag())
        }
        (Some(slot), None) => format!("SLOT {slot}{session}", slot = slot + 1),
        (None, _) => "Assign in Settings".to_string(),
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

fn tuner_box(w: &RodhareistEditorWindow, p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
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
        .id("rodhareist-home-tuner")
        .flex_1()
        .flex()
        .flex_col()
        .justify_between()
        .px(px(14.0 * s))
        .py(px(10.0 * s))
        .opacity(if on { 1.0 } else { 0.45 })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.opacity(1.0))
        .tooltip(fb_tooltip("Opens the tuner"))
        .on_click(act(cx, |this, cx| this.toggle_wire("tuner_on", cx)))
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

/// In-tune green, as the Home tile uses it.
const TUNED: u32 = 0x9be15d;
/// The tuner page's scale and strobe width, at the design size.
const TUNER_SCALE_W: f32 = 700.0;
/// One strobe stripe period, at the design size.
const STROBE_PERIOD: f32 = 28.0;

/// The tuner, in the deck while `tuner_on` is set: the note, a cents needle,
/// a strobe band and the signal's clarity, all from the DSP's tuner reading.
/// The rig keeps playing (the DSP does not mute while tuning).
fn tuner_page(w: &RodhareistEditorWindow, cx: &mut Cx, s: f32) -> Vec<AnyElement> {
    use rodharerist::telemetry as tm;
    let look = &w.ui.tuner;
    let now = Instant::now();
    let hz = w.pads().map_or(0.0, |block| block[tm::TUNER_HZ]);
    let live = hz > 0.0;
    let clarity = w
        .pads()
        .map_or(0.0, |block| block[tm::TUNER_CLARITY])
        .clamp(0.0, 1.0);
    let in_tune = look.note.is_some() && look.in_tune_since.is_some();
    let ink = match look.note {
        None => FAINT,
        Some(_) if in_tune => TUNED,
        Some(_) => WHITE,
    };
    let (note, octave, status) = match look.note {
        None => (
            "\u{2014}".to_string(),
            String::new(),
            "Play one string".to_string(),
        ),
        Some((name, octave, cents)) => (
            name.to_string(),
            octave.to_string(),
            if in_tune {
                "In tune".to_string()
            } else {
                format!(
                    "{}{:.0} cents {}",
                    if cents > 0.0 { "+" } else { "\u{2212}" },
                    cents.abs(),
                    if cents > 0.0 { "sharp" } else { "flat" }
                )
            },
        ),
    };
    // The pulse on coming into tune: a ring that fades over SLOW.
    let pulse = look.in_tune_since.map_or(0.0, |since| {
        let t = now.duration_since(since).as_secs_f32()
            / (crate::theme::motion::SLOW_MS as f32 / 1_000.0);
        (1.0 - t).clamp(0.0, 1.0)
    });

    let note_box = div()
        .relative()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .w(px(230.0 * s))
        .h_full()
        .rounded(px(6.0 * s))
        .bg(color(plate::WELL))
        .border(px((1.0 + 2.0 * pulse) * s))
        .border_color(if in_tune {
            color_a(TUNED, 0.35 + 0.65 * pulse)
        } else {
            color(plate::WELL_EDGE)
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .child(
                    numbers(note, 92.0, ink, s)
                        .font_weight(FontWeight::SEMIBOLD)
                        .line_height(px(100.0 * s)),
                )
                .child(numbers(octave, 26.0, SOFT, s).mt(px(14.0 * s))),
        )
        .child(numbers(status, 15.0, ink, s));

    // The cents scale: a tick every 5 cents, labels every 25, the in-tune
    // window washed green, and the eased needle.
    let ticks = (-10..=10).map(|step: i32| {
        let cents = step as f32 * 5.0;
        let major = step % 2 == 0;
        div()
            .absolute()
            .left(gpui::relative((cents + 50.0) / 100.0))
            .ml(px(-0.5 * s))
            .bottom(px(22.0 * s))
            .w(px(1.0 * s))
            .h(px(if step == 0 {
                54.0
            } else if major {
                30.0
            } else {
                16.0
            } * s))
            .bg(color(if step == 0 { PALE } else { 0x5a5a5a }))
    });
    let labels = [-50, -25, 0, 25, 50].map(|cents: i32| {
        div()
            .absolute()
            .left(gpui::relative((cents as f32 + 50.0) / 100.0))
            .ml(px(-20.0 * s))
            .w(px(40.0 * s))
            .bottom_0()
            .flex()
            .justify_center()
            .child(numbers(
                if cents > 0 {
                    format!("+{cents}")
                } else {
                    cents.to_string()
                },
                11.0,
                FAINT,
                s,
            ))
    });
    let window_l = (50.0 - IN_TUNE_CENTS) / 100.0;
    let needle_ink = match look.note {
        None => 0x5a5a5a,
        Some(_) if in_tune => TUNED,
        Some(_) => ACCENT,
    };
    let needle = div()
        .absolute()
        .left(gpui::relative(
            (look.cents.clamp(-50.0, 50.0) + 50.0) / 100.0,
        ))
        .ml(px(-2.0 * s))
        .top(px(6.0 * s))
        .bottom(px(18.0 * s))
        .w(px(4.0 * s))
        .rounded(px(2.0 * s))
        .bg(color(needle_ink));
    let scale = div()
        .relative()
        .w(px(TUNER_SCALE_W * s))
        .h(px(120.0 * s))
        .child(
            div()
                .absolute()
                .left(gpui::relative(window_l))
                .w(gpui::relative(2.0 * IN_TUNE_CENTS / 100.0))
                .top(px(6.0 * s))
                .bottom(px(22.0 * s))
                .bg(color_a(TUNED, if in_tune { 0.28 } else { 0.1 })),
        )
        .children(ticks)
        .children(labels)
        .child(needle);

    // The strobe: stripes that drift with how far off the note is, and stand
    // still in tune. Drawn only while there is a note to show.
    let stripes = (TUNER_SCALE_W / STROBE_PERIOD) as usize + 2;
    let offset = look.strobe * STROBE_PERIOD;
    let strobe_ink = if in_tune { TUNED } else { 0x8a8a8a };
    let mut strobe = div()
        .relative()
        .w(px(TUNER_SCALE_W * s))
        .h(px(34.0 * s))
        .rounded(px(3.0 * s))
        .overflow_hidden()
        .bg(color(plate::WELL))
        .border(px(1.0))
        .border_color(color(plate::WELL_EDGE));
    if look.note.is_some() {
        strobe = strobe.children((0..stripes).map(|i| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px((i as f32 * STROBE_PERIOD - offset) * s))
                .w(px(STROBE_PERIOD * 0.5 * s))
                .bg(color_a(strobe_ink, 0.55))
        }));
    }

    let readout = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(18.0 * s))
        .child(numbers(
            if live && hz > 0.0 {
                format!("{hz:.1} Hz")
            } else {
                "\u{2014} Hz".to_string()
            },
            13.0,
            INK,
            s,
        ))
        .child(numbers("A4 = 440 Hz", 12.0, FAINT, s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0 * s))
                .child(caption("Signal", 10.0, 0.1, SOFT, s))
                .child(
                    div()
                        .w(px(90.0 * s))
                        .h(px(6.0 * s))
                        .bg(color(0x1a1a1a))
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(clarity))
                                .bg(color(if clarity > 0.6 { TUNED } else { 0x8a8a8a })),
                        ),
                ),
        );

    let done = div()
        .id("rodhareist-tuner-done")
        .flex()
        .items_center()
        .justify_center()
        .h(px(32.0 * s))
        .px(px(22.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(accent())
        .bg(color(0x1f1f1f))
        .text_size(type_px(13.0, s))
        .text_color(color(WHITE))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(color(0x2e2e2e)))
        .child("Done")
        .on_click(act(cx, |this, cx| this.close_tuner_page(cx)));

    let page = section(s, 6.0, 1011.0)
        .px(px(24.0 * s))
        .py(px(18.0 * s))
        .gap(px(16.0 * s))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(12.0 * s))
                        .child(heading("Tuner", s))
                        .child(words(
                            "The rig keeps playing while you tune.",
                            12.0,
                            SOFT,
                            s,
                        )),
                )
                .child(readout),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .gap(px(24.0 * s))
                .child(note_box)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .justify_center()
                        .gap(px(14.0 * s))
                        .child(scale)
                        .child(strobe),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(words(
                    "Esc or Done closes the tuner. Any page tab leaves it too.",
                    11.5,
                    FAINT,
                    s,
                ))
                .child(done),
        );
    vec![page.into_any_element()]
}

fn tempo_box(w: &RodhareistEditorWindow, p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
    // Synced, Delay A follows the host tempo: show that, and say a tap takes
    // the delay back to its own time.
    let synced = p.delay_sync != DelaySync::Free;
    let bpm = if synced {
        w.host_tempo().map(|bpm| bpm as f32).unwrap_or(120.0)
    } else {
        60_000.0 / p.delay_time_ms.max(1.0)
    };
    let (label, tip) = if synced {
        (
            "Delay A \u{b7} Sync",
            "Tap a quarter note: sets Delay A's time and switches it from Sync to Free",
        )
    } else {
        ("Delay A", "Tap a quarter note: sets Delay A's time")
    };
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
                .child(caption(label, 10.5, 0.1, SOFT, s))
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
                .tooltip(fb_tooltip(tip))
                .on_click(act(cx, |this, cx| this.tap_tempo(cx))),
        )
        .into_any_element()
}

fn line2_box(p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
    let info = lane_controls(p, 0);
    let buttons =
        div()
            .flex()
            .flex_row()
            .gap(px(4.0 * s))
            .children(info.choices.iter().copied().map(|(label, on, value)| {
                let (bg, border, _) = choice_colors(on, false);
                div()
                    .id(("rodhareist-line2", value as usize))
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(22.0 * s))
                    .px(px(8.0 * s))
                    .rounded(px(3.0 * s))
                    .border(px(1.0))
                    .border_color(border)
                    .bg(bg)
                    .cursor(gpui::CursorStyle::PointingHand)
                    .child(words(label, 11.0, if on { WHITE } else { PALE }, s))
                    .on_click(act(cx, move |this, cx| {
                        this.set_wire_value("line2_mode", value, cx);
                    }))
                    .into_any_element()
            }));
    let dial = Dial::new("line2_mix", "BLEND", 0.0, 100.0, 70.0, DialUnit::Percent);
    let look =
        KnobLook::small(40.0).styled(4.0, 14.0, plate::FACE, plate::FACE_EDGE, 1.5, 5.0, 2.2);
    let cell = KnobCell::new(64.0, 10.0, 11.0).gap(1.0).label(0.08, SOFT);
    tile(s)
        .w(px(220.0 * s))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0 * s))
        .px(px(8.0 * s))
        .py(px(6.0 * s))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(4.0 * s))
                .child(numbers(
                    "LANE 1",
                    11.0,
                    if info.wet { ACCENT } else { SOFT },
                    s,
                ))
                .child(
                    div()
                        .text_size(type_px(11.0, s))
                        .line_height(px(13.0 * s))
                        .text_color(color(if info.wet { WHITE } else { PALE }))
                        .child(info.detail),
                )
                .child(buttons),
        )
        .child(knob_typed(
            cx,
            "rodhareist-line2-mix",
            dial,
            p.line2_mix,
            look,
            cell,
            accent(),
            info.send_live,
            s,
            |this: &mut RodhareistEditorWindow, v, cx| this.set_wire_value("line2_mix", v, cx),
            |this, window, cx| this.begin_knob_entry("line2_mix", "BLEND", 0.0, 100.0, window, cx),
        ))
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
        .child(knob_typed(
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
            |this, window, cx| {
                this.begin_knob_entry("output_trim", "MASTER", -24.0, 24.0, window, cx)
            },
        ))
        .into_any_element()
}

// ── Explore ────────────────────────────────────────────────────────────────

fn explore(
    w: &RodhareistEditorWindow,
    p: &Params,
    focus: ChainFocus,
    search_field: AnyElement,
    cx: &mut Cx,
    s: f32,
) -> Vec<AnyElement> {
    let all = entries(p);
    let current = w.ui.family;
    let mut rows: Vec<(Option<Family>, &'static str, u32, usize)> =
        vec![(None, "All blocks", INK, all.len())];
    rows.extend(Family::ALL.iter().map(|&family| {
        let count = all.iter().filter(|e| e.browse.parent() == family).count();
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
    let query = w.search.value.trim().to_lowercase();
    // Amp's subtab narrows Amp to one gain class (or the NAM engine).
    let amp_tab = (current == Some(Family::Amp))
        .then_some(w.ui.explore_amp)
        .flatten();
    let amp_tabs = (current == Some(Family::Amp)).then(|| {
        let mut tabs = vec![(
            None,
            "All",
            all.iter()
                .filter(|e| e.browse.parent() == Family::Amp)
                .count(),
        )];
        tabs.extend(Family::AMP_TABS.iter().map(|&(class, label)| {
            (
                Some(class),
                label,
                all.iter()
                    .filter(|e| e.category == Category::Amp && e.browse == class)
                    .count(),
            )
        }));
        subtabs(
            "rodhareist-explore-amp",
            tabs,
            amp_tab,
            true,
            cx,
            s,
            |ui, tab| ui.explore_amp = tab,
        )
    });
    let matches: Vec<Entry> = all
        .into_iter()
        .filter(|e| current.is_none_or(|family| e.browse.parent() == family))
        .filter(|e| amp_tab.is_none_or(|class| e.browse == class))
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

    let search = div()
        .flex()
        .items_center()
        .flex_1()
        .min_w(px(0.0))
        .h(px(30.0 * s))
        .child(search_field);
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
        .children(amp_tabs)
        .child(grid);
    vec![families.into_any_element(), library.into_any_element()]
}

fn library_card(p: &Params, entry: Entry, target: usize, cx: &mut Cx, s: f32) -> AnyElement {
    let family = entry.browse;
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
            section(s, 274.0, 743.0)
                .p(px(18.0 * s))
                .gap(px(8.0 * s))
                .child(heading("This slot", s))
                .child(words(
                    "This slot is empty. Open Explore, or pick a block here, for this slot.",
                    13.0,
                    PALE,
                    s,
                ))
                .into_any_element(),
        ];
    };

    let (list, selected) = blocks::models(p, kind);
    let models = model_column(w, kind, &list, selected, cx, s);
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

fn model_row(
    w: &RodhareistEditorWindow,
    index: usize,
    choice: &blocks::ModelChoice,
    on: bool,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
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
        let wire = choice.wire_id;
        let value = choice.value;
        row = row
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x303030)))
            .on_click(w.set_cb(cx, wire, value));
    }
    row.into_any_element()
}

fn model_column(
    w: &RodhareistEditorWindow,
    kind: StageKind,
    list: &[blocks::ModelChoice],
    selected: Option<usize>,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::new();
    let mut tabs = None;
    if kind == StageKind::Amp {
        let class_of = |choice: &blocks::ModelChoice| amp_browse(choice.wire_id, choice.value);
        // The subtab picked, else the one the selected model is in (#407).
        let shown =
            w.ui.board_amp
                .or_else(|| selected.and_then(|i| list.get(i)).map(class_of))
                .unwrap_or(Family::Clean);
        tabs = Some(subtabs(
            "rodhareist-board-amp",
            Family::AMP_TABS
                .iter()
                .map(|&(class, label)| (Some(class), label, 0))
                .collect(),
            Some(shown),
            false,
            cx,
            s,
            |ui, tab| ui.board_amp = tab,
        ));
        for (index, choice) in list.iter().enumerate() {
            if class_of(choice) == shown {
                rows.push(model_row(w, index, choice, selected == Some(index), cx, s));
            }
        }
    } else {
        for (index, choice) in list.iter().enumerate() {
            rows.push(model_row(w, index, choice, selected == Some(index), cx, s));
        }
    }
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .gap(px(6.0 * s))
        .children(tabs)
        .child(
            div()
                .id("rodhareist-models")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(4.0 * s))
                .children(rows),
        )
        .into_any_element()
}

fn sync_label(sync: DelaySync) -> &'static str {
    match sync {
        DelaySync::Free => "Free",
        DelaySync::Quarter => "1/4",
        DelaySync::DottedEighth => "1/8.",
        DelaySync::Eighth => "1/8",
        DelaySync::Half => "1/2",
        DelaySync::DottedQuarter => "1/4.",
        DelaySync::QuarterTriplet => "1/4t",
        DelaySync::EighthTriplet => "1/8t",
        DelaySync::Sixteenth => "1/16",
        DelaySync::DottedSixteenth => "1/16.",
    }
}

/// The delay time knob a sync choice belongs to: `(time id, sync id, sync,
/// free ms)` for each of the four time knobs.
fn delay_time_sync(
    p: &Params,
    time_id: &str,
) -> Option<(&'static str, &'static str, DelaySync, f32)> {
    Some(match time_id {
        "delay_time" => ("delay_time", "delay_sync", p.delay_sync, p.delay_time_ms),
        "delay_time_r" => (
            "delay_time_r",
            "delay_sync_r",
            p.delay_sync_r,
            p.delay_time_r_ms,
        ),
        "delay2_time" => (
            "delay2_time",
            "delay2_sync",
            p.stage_b.delay_sync,
            p.stage_b.delay_time_ms,
        ),
        "delay2_time_r" => (
            "delay2_time_r",
            "delay2_sync_r",
            p.stage_b.delay_sync_r,
            p.stage_b.delay_time_r_ms,
        ),
        _ => return None,
    })
}

/// The synced divisions, shortest first: the steps the time knob turns
/// through in Sync.
fn sync_steps() -> Vec<DelaySync> {
    let mut steps: Vec<DelaySync> = DelaySync::ALL
        .iter()
        .copied()
        .filter(|sync| sync.beats().is_some())
        .collect();
    steps.sort_by(|a, b| {
        a.beats()
            .partial_cmp(&b.beats())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    steps
}

/// The division nearest `ms` at `tempo`: where Sync starts from a free time,
/// so switching does not jump.
fn nearest_sync(ms: f32, tempo: f32) -> DelaySync {
    sync_steps()
        .into_iter()
        .min_by(|a, b| {
            let da = (rodharerist::effective_delay_ms(*a, ms, tempo) - ms).abs();
            let db = (rodharerist::effective_delay_ms(*b, ms, tempo) - ms).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(DelaySync::Quarter)
}

/// A time knob in Sync: it steps through the divisions, shortest first, and
/// reads the one chosen. Free keeps the millisecond knob ([`block_knob`]).
#[allow(clippy::too_many_arguments)]
fn sync_knob(
    cx: &mut Cx,
    spec: ParamSpec,
    sync_id: &'static str,
    sync: DelaySync,
    ink: Rgba,
    size: f32,
    width: f32,
    s: f32,
) -> AnyElement {
    let steps = sync_steps();
    let last = steps.len().saturating_sub(1);
    let index = steps.iter().position(|step| *step == sync).unwrap_or(0);
    let quarter = steps
        .iter()
        .position(|step| *step == DelaySync::Quarter)
        .unwrap_or(0);
    let dial = Dial::new(
        spec.id,
        spec.label,
        0.0,
        last as f32,
        quarter as f32,
        DialUnit::Plain,
    );
    let look = KnobLook::small(size).styled(3.6, 18.0, 0x1a1a1a, 0x555555, 1.0, 6.0, 2.0);
    let cell = KnobCell::new(width, 11.0, 12.5)
        .gap(3.0)
        .label(0.08, PALE)
        .label_below();
    let element = plate::knob_reading(
        cx,
        format!("rodhareist-knob-{}", spec.id),
        dial,
        index as f32,
        Some(sync_label(sync).to_string()),
        look,
        cell,
        ink,
        true,
        s,
        move |this: &mut RodhareistEditorWindow, v, cx| {
            let step = steps[(v.round().max(0.0) as usize).min(last)];
            this.set_wire_value(sync_id, step as u8 as f32, cx);
        },
    );
    if spec.inactive {
        div().opacity(0.4).child(element).into_any_element()
    } else {
        element
    }
}

/// The delay's time mode: Free (the time knob sets milliseconds) or Sync (it
/// steps through note divisions at the host tempo), for Time and, on the
/// stereo model, Time R. Shows the time that sounds.
fn delay_sync_row(
    w: &RodhareistEditorWindow,
    p: &Params,
    kind: StageKind,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let second = kind == StageKind::Delay2;
    let tempo = w.host_tempo().map(|bpm| bpm as f32).unwrap_or(120.0);
    let stereo = if second {
        p.stage_b.delay_model == rodharerist::DelayModel::Stereo
    } else {
        p.delay_model == rodharerist::DelayModel::Stereo
    };
    let mut sides = vec![if second { "delay2_time" } else { "delay_time" }];
    if stereo {
        sides.push(if second {
            "delay2_time_r"
        } else {
            "delay_time_r"
        });
    }
    let rows = sides.into_iter().enumerate().filter_map(|(tag, time_id)| {
        let (time_id, sync_id, sync, free_ms) = delay_time_sync(p, time_id)?;
        let sounding = rodharerist::effective_delay_ms(sync, free_ms, tempo);
        let synced = sync != DelaySync::Free;
        let to_sync = nearest_sync(sounding, tempo);
        let mode = |index: usize,
                    label: &'static str,
                    on: bool,
                    pick: Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>,
                    cx: &mut Cx| {
            plate::pill(
                ("rodhareist-delay-mode", tag * 2 + index),
                label,
                on,
                accent(),
                plate::PillLook::words(22.0, 10.0, 11.5),
                true,
                s,
                act(cx, move |this, cx| pick(this, cx)),
            )
        };
        let label = if tag == 0 && stereo {
            "Time L"
        } else if tag == 0 {
            "Time"
        } else {
            "Time R"
        };
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0 * s))
                .child(caption(label, 10.5, 0.1, SOFT, s).w(px(52.0 * s)))
                .child(mode(
                    0,
                    "Free",
                    !synced,
                    // Keep what sounds: the synced time becomes the free time.
                    Box::new(move |this, cx| {
                        if synced {
                            this.set_wire_value(time_id, sounding, cx);
                            this.set_wire_value(sync_id, DelaySync::Free as u8 as f32, cx);
                        }
                    }),
                    cx,
                ))
                .child(mode(
                    1,
                    "Sync",
                    synced,
                    // Start from the division nearest the free time.
                    Box::new(move |this, cx| {
                        if !synced {
                            this.set_wire_value(sync_id, to_sync as u8 as f32, cx);
                        }
                    }),
                    cx,
                ))
                .child(
                    numbers(
                        if synced {
                            format!("{} \u{00B7} {sounding:.0} ms", sync_label(sync))
                        } else {
                            format!("{sounding:.0} ms")
                        },
                        12.0,
                        PALE,
                        s,
                    )
                    .ml(px(6.0 * s)),
                ),
        )
    });
    div()
        .flex()
        .flex_col()
        .gap(px(6.0 * s))
        .children(rows)
        .into_any_element()
}
fn block_card(
    w: &RodhareistEditorWindow,
    p: &Params,
    kind: StageKind,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    // A capture is a file, a tone stack and four trims: its own layout.
    if kind == StageKind::Amp && p.tone_engine == ToneEngineKind::NamCapture {
        return nam_card(w, p, cx, s);
    }
    let family = Family::of(Category::of(kind));
    let ink = color(family.color());
    let on = blocks::is_on(p, kind);
    let values: std::collections::HashMap<&'static str, f32> =
        rodharerist::ui_values(p).into_iter().collect();
    let specs = blocks::params(p, kind);
    // More than one row of knobs: smaller knobs in cells wide enough for
    // labels such as "Capture out", six to a row.
    let dense = specs.len() > 6;
    let (size, width, knob_gap) = if dense {
        (52.0, 96.0, 10.0)
    } else {
        (76.0, 96.0, 14.0)
    };
    let knobs: Vec<AnyElement> = specs
        .into_iter()
        .map(|spec| {
            // A delay time in Sync is a division knob; in Free, milliseconds.
            if let Some((_, sync_id, sync, _)) = delay_time_sync(p, spec.id) {
                if sync != DelaySync::Free {
                    return sync_knob(cx, spec, sync_id, sync, ink, size, width, s);
                }
            }
            let value = values.get(spec.id).copied().unwrap_or(spec.min);
            block_knob(cx, spec, value, ink, size, width, s)
        })
        .collect();

    // Which send row the block sits on, the side of the path it runs on,
    // and the cabinet's microphone and IR.
    let mut extras: Vec<AnyElement> = Vec::new();
    if matches!(kind, StageKind::Delay | StageKind::Delay2) {
        extras.push(delay_sync_row(w, p, kind, cx, s));
    }
    extras.push(lane_pills(w, p, kind, cx, s));
    if let (Some(current), Some(id)) = (blocks::channel(p, kind), kind.channel_id()) {
        extras.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0 * s))
                .child(caption("Channel", 10.5, 0.1, SOFT, s).mr(px(4.0 * s)))
                .children(StageChannel::ALL.iter().map(|&channel| {
                    plate::pill(
                        ("rodhareist-channel", channel.index() as usize),
                        blocks::channel_label(channel),
                        current == channel,
                        accent(),
                        plate::PillLook::words(28.0, 12.0, 12.5),
                        true,
                        s,
                        w.set_cb(cx, id, channel.index() as f32),
                    )
                }))
                .into_any_element(),
        );
    }
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
                ir_loaded_label(w),
                w.ir_loading(),
                w.ir_error(),
                w.load_ir_cb(cx),
                360.0,
                s,
            ));
            if let Some(note) = ir_unconfirmed_note(w) {
                extras.push(words(note, 12.0, ACCENT, s).into_any_element());
            }
        }
    }
    let led = card_led(on, ink, s);
    let name = blocks::model_name(p, kind).unwrap_or("No amp");
    // A smaller footswitch when the card is full, so it stays inside it.
    let switch = if dense || extras.len() > 1 {
        52.0
    } else {
        66.0
    };
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
                .gap(px(knob_gap * s))
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
                        .size(px(switch * s))
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

/// A card's on/off lamp, glowing in the family colour while on.
fn card_led(on: bool, ink: Rgba, s: f32) -> gpui::Div {
    let led = div()
        .size(px(12.0 * s))
        .rounded_full()
        .bg(if on { ink } else { color(0x3a3a3a) });
    if !on {
        return led;
    }
    led.shadow(vec![gpui::BoxShadow {
        color: ink.into(),
        offset: gpui::point(px(0.0), px(0.0)),
        blur_radius: px(12.0 * s),
        spread_radius: px(0.0),
        inset: false,
    }])
}

/// The lane a block runs on. Moving it settles the lane sends the same way
/// a drag does, so a block never lands on a lane that is switched off.
fn lane_pills(
    w: &RodhareistEditorWindow,
    p: &Params,
    kind: StageKind,
    cx: &mut Cx,
    s: f32,
) -> AnyElement {
    let row_now = (p.stage_row[kind as usize] as usize).min(3);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .child(caption("Lane", 10.5, 0.1, SOFT, s).mr(px(4.0 * s)))
        .children((0..4).map(|row| {
            plate::pill(
                ("rodhareist-block-row", row),
                ["1", "2", "3", "4"][row],
                row_now == row,
                accent(),
                plate::PillLook::words(28.0, 12.0, 12.5),
                true,
                s,
                w.chain_edit_cb(
                    cx,
                    move |p| move_to_lane(p, kind, row as u8),
                    ChainFocus::Block(kind),
                ),
            )
        }))
        .into_any_element()
}

/// The amp running a NAM capture. Three plain bands, top to bottom: the
/// capture file with its loudness, the knobs in two labelled groups (the
/// tone stack and the capture's own levels), then lane and bypass.
fn nam_card(w: &RodhareistEditorWindow, p: &Params, cx: &mut Cx, s: f32) -> AnyElement {
    let kind = StageKind::Amp;
    let family = Family::Amp;
    let ink = color(family.color());
    let on = blocks::is_on(p, kind);
    let values: std::collections::HashMap<&'static str, f32> =
        rodharerist::ui_values(p).into_iter().collect();
    let (levels, tone): (Vec<ParamSpec>, Vec<ParamSpec>) = blocks::params(p, kind)
        .into_iter()
        .partition(|spec| spec.id.starts_with("nam_"));
    const KNOB: f32 = 44.0;
    const CELL: f32 = 62.0;
    let knobs = |specs: Vec<ParamSpec>, cx: &mut Cx| -> Vec<AnyElement> {
        specs
            .into_iter()
            .map(|mut spec| {
                // Under the "Capture" heading the prefix only repeats it.
                spec.label = match spec.id {
                    "nam_input_trim" => "In",
                    "nam_output_trim" => "Out",
                    "nam_mix" => "Mix",
                    "nam_slim_size" => "Slim",
                    _ => spec.label,
                };
                let value = values.get(spec.id).copied().unwrap_or(spec.min);
                block_knob(cx, spec, value, ink, KNOB, CELL, s)
            })
            .collect()
    };
    let tone_knobs = knobs(tone, cx);
    let level_knobs = knobs(levels, cx);
    let group = |title: &str, cells: Vec<AnyElement>| {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0 * s))
            .child(caption(title, 10.5, 0.12, SOFT, s))
            .child(div().flex().flex_row().gap(px(2.0 * s)).children(cells))
    };

    // The file band: what is loaded, how loud it plays, and the button.
    let loaded = w.nam_loaded_info();
    let has_capture = loaded.is_some();
    let (status, status_ink) = if w.nam_loading() {
        ("Loading\u{2026}".to_string(), SOFT)
    } else if let Some(error) = w.nam_error() {
        (error, ACCENT)
    } else if has_capture {
        ("Capture loaded".to_string(), SOFT)
    } else {
        ("Load a .nam file from Neural Amp Modeler".to_string(), SOFT)
    };
    let mut load = div()
        .id("rodhareist-load-nam")
        .flex()
        .items_center()
        .flex_shrink_0()
        .h(px(32.0 * s))
        .px(px(14.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(accent())
        .bg(color(0x1f1f1f))
        .text_size(type_px(13.0, s))
        .text_color(color(WHITE))
        .child(if has_capture {
            "Replace\u{2026}"
        } else {
            "Load capture\u{2026}"
        });
    if !w.nam_loading() {
        load = load
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(color(0x2e2e2e)))
            .on_click(w.load_nam_cb(cx));
    }
    let loudness = div()
        .id("rodhareist-nam-loudness")
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(4.0 * s))
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
        .tooltip(fb_tooltip(
            "Normalized evens out captures recorded at different levels",
        ));
    let file = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.0 * s))
        .h(px(60.0 * s))
        .px(px(14.0 * s))
        .rounded(px(6.0 * s))
        .bg(color(0x222222))
        .border(px(1.0))
        .border_color(color(if has_capture { 0x4a4a4a } else { 0x3a3a3a }))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .size(px(36.0 * s))
                .rounded(px(4.0 * s))
                .bg(color(0x1a1a1a))
                .child(icon(
                    family.icon(),
                    (24.0, 24.0),
                    (20.0, 20.0),
                    1.6,
                    if has_capture { ink } else { color(FAINT) },
                    s,
                )),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(2.0 * s))
                .child(
                    words(
                        loaded.unwrap_or_else(|| "No capture".to_string()),
                        16.0,
                        if has_capture { WHITE } else { PALE },
                        s,
                    )
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate(),
                )
                .child(words(status, 12.0, status_ink, s).truncate()),
        )
        .child(loudness)
        .child(load);

    let name = blocks::model_name(p, kind).unwrap_or("NAM Capture");
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
                        .child(words(model_desc(name), 13.0, PALE, s)),
                )
                .child(card_led(on, ink, s)),
        )
        .child(file)
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_center()
                .gap(px(18.0 * s))
                .opacity(if on { 1.0 } else { 0.6 })
                .child(group("Tone", tone_knobs))
                .child(div().w(px(1.0)).h(px(96.0 * s)).bg(color(0x3a3a3a)))
                .child(group("Capture", level_knobs)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(lane_pills(w, p, kind, cx, s))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0 * s))
                        .child(
                            words(
                                plate::tracked(if on { "On" } else { "Bypassed" }, 0.1),
                                12.0,
                                if on { WHITE } else { SOFT },
                                s,
                            )
                            .font_weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            div()
                                .id("rodhareist-block-bypass")
                                .size(px(40.0 * s))
                                .rounded_full()
                                .bg(color(0x1a1a1a))
                                .border(px(3.0 * s))
                                .border_color(color(if on { 0x6a6a6a } else { 0x4a4a4a }))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .hover(|style| style.border_color(color(0x8a8a8a)))
                                .on_click(w.toggle_cb(cx, blocks::enable_id(kind))),
                        ),
                ),
        )
        .into_any_element()
}

/// A file slot (IR or NAM capture): its load button and what is loaded,
/// cut to `status_w`.
#[allow(clippy::too_many_arguments)]
fn load_row(
    id: &'static str,
    button: &'static str,
    loaded: Option<String>,
    loading: bool,
    error: Option<String>,
    on_load: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    status_w: f32,
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
        .child(
            words(status, 12.5, ink, s)
                .max_w(px(status_w * s))
                .truncate(),
        )
        .into_any_element()
}

// ── Settings ───────────────────────────────────────────────────────────────

/// The footswitch slot grid: square cells, eight to a row, a fixed width.
/// A wrapping pill row capped at 420 px ran sixteen slots past the card.
const SLOT_CELL: f32 = 28.0;
const SLOT_GAP: f32 = 3.0;
const SLOT_GRID_W: f32 = 8.0 * SLOT_CELL + 7.0 * SLOT_GAP;

enum Control {
    /// A slot pick: a fixed grid of numbered squares, eight to a row, so up
    /// to sixteen slots stay inside the row.
    Slots(
        Vec<(
            String,
            bool,
            Box<dyn Fn(&mut RodhareistEditorWindow, &mut Cx)>,
        )>,
    ),
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
                        // The DSP clamps the trim to its range; the stepper
                        // must too, or the row reads a level that never plays.
                        Box::new(move |this, cx| {
                            this.set_wire_value("input_trim", step_trim(trim, -0.5), cx)
                        }),
                        Box::new(move |this, cx| {
                            this.set_wire_value("input_trim", step_trim(trim, 0.5), cx)
                        }),
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
                            "Toggles slot {}{}. A pin lasts for this editor session.",
                            slot + 1,
                            p.stage_order[slot].map_or(" \u{00B7} empty".to_string(), |kind| {
                                format!(
                                    " \u{00B7} {}",
                                    blocks::model_name(p, kind).unwrap_or("No amp")
                                )
                            })
                        ),
                        None => "Assign a slot. A pin lasts for this editor session.".into(),
                    };
                    Row {
                        label: format!("Footswitch {}", i + 1),
                        help,
                        control: Control::Slots(
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
        Control::Slots(options) => div()
            .flex()
            .flex_row()
            .flex_wrap()
            .flex_shrink_0()
            .justify_end()
            .w(px(SLOT_GRID_W * s))
            .gap(px(SLOT_GAP * s))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(k, (label, on, pick))| {
                        plate::pill(
                            ("rodhareist-slot-pick", index * 64 + k),
                            label,
                            on,
                            accent(),
                            plate::PillLook::numbers(SLOT_CELL, 0.0, 12.0),
                            true,
                            s,
                            run(std::rc::Rc::from(pick)),
                        )
                        .w(px(SLOT_CELL * s))
                        .justify_center()
                    }),
            )
            .into_any_element(),
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
    frame: &ChainFrame,
    fields: RodFields,
    window: &Window,
    cx: &mut Cx,
) -> AnyElement {
    let viewport = window.viewport_size();
    let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
    let p = w.params_snapshot();
    let focus = resolve_focus(&p.stage_order, w.chain_focus);
    let mut children = vec![display(w, &p, focus, frame, cx, s)];
    // The tuner takes the deck while it is on, whichever page is behind it.
    children.extend(if p.tuner_on {
        tuner_page(w, cx, s)
    } else {
        match w.ui.page {
            Page::Home => home(w, &p, fields.preset_name, cx, s),
            Page::Explore => explore(w, &p, focus, fields.search, cx, s),
            Page::Pedalboard => pedalboard(w, &p, focus, cx, s),
            Page::Settings => settings(w, &p, cx, s),
        }
    });
    // A knob's type-in floats just under where it was double-clicked, above
    // everything, kept inside the window.
    if let Some((field, at)) = fields.knob_value {
        children.push(
            gpui::deferred(
                gpui::anchored()
                    .position(gpui::point(at.x - px(120.0 * s), at.y + px(16.0 * s)))
                    .snap_to_window_with_margin(px(8.0 * s))
                    .child(field),
            )
            .with_priority(2)
            .into_any_element(),
        );
    }
    plate_frame(s, children)
}

/// `trim` moved by `delta` dB, kept inside the input trim's range.
fn step_trim(trim: f32, delta: f32) -> f32 {
    match blocks::trim_spec("input_trim", "") {
        Some(spec) => (trim + delta).clamp(spec.min, spec.max),
        None => trim + delta,
    }
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

    #[test]
    fn an_unused_rig_shows_one_lane_and_four_still_fit() {
        let p = rodharerist::default_params();
        assert_eq!(lane_count(&p), 1);
        assert_eq!(target_look(&p, false).lanes, 1);
        // A drag opens the next lane as a drop target.
        assert_eq!(target_look(&p, true).lanes, 2);
        let two = lane_geom(2, 7);
        assert!(two.size >= 48.0);
        assert!(LANE_TOP + 2.0 * two.lane_h <= LANE_BOTTOM + 0.01);
        let mut parked = p.clone();
        parked.stage_row[StageKind::Delay as usize] = 3;
        assert_eq!(lane_count(&parked), 4);
        assert_eq!(target_look(&parked, true).lanes, 4);
        let four = lane_geom(4, 7);
        assert!(four.size + 6.0 <= four.lane_h);
        assert!(LANE_TOP + 4.0 * four.lane_h <= LANE_BOTTOM + 0.01);
    }

    #[test]
    fn a_block_dropped_on_a_closed_lane_opens_it_and_the_last_one_out_closes_it() {
        let mut p = rodharerist::default_params();
        assert_eq!(p.row_send[0], RowSend::Off);
        move_to_lane(&mut p, StageKind::Delay, 1);
        assert_eq!(p.stage_row[StageKind::Delay as usize], 1);
        assert_eq!(
            p.row_send[0],
            RowSend::Parallel,
            "an Off lane would mute it"
        );
        assert_eq!(lane_count(&p), 2);

        // A mode the player chose stays while the lane has blocks.
        p.row_send[0] = RowSend::WetDryWet;
        move_to_lane(&mut p, StageKind::Reverb, 1);
        assert_eq!(p.row_send[0], RowSend::WetDryWet);
        move_to_lane(&mut p, StageKind::Reverb, 0);
        assert_eq!(p.row_send[0], RowSend::WetDryWet);

        move_to_lane(&mut p, StageKind::Delay, 0);
        assert_eq!(p.row_send[0], RowSend::Off);
        assert_eq!(lane_count(&p), 1);
    }

    #[test]
    fn a_chain_change_glides_from_where_the_blocks_were_drawn() {
        let mut ui = RodUi::default();
        let mut p = rodharerist::default_params();
        let t0 = Instant::now();
        // The first frame has nothing to move from.
        let (rest, moving) = chain_frame(&mut ui, &p, t0);
        assert!(!moving);
        let at = |look: &ChainLook, kind| {
            look.blocks
                .iter()
                .find(|entry| entry.0 == kind)
                .map(|entry| (entry.2, entry.3))
                .unwrap()
        };
        let start = at(&rest.look, StageKind::Delay);

        move_to_lane(&mut p, StageKind::Delay, 1);
        let (first, moving) = chain_frame(&mut ui, &p, t0);
        assert!(moving);
        assert_eq!(at(&first.look, StageKind::Delay), start);
        assert!(first.changed_lane.contains(&StageKind::Delay));
        assert_eq!(first.lane_alpha(1), 0.0, "the new lane fades in");

        let (mid, moving) = chain_frame(&mut ui, &p, t0 + CHAIN_MOTION / 2);
        assert!(moving);
        let end = at(&target_look(&p, false), StageKind::Delay);
        let (_, mid_top) = at(&mid.look, StageKind::Delay);
        assert!(mid_top > start.1 && mid_top < end.1 || mid_top < start.1 && mid_top > end.1);

        let (done, moving) = chain_frame(&mut ui, &p, t0 + CHAIN_MOTION);
        assert!(!moving);
        assert_eq!(at(&done.look, StageKind::Delay), end);
        assert!(ui.chain_motion.is_none());
    }

    #[test]
    fn default_lanes_stay_wet_on_the_main_path_and_off_below() {
        let p = rodharerist::default_params();
        let main = lane_controls(&p, 0);
        assert_eq!(main.number, 1);
        assert!(!main.wet);
        assert!(!main.send_live);
        assert_eq!(main.detail, "Lane 1 · main path");
        assert_eq!(p.line2_mode, Line2Mode::Wet);
        assert_eq!(p.line2_mix, 70.0);
        for row in 1..4 {
            let lane = lane_controls(&p, row);
            assert!(!lane.wet);
            assert!(!lane.send_live);
            assert_eq!(p.row_send[row - 1], RowSend::Off);
        }
    }

    #[test]
    fn wet_dry_wet_names_the_lane_and_the_wet_path() {
        let mut p = rodharerist::default_params();
        p.line2_mode = Line2Mode::WetDryWet;
        let main = lane_controls(&p, 0);
        assert!(main.wet);
        assert!(main.send_live);
        assert_eq!(main.mode_id, "line2_mode");
        assert_eq!(main.send_id, "line2_mix");
        assert_eq!(main.detail, "Lane 1 · this lane is the wet path");

        p.row_send[2] = RowSend::WetDryWet;
        let lane = lane_controls(&p, 3);
        assert_eq!(lane.number, 4);
        assert!(lane.wet);
        assert!(lane.send_live);
        assert_eq!(lane.mode_id, "row_send_3");
        assert_eq!(lane.send_id, "row_level_3");
        assert_eq!(lane.detail, "Lane 4 · this lane is the wet path");
    }

    #[test]
    fn the_ir_line_names_the_cabinet_only_when_that_model_is_selected() {
        let p = rodharerist::default_params();
        assert!(!ir_path_line(&p).contains("impulse"));
        let mut ir = p;
        ir.cab_model = CabModel::Ir;
        assert_eq!(ir_path_line(&ir), "Cabinet is the impulse response");
        ir.cab_on = false;
        assert_eq!(ir_path_line(&ir), "Cabinet is bypassed");
        ir.stage_order = [None; rodharerist::PATH_SLOTS];
        assert_eq!(ir_path_line(&ir), "Cabinet is not on the path");
    }

    #[test]
    fn gain_groups_keep_a_clean_amp_out_of_high_gain() {
        let all = entries(&rodharerist::default_params());
        let grouped = |family: Family, label: &str| {
            all.iter()
                .any(|entry| entry.browse == family && entry.label == label)
        };
        assert!(grouped(Family::Clean, "Twin Clean"));
        assert!(!grouped(Family::HighGain, "Twin Clean"));
        assert!(grouped(Family::HighGain, "Herbert Lead"));
        assert!(grouped(Family::HighGain, "Recto Modern"));
        assert!(grouped(Family::Crunch, "Mandarin 80"));
        assert!(grouped(Family::Drive, "Green Screamer"));
        assert!(grouped(Family::Amp, "NAM Capture"));
        assert!(!grouped(Family::Clean, "NAM Capture"));
    }

    /// #407: the gain classes and the NAM engine are subtabs of one Amp
    /// category, and every amp entry sits in exactly one of them.
    #[test]
    fn amp_classes_are_subtabs_of_amp() {
        let all = entries(&rodharerist::default_params());
        let amps: Vec<_> = all.iter().filter(|e| e.category == Category::Amp).collect();
        assert!(!amps.is_empty());
        for entry in &amps {
            assert_eq!(entry.browse.parent(), Family::Amp, "{}", entry.label);
            assert!(
                Family::AMP_TABS
                    .iter()
                    .any(|(class, _)| *class == entry.browse),
                "{}",
                entry.label
            );
        }
        let in_tabs: usize = Family::AMP_TABS
            .iter()
            .map(|(class, _)| amps.iter().filter(|e| e.browse == *class).count())
            .sum();
        assert_eq!(in_tabs, amps.len());
        // Explore lists the nine chain families, Amp once.
        assert_eq!(Family::ALL.iter().filter(|f| **f == Family::Amp).count(), 1);
        assert!(!Family::ALL.contains(&Family::Clean));
    }

    /// The Sync knob turns from the shortest division to the longest, and
    /// holds every division the core has (not Free).
    #[test]
    fn the_sync_knob_steps_shortest_first_through_every_division() {
        let steps = sync_steps();
        assert!(!steps.contains(&DelaySync::Free));
        assert_eq!(steps.len(), DelaySync::ALL.len() - 1);
        let beats: Vec<f32> = steps.iter().map(|s| s.beats().unwrap()).collect();
        assert!(beats.windows(2).all(|w| w[0] < w[1]), "{beats:?}");
        assert_eq!(steps.first(), Some(&DelaySync::Sixteenth));
        assert_eq!(steps.last(), Some(&DelaySync::Half));
    }

    /// Switching a free time to Sync lands on the division closest to it.
    #[test]
    fn sync_starts_from_the_division_nearest_the_free_time() {
        // At 120 BPM a quarter is 500 ms, an eighth 250, a dotted eighth 375.
        assert_eq!(nearest_sync(500.0, 120.0), DelaySync::Quarter);
        assert_eq!(nearest_sync(260.0, 120.0), DelaySync::Eighth);
        assert_eq!(nearest_sync(370.0, 120.0), DelaySync::DottedEighth);
    }

    #[test]
    fn pitch_of_names_the_note_and_octave() {
        let (name, octave, cents) = pitch_of(110.0).unwrap();
        assert_eq!((name, octave), ("A", 2));
        assert!(cents.abs() < 0.01);
        let (name, octave, cents) = pitch_of(82.41).unwrap();
        assert_eq!((name, octave), ("E", 2));
        assert!(cents.abs() < 1.0);
        assert!(pitch_of(0.0).is_none());
    }

    /// The needle eases onto the reading, an in-tune note stops animating
    /// once its pulse is over, and silence clears the note after the hold.
    #[test]
    fn the_tuner_eases_then_rests() {
        let mut look = TunerLook::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let sharp = pitch_of(111.22);
        let mut moving = true;
        for frame in 0..60 {
            moving = tuner_frame(&mut look, sharp, at(frame * 16));
        }
        let cents = sharp.unwrap().2;
        assert!(
            (look.cents - cents).abs() < 0.5,
            "{} vs {cents}",
            look.cents
        );
        assert!(moving, "a note off the pitch keeps the strobe moving");

        let tuned = pitch_of(110.01);
        for frame in 60..120 {
            moving = tuner_frame(&mut look, tuned, at(frame * 16));
        }
        assert!(look.in_tune_since.is_some());
        assert!(!moving, "in tune and settled: no frames");

        // Silence: the note holds briefly, then clears and the needle rests.
        assert!(tuner_frame(&mut look, None, at(120 * 16)));
        assert!(look.note.is_some());
        for frame in 121..200 {
            moving = tuner_frame(&mut look, None, at(frame * 16));
        }
        assert!(look.note.is_none());
        assert!(!moving);
    }
}
