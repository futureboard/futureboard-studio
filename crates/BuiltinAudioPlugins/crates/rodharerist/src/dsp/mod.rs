//! Rodhareist — flagship guitar multi-effect DSP core.
//!
//! A realtime-safe stereo pedalboard/amp chain, engine-agnostic like the other
//! `BuiltinAudioPlugins` cores. The signal path mirrors the React editor's
//! signal chain exactly:
//!
//! ```text
//! Gate → Comp → Drive → Amp → EQ → Mod → Delay → Reverb → Cabinet
//! ```
//!
//! (user-reorderable; a Wah stage starts in the rack, the Mod slot runs one of
//! chorus / phaser / flanger / tremolo, and the Delay slot one of tape /
//! digital / analog / ping-pong / dual)
//!
//! ## Realtime rules
//!
//! Every buffer (delay lines, reverb combs/allpasses) is preallocated in
//! [`Dsp::new`] / [`Dsp::set_sample_rate`]. [`StereoEffect::process_stereo`]
//! performs no allocation, logging, or locking. Control-thread updates go through
//! [`Dsp::set_params`] / [`Dsp::apply_ui_param`], which recompute coefficients
//! outside the audio callback.
//!
//! Only the MIT/Apache [`biquad`] crate is used for filtering; every waveshaper,
//! delay and reverb is hand-written here (no extra third-party runtime deps).

mod amp;
mod cab;
mod chorus;
mod comp;
mod delay;
mod drive;
mod drive_models;
mod eq;
mod flanger;
mod gate;
mod handoff;
mod ir;
mod mod_stage;
mod nam;
mod nonlinear;
mod phaser;
mod rate;
mod reverb;
mod smooth;
mod tone_stage;
mod tremolo;
mod wah;
mod wav;

use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    time_constant,
};

use cab::Cabinet;
use comp::CompStage;
use delay::DelayStage;
use drive::Drive;
use eq::EqStage;
use gate::NoiseGate;
pub use ir::{
    IrInfo, IrLoadError, IrLoader, MAX_IR_SECONDS, PARTITION as IR_PARTITION_SAMPLES,
    PreparedIrRuntime, prepare_ir_runtime,
};
use mod_stage::ModStage;
pub use nam::{
    NAM_BLOCK as NAM_BLOCK_SAMPLES, NamCaptureInfo, NamLoadError, NamLoader, PreparedNamRuntime,
    prepare_nam_runtime,
};
use reverb::PlateReverb;
pub use tone_stage::ToneEngineKind;
use tone_stage::ToneStage;
use wah::Wah;

pub const PLUGIN_ID: &str = "futureboard.rodharerist";

/// Number of slots in the user-orderable signal path (one per [`StageKind`]).
pub const PATH_SLOTS: usize = 15;

/// One slot in the Helix-style signal path. Order is user-editable.
///
/// Discriminants are a wire/persistence ABI (`path_slot_*` values, saved
/// `stage_order`) — append new stages, never renumber.
///
/// The `*2` variants are the second instance of the five stages a player
/// actually doubles on a real board: two drives (a screamer pushing a Klon),
/// chorus *and* phaser, a slapback into a long repeat, an EQ each side of the
/// amp, a compressor at each end. They are independent stages with their own
/// model, enable and knobs — not a re-run of the first. Amp, Cab, Reverb, Gate
/// and Wah stay single: a second cabinet convolver or NAM model would double
/// this plugin's heaviest allocation for a rig nobody builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum StageKind {
    Gate = 0,
    Drive = 1,
    Amp = 2,
    Mod = 3,
    Delay = 4,
    Reverb = 5,
    Cab = 6,
    Comp = 7,
    Eq = 8,
    Wah = 9,
    Drive2 = 10,
    Mod2 = 11,
    Delay2 = 12,
    Eq2 = 13,
    Comp2 = 14,
}

impl StageKind {
    /// Number of distinct stages. Not the same thing as [`PATH_SLOTS`], even
    /// when the numbers coincide.
    pub const COUNT: usize = 15;

    pub const ALL: &'static [Self] = &[
        Self::Gate,
        Self::Drive,
        Self::Amp,
        Self::Mod,
        Self::Delay,
        Self::Reverb,
        Self::Cab,
        Self::Comp,
        Self::Eq,
        Self::Wah,
        Self::Drive2,
        Self::Mod2,
        Self::Delay2,
        Self::Eq2,
        Self::Comp2,
    ];

    pub fn from_index(i: i32) -> Option<Self> {
        match i {
            0 => Some(Self::Gate),
            1 => Some(Self::Drive),
            2 => Some(Self::Amp),
            3 => Some(Self::Mod),
            4 => Some(Self::Delay),
            5 => Some(Self::Reverb),
            6 => Some(Self::Cab),
            7 => Some(Self::Comp),
            8 => Some(Self::Eq),
            9 => Some(Self::Wah),
            10 => Some(Self::Drive2),
            11 => Some(Self::Mod2),
            12 => Some(Self::Delay2),
            13 => Some(Self::Eq2),
            14 => Some(Self::Comp2),
            _ => None,
        }
    }

    pub fn index(self) -> u8 {
        self as u8
    }

    /// Default factory path order: comp after the gate (level control before
    /// dirt), EQ after the amp (tone shaping before time effects). Comp and
    /// EQ default to neutral settings, so the factory patch's character is
    /// unchanged from the 7-stage era.
    ///
    /// The Wah stage is deliberately *not* in the default path — a wah is
    /// never tonally neutral, so it starts in the editor's rack (last slot
    /// empty) and joins the path only when the user places it. The same holds
    /// for every second instance: a doubled block is a choice, never a default.
    pub fn default_path() -> [Option<Self>; PATH_SLOTS] {
        let mut path = [None; PATH_SLOTS];
        path[0] = Some(Self::Gate);
        path[1] = Some(Self::Comp);
        path[2] = Some(Self::Drive);
        path[3] = Some(Self::Amp);
        path[4] = Some(Self::Eq);
        path[5] = Some(Self::Mod);
        path[6] = Some(Self::Delay);
        path[7] = Some(Self::Reverb);
        path[8] = Some(Self::Cab);
        path
    }

    /// The first instance this stage doubles, if it is a second instance.
    /// Used to read a `*2` block's shared traits (colour, model list) from the
    /// stage it duplicates.
    pub fn doubles(self) -> Option<Self> {
        match self {
            Self::Drive2 => Some(Self::Drive),
            Self::Mod2 => Some(Self::Mod),
            Self::Delay2 => Some(Self::Delay),
            Self::Eq2 => Some(Self::Eq),
            Self::Comp2 => Some(Self::Comp),
            _ => None,
        }
    }
}

/// Overdrive/boost voicing, matching the editor's `dist` models.
///
/// APPEND-ONLY: the variant order is `ALL`'s order, and that index is the
/// `drive_model` wire value — never reorder or insert mid-list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DriveModel {
    /// "Green Screamer" — mid-boosted tube-screamer style overdrive.
    Screamer,
    /// "Minotaur Boost" — cleaner, near-transparent analog boost.
    Minotaur,
    /// "Rats Nest" — hard-clipping filthy distortion.
    Rat,
    /// "Breaker Blues" — soft low-gain overdrive.
    Breaker,
    /// "Face Fuzz" — gated asymmetric fuzz.
    Fuzz,
    /// "Centurion" — transparent mid-forward OD.
    Centurion,
    /// "DS Classic" — raw orange-box hard clipper (DS-1 style).
    DsOne,
    /// "Super Drive" — asymmetric-diode smooth overdrive (SD-1 style).
    SuperDrive,
    /// "Metal Core" — huge-gain scooped-mid metal distortion (MT-2 style).
    MetalCore,
    /// "Tight Rift" — modern multi-stage tight high-gain (Neural-style).
    TightRift,
    /// "Amber Crunch" — bright silicon hard-clip blues/rock rhythm OD.
    AmberCrunch,
    /// "Copper Fuzz" — silicon fuzz, tighter and more aggressive than the
    /// germanium Face Fuzz.
    CopperFuzz,
}

impl DriveModel {
    pub const ALL: &'static [Self] = &[
        Self::Screamer,
        Self::Minotaur,
        Self::Rat,
        Self::Breaker,
        Self::Fuzz,
        Self::Centurion,
        Self::DsOne,
        Self::SuperDrive,
        Self::MetalCore,
        Self::TightRift,
        // Append-only past this point (wire ABI).
        Self::AmberCrunch,
        Self::CopperFuzz,
    ];

    /// Map the editor model id.
    pub fn from_model_id(id: &str) -> Option<Self> {
        match id {
            "screamer" => Some(Self::Screamer),
            "minotaur" => Some(Self::Minotaur),
            "rat" => Some(Self::Rat),
            "breaker" => Some(Self::Breaker),
            "fuzz" => Some(Self::Fuzz),
            "centurion" => Some(Self::Centurion),
            "ds_one" => Some(Self::DsOne),
            "super_drive" => Some(Self::SuperDrive),
            "metal_core" => Some(Self::MetalCore),
            "tight_rift" => Some(Self::TightRift),
            "amber_crunch" => Some(Self::AmberCrunch),
            "copper_fuzz" => Some(Self::CopperFuzz),
            _ => None,
        }
    }

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::Screamer)
    }
}

/// Amplifier voicing, matching the editor's `amp` models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AmpModel {
    /// "Mandarin 80" — warm, mid-forward British tube head.
    Mandarin,
    /// "Brit Plexi 100" — bright, open plexiglass Super Lead.
    Plexi,
    /// "Twin Clean" — high-headroom American clean.
    Twin,
    /// "Top Boost" — chiming British combo.
    TopBoost,
    /// "Recto Modern" — tight high-gain modern.
    Recto,
    /// "JCM Crunch" — classic British crunch.
    Jcm,
    /// "Lead Slate" — saturated hot-rodded lead.
    Slate,
    /// "Bassman" — loose American bass-heavy.
    Bassman,
    /// "Overdrive Special" — smooth, singing boutique low/mid-gain sustain.
    Boutique,
    /// "Invader 5150" — tight, scooped modern high-gain full stack.
    Invader,
    /// "Tweed Deluxe" — small, early-breakup single-speaker combo.
    TweedCombo,
}

impl AmpModel {
    pub const ALL: &'static [Self] = &[
        Self::Mandarin,
        Self::Plexi,
        Self::Twin,
        Self::TopBoost,
        Self::Recto,
        Self::Jcm,
        Self::Slate,
        Self::Bassman,
        // Append-only past this point (wire ABI) — see `from_index`.
        Self::Boutique,
        Self::Invader,
        Self::TweedCombo,
    ];

    pub fn from_model_id(id: &str) -> Option<Self> {
        match id {
            "mandarin" => Some(Self::Mandarin),
            "plexi" => Some(Self::Plexi),
            "twin" => Some(Self::Twin),
            "topboost" => Some(Self::TopBoost),
            "recto" => Some(Self::Recto),
            "jcm" => Some(Self::Jcm),
            "slate" => Some(Self::Slate),
            "bassman" => Some(Self::Bassman),
            "boutique" => Some(Self::Boutique),
            "invader" => Some(Self::Invader),
            "tweed_combo" => Some(Self::TweedCombo),
            _ => None,
        }
    }

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::Mandarin)
    }
}

/// Cabinet voicing, matching the editor's `cab` models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CabModel {
    /// "1960v Vintage 4x12" — Celestion vintage cabinet sim.
    Vintage4x12,
    /// "American 2x12" — brighter, tighter open-back combo.
    American2x12,
    /// "Tweed 1x12" — small, boxy, rolled-off single speaker.
    Tweed1x12,
    /// "Modern 4x12" — tight, scooped, extended highs.
    Modern4x12,
    /// Wide open-back cabinet with front/rear cancellation.
    OpenBack,
    /// Warm, moderately damped vintage 2x12.
    Vintage2x12,
    /// Deep, tightly damped oversized closed 4x12.
    Oversized4x12,
    /// Extended-low-frequency bass cabinet.
    BassCabinet,
    /// "British Stack 4x12" — mid-forward greenback-voiced closed 4x12
    /// (Marshall-1960-style, trademark-free voicing).
    Brit4x12,
    /// "Uberkab 4x12" — tight, scooped German high-gain closed 4x12
    /// (ENGL-style voicing).
    Uber4x12,
    /// "SLO Custom 4x12" — smooth, singing high-gain closed 4x12
    /// (Soldano-style voicing).
    Slo4x12,
    /// "Impulse Response" — not a modeled voicing at all: convolution with a
    /// user-loaded `.wav` IR (see [`ir::IrConvolver`]). The mic position and
    /// distance knobs do not apply — the IR already is a miked cabinet — and
    /// the slot passes dry until a file is loaded.
    Ir,
    /// "Modern 2x12" — tight closed-back pair: the `ModernClosed` family's
    /// scoop and extended top, scaled down from the 4x12s.
    Modern2x12,
    /// "American 1x12 Combo" — small, bright, open single speaker; brighter
    /// and less boxy than the Tweed 1x12.
    American1x12,
}

impl CabModel {
    pub const ALL: &'static [Self] = &[
        Self::Vintage4x12,
        Self::American2x12,
        Self::Tweed1x12,
        Self::Modern4x12,
        Self::OpenBack,
        Self::Vintage2x12,
        Self::Oversized4x12,
        Self::BassCabinet,
        Self::Brit4x12,
        Self::Uber4x12,
        Self::Slo4x12,
        Self::Ir,
        // Append-only past this point (wire ABI).
        Self::Modern2x12,
        Self::American1x12,
    ];

    /// The modeled voicings only — [`Self::ALL`] minus [`Self::Ir`], which is
    /// a convolution engine rather than a filter profile. Anything asserting
    /// or enumerating *voicing* behaviour wants this list, not `ALL`.
    pub const MODELED: &'static [Self] = &[
        Self::Vintage4x12,
        Self::American2x12,
        Self::Tweed1x12,
        Self::Modern4x12,
        Self::OpenBack,
        Self::Vintage2x12,
        Self::Oversized4x12,
        Self::BassCabinet,
        Self::Brit4x12,
        Self::Uber4x12,
        Self::Slo4x12,
        Self::Modern2x12,
        Self::American1x12,
    ];

    pub fn from_model_id(id: &str) -> Option<Self> {
        match id {
            "vintage_cab" => Some(Self::Vintage4x12),
            "american_2x12" => Some(Self::American2x12),
            "tweed_1x12" => Some(Self::Tweed1x12),
            "modern_412" => Some(Self::Modern4x12),
            "open_back" => Some(Self::OpenBack),
            "vintage_212" => Some(Self::Vintage2x12),
            "oversized_412" => Some(Self::Oversized4x12),
            "bass_cabinet" => Some(Self::BassCabinet),
            "brit_412" => Some(Self::Brit4x12),
            "uber_412" => Some(Self::Uber4x12),
            "slo_412" => Some(Self::Slo4x12),
            "ir" => Some(Self::Ir),
            "modern_212" => Some(Self::Modern2x12),
            "american_1x12" => Some(Self::American1x12),
            _ => None,
        }
    }

    pub fn from_index(i: u32) -> Self {
        Self::ALL
            .get(i as usize)
            .copied()
            .unwrap_or(Self::Vintage4x12)
    }
}

/// Microphone capsule topology used by the cabinet stage.
///
/// APPEND-ONLY: the index is persisted and automated through
/// `cab_mic_type`; old state defaults to Dynamic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum MicModel {
    #[default]
    Dynamic,
    Ribbon,
    Condenser,
}

impl MicModel {
    pub const ALL: &'static [Self] = &[Self::Dynamic, Self::Ribbon, Self::Condenser];

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::Dynamic)
    }

    pub fn index(self) -> u8 {
        self as u8
    }
}

/// Modulation algorithm in the Mod slot, matching the editor's `mod` models.
/// All share the Rate/Depth/Mix knobs (`chorus_*` wire ids), like the Drive
/// slot's models share `drive_*`.
///
/// APPEND-ONLY: index into `ALL` is the `mod_model` wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ModModel {
    /// "70s Analog Chorus" — dual modulated delay lines in quadrature.
    #[default]
    Chorus,
    /// "Vibe Phase 90" — swept 4-stage allpass phaser.
    Phaser,
    /// "Jet Flanger" — short modulated delay with regeneration.
    Flanger,
    /// "Opto Tremolo" — amp-style amplitude modulation.
    Tremolo,
    /// "Molam Swirl" — Uni-Vibe-style staggered throb for Isan / luk-thung leads.
    MolamSwirl,
    /// "Phin Vibe" — staggered, feedback-free Univibe-style throb.
    PhinVibe,
    /// "Khaen Swirl" — eight stages, four nulls, lush and continuous.
    KhaenSwirl,
    /// "Bi-Lam" — a twelve-stage cascade, slow and very wide.
    BiLam,
    /// "Isan Jet" — six stages, hard regeneration, fast and narrow up top.
    IsanJet,
    /// "Soft Phase" — two stages, one gentle notch: the subtle end of the
    /// range.
    SoftPhase,
    /// "Wide Vibe" — linked stereo spread for a genuinely wide swirl.
    WideVibe,
}

impl ModModel {
    // Appended only. The first four indices are what already-saved projects
    // carry on the `mod_model` wire, so the phaser voices go on the end even
    // though the editor groups them next to the original phaser.
    pub const ALL: &'static [Self] = &[
        Self::Chorus,
        Self::Phaser,
        Self::Flanger,
        Self::Tremolo,
        Self::MolamSwirl,
        Self::PhinVibe,
        Self::KhaenSwirl,
        Self::BiLam,
        Self::IsanJet,
        Self::SoftPhase,
        Self::WideVibe,
    ];

    pub fn from_model_id(id: &str) -> Option<Self> {
        match id {
            "chorus" => Some(Self::Chorus),
            "phaser" => Some(Self::Phaser),
            "flanger" => Some(Self::Flanger),
            "tremolo" => Some(Self::Tremolo),
            "molam_swirl" => Some(Self::MolamSwirl),
            "phin_vibe" => Some(Self::PhinVibe),
            "khaen_swirl" => Some(Self::KhaenSwirl),
            "bi_lam" => Some(Self::BiLam),
            "isan_jet" => Some(Self::IsanJet),
            "soft_phase" => Some(Self::SoftPhase),
            "wide_vibe" => Some(Self::WideVibe),
            _ => None,
        }
    }

    /// The voiced phaser circuit this model runs, if it is a phaser at all.
    pub(in crate::dsp) fn phaser_voice(self) -> Option<phaser::PhaserVoice> {
        use phaser::PhaserVoice as V;
        match self {
            Self::Phaser => Some(V::Phase90),
            Self::MolamSwirl => Some(V::MolamSwirl),
            Self::PhinVibe => Some(V::PhinVibe),
            Self::KhaenSwirl => Some(V::KhaenSwirl),
            Self::BiLam => Some(V::BiLam),
            Self::IsanJet => Some(V::IsanJet),
            Self::SoftPhase => Some(V::SoftPhase),
            Self::WideVibe => Some(V::WideVibe),
            Self::Chorus | Self::Flanger | Self::Tremolo => None,
        }
    }

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::Chorus)
    }
}

/// Wah voicing, matching the editor's `wah` models.
///
/// APPEND-ONLY: index into `ALL` is the `wah_model` wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum WahModel {
    /// "Cry Wah" — pedal-position sweep (the Position knob is the pedal).
    #[default]
    CryWah,
    /// "Touch Wah" — envelope-follower auto wah.
    TouchWah,
}

impl WahModel {
    pub const ALL: &'static [Self] = &[Self::CryWah, Self::TouchWah];

    pub fn from_model_id(id: &str) -> Option<Self> {
        match id {
            "cry_wah" => Some(Self::CryWah),
            "touch_wah" => Some(Self::TouchWah),
            _ => None,
        }
    }

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::CryWah)
    }
}

/// EQ voicing in the Eq slot, matching the editor's `eq` models. All three
/// share the same six `eq_*` knobs (low shelf, two sweepable bells, high
/// shelf) — only the fixed shelf corners and bell Q differ, see
/// `eq::EqProfile`.
///
/// APPEND-ONLY: index into `ALL` is the `eq_model` wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum EqModel {
    /// "Studio EQ" — the original curve.
    #[default]
    Studio,
    /// "Vintage EQ" — passive-console character: lower shelf corner, earlier
    /// top roll-off, wide smooth bells.
    Vintage,
    /// "Modern EQ" — surgical/digital character: extended shelf range,
    /// narrow precise bells.
    Modern,
}

impl EqModel {
    pub const ALL: &'static [Self] = &[Self::Studio, Self::Vintage, Self::Modern];

    pub fn from_model_id(id: &str) -> Option<Self> {
        match id {
            // "parametric" — the editor's original (and, until now, only) EQ
            // model id, predating the model select. Kept as Studio's id
            // rather than renamed, so every existing preset/project
            // referencing it keeps its exact voicing.
            "parametric" => Some(Self::Studio),
            "vintage_eq" => Some(Self::Vintage),
            "modern_eq" => Some(Self::Modern),
            _ => None,
        }
    }

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::Studio)
    }
}

/// Reverb voicing in the Reverb slot, matching the editor's `verb` models. All
/// share the Decay/Mix knobs (`reverb_*` wire ids), like the Mod slot's models
/// share `chorus_*`.
///
/// APPEND-ONLY: index into `ALL` is the `reverb_model` wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ReverbModel {
    /// "Studio Plate" — bright, dense, immediate metallic plate.
    #[default]
    Plate,
    /// "Tracking Room" — short, damped, early — a tight room.
    Room,
    /// "Concert Hall" — long predelay and tail, spacious.
    Hall,
    /// "Shimmer" — hall bed with an octave-up voice regenerating in the tail.
    Shimmer,
}

impl ReverbModel {
    pub const ALL: &'static [Self] = &[Self::Plate, Self::Room, Self::Hall, Self::Shimmer];

    pub fn from_model_id(id: &str) -> Option<Self> {
        match id {
            "plate" => Some(Self::Plate),
            "room" => Some(Self::Room),
            "hall" => Some(Self::Hall),
            "shimmer" => Some(Self::Shimmer),
            _ => None,
        }
    }

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::Plate)
    }
}

/// Echo voicing in the Delay slot, matching the editor's `delay` models. All
/// share the Time/Feedback/Mix/Tone knobs (`delay_*` wire ids), like the Mod
/// slot's models share `chorus_*`.
///
/// APPEND-ONLY: index into `ALL` is the `delay_model` wire value. `Tape` stays
/// first — it is what every project saved before this stage grew models
/// deserializes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DelayModel {
    /// "Tape Echo" — warm, saturated repeats with audible wow and flutter.
    #[default]
    Tape,
    /// "Digital Delay" — clean, full-bandwidth, unmoving repeats.
    Digital,
    /// "Analog BBD" — dark, thinning, compressed bucket-brigade repeats.
    Analog,
    /// "Ping Pong" — repeats alternating across the stereo image.
    PingPong,
    /// "Dual Delay" — quarter note left against a dotted eighth right.
    Dual,
}

impl DelayModel {
    pub const ALL: &'static [Self] = &[
        Self::Tape,
        Self::Digital,
        Self::Analog,
        Self::PingPong,
        Self::Dual,
    ];

    pub fn from_model_id(id: &str) -> Option<Self> {
        match id {
            "tape" => Some(Self::Tape),
            "digital" => Some(Self::Digital),
            "analog" => Some(Self::Analog),
            "ping_pong" => Some(Self::PingPong),
            "dual" => Some(Self::Dual),
            _ => None,
        }
    }

    pub fn from_index(i: u32) -> Self {
        Self::ALL.get(i as usize).copied().unwrap_or(Self::Tape)
    }
}

/// Full parameter set. Knob ranges match `editorui/src/data.ts` one-to-one so the
/// React UI and the bridge speak the same units.
///
/// This is the per-insert DSP state: the shared built-in editor's
/// `SelectInstanceMsg.state` (see `builtin_plugin_editor_window.rs`) is a
/// serialized [`Params`] wrapped in [`RodhareistState`], not a separately
/// invented schema — there is no reason for the wire format to diverge from
/// what the DSP actually holds.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Params {
    pub power: bool,

    /// Global input trim applied before the first stage (dB, -24..24). This is
    /// the level the NAM capture sees, so it changes the model's gain and
    /// tonal response — it is a first-class control, not a hidden utility.
    pub input_trim_db: f32,
    /// Global output trim applied after the last stage (dB, -24..24).
    pub output_trim_db: f32,

    // Per-stage enables (editor "bypass" toggles, inverted).
    pub gate_on: bool,
    pub drive_on: bool,
    pub amp_on: bool,
    pub mod_on: bool,
    pub delay_on: bool,
    pub reverb_on: bool,
    pub cab_on: bool,
    #[serde(default = "default_true")]
    pub comp_on: bool,
    #[serde(default = "default_true")]
    pub eq_on: bool,
    #[serde(default = "default_true")]
    pub wah_on: bool,

    pub drive_model: DriveModel,
    pub amp_model: AmpModel,
    pub cab_model: CabModel,
    #[serde(default)]
    pub mic_model: MicModel,
    /// Which algorithm the Mod slot runs (shares the `chorus_*` knobs).
    #[serde(default)]
    pub mod_model: ModModel,
    #[serde(default)]
    pub wah_model: WahModel,
    /// Which voicing the Eq slot runs (shares the `eq_*` knobs).
    #[serde(default)]
    pub eq_model: EqModel,
    /// Which algorithm the Reverb slot runs (shares the `reverb_*` knobs).
    #[serde(default)]
    pub reverb_model: ReverbModel,
    /// Which algorithm the Delay slot runs (shares the `delay_*` knobs).
    #[serde(default)]
    pub delay_model: DelayModel,

    /// Which engine the Tone/Amp slot runs: Classic Amp, NAM Capture, or Bypass.
    /// Mutually exclusive — never more than one processes at a time.
    pub tone_engine: ToneEngineKind,

    /// Helix-style ordered path. `None` = empty slot (stage not in path).
    /// Stages absent from this list are not processed.
    #[serde(deserialize_with = "deserialize_stage_order")]
    pub stage_order: [Option<StageKind>; PATH_SLOTS],

    /// Noise gate threshold (dB, -80..0).
    pub gate_thresh_db: f32,

    // Drive (0..10).
    pub drive_gain: f32,
    pub drive_tone: f32,
    pub drive_level: f32,

    // Amp tonestack (0..10).
    pub amp_gain: f32,
    pub amp_bass: f32,
    pub amp_middle: f32,
    pub amp_treble: f32,
    pub amp_presence: f32,
    pub amp_master: f32,

    // Chorus.
    pub chorus_rate: f32,  // 0..10
    pub chorus_depth: f32, // 0..10
    pub chorus_mix: f32,   // 0..100 %

    // Delay.
    pub delay_time_ms: f32, // 40..1200
    pub delay_fb: f32,      // 0..100 %
    pub delay_mix: f32,     // 0..100 %
    /// Feedback-path brightness (0..10, 5 = the selected voicing's own
    /// centre). Every model reads it; each stays centred on its own colour.
    #[serde(default = "default_delay_tone")]
    pub delay_tone: f32,

    // Reverb.
    pub reverb_decay_s: f32, // 0.5..15
    pub reverb_mix: f32,     // 0..100 %
    /// Octave-up feedback amount for the Shimmer model (0..100 %). Other
    /// reverb models ignore it while preserving the value for model switches.
    #[serde(default = "default_reverb_shimmer")]
    pub reverb_shimmer: f32,

    // Cabinet.
    pub cab_mic: f32,  // 0..100 % (mic position / brightness)
    pub cab_dist: f32, // 0..100 % (distance / roll-off)

    // Wah (defaults are a mid-heel pedal; the stage starts out of the path).
    #[serde(default = "default_wah_pos")]
    pub wah_pos: f32, // 0..10 (pedal position / base frequency)
    #[serde(default = "default_wah_res")]
    pub wah_res: f32, // 0..10 (resonance)
    #[serde(default = "default_wah_sens")]
    pub wah_sens: f32, // 0..10 (envelope sensitivity, Touch Wah only)

    // Studio compressor (defaults are gentle/neutral — see `default_params`).
    #[serde(default = "default_comp_thresh")]
    pub comp_thresh_db: f32, // -60..0
    #[serde(default = "default_comp_ratio")]
    pub comp_ratio: f32, // 1..20
    #[serde(default = "default_comp_attack")]
    pub comp_attack_ms: f32, // 0.1..100
    #[serde(default = "default_comp_release")]
    pub comp_release_ms: f32, // 10..1000
    #[serde(default)]
    pub comp_makeup_db: f32, // 0..24

    // Studio EQ (0 dB gains = bit-transparent).
    #[serde(default)]
    pub eq_low_gain_db: f32, // -15..15
    #[serde(default = "default_eq_mid1_freq")]
    pub eq_mid1_freq_hz: f32, // 100..1000
    #[serde(default)]
    pub eq_mid1_gain_db: f32, // -15..15
    #[serde(default = "default_eq_mid2_freq")]
    pub eq_mid2_freq_hz: f32, // 600..6000
    #[serde(default)]
    pub eq_mid2_gain_db: f32, // -15..15
    #[serde(default)]
    pub eq_high_gain_db: f32, // -15..15

    // NAM Capture (only active while `tone_engine == ToneEngineKind::NamCapture`).
    pub nam_input_trim_db: f32,  // -24..24
    pub nam_output_trim_db: f32, // -24..24
    pub nam_mix: f32,            // 0..100 % wet
    pub nam_loudness_norm: bool,
    /// NAM A2 SlimmableContainer width dial, 0..100 % (100 = full quality).
    /// Ignored by a single WaveNet/LSTM capture.
    #[serde(default = "default_nam_slim_size")]
    pub nam_slim_size: f32,

    /// The second instance of each doublable stage (`StageKind::Drive2` and
    /// friends). Absent from a pre-v4 save; `Default` then supplies a full,
    /// legal set which no path references, so an old project loads unchanged.
    #[serde(default)]
    pub stage_b: StageBParams,
}

/// Read the saved path, accepting the shorter array an older project wrote.
///
/// `stage_order` is a fixed-size array, and serde rejects a sequence of the
/// wrong length outright. That is why each past growth of this array (7 → 9 →
/// 10) made older saves unparseable and needed a schema bump purely to signal
/// "give up and use defaults" — the user's path was lost. Reading it as a
/// sequence and padding with empty slots keeps a pre-v4 rig exactly as saved:
/// the stages it names stay in the same order, and the slots it never knew
/// about are simply empty. A *longer* array is still an error: that is a save
/// from a future build, and quietly truncating it would drop stages.
fn deserialize_stage_order<'de, D>(d: D) -> Result<[Option<StageKind>; PATH_SLOTS], D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let slots = Vec::<Option<StageKind>>::deserialize(d)?;
    if slots.len() > PATH_SLOTS {
        return Err(serde::de::Error::invalid_length(
            slots.len(),
            &"a path of at most PATH_SLOTS stages",
        ));
    }
    let mut out = [None; PATH_SLOTS];
    for (slot, saved) in out.iter_mut().zip(slots) {
        *slot = saved;
    }
    Ok(out)
}

/// Knobs, model and enable for the *second* instance of each doublable stage.
///
/// Nested rather than flattened into 29 more `*2_*` fields on [`Params`]: this
/// is one coherent thing (the B half of the path) with one `Default`, one
/// serde default and one clamp — and it leaves every A-side field, which the
/// factory presets, the wire table and the DSP tests all read by name,
/// untouched.
///
/// Field names deliberately mirror their A-side counterparts, so
/// `p.chorus_rate` and `p.stage_b.chorus_rate` are visibly the same knob on
/// two different blocks.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StageBParams {
    pub drive_on: bool,
    pub drive_model: DriveModel,
    pub drive_gain: f32,
    pub drive_tone: f32,
    pub drive_level: f32,

    pub mod_on: bool,
    pub mod_model: ModModel,
    pub chorus_rate: f32,
    pub chorus_depth: f32,
    pub chorus_mix: f32,

    pub delay_on: bool,
    pub delay_model: DelayModel,
    pub delay_time_ms: f32,
    pub delay_fb: f32,
    pub delay_mix: f32,
    pub delay_tone: f32,

    pub eq_on: bool,
    pub eq_model: EqModel,
    pub eq_low_gain_db: f32,
    pub eq_mid1_freq_hz: f32,
    pub eq_mid1_gain_db: f32,
    pub eq_mid2_freq_hz: f32,
    pub eq_mid2_gain_db: f32,
    pub eq_high_gain_db: f32,

    pub comp_on: bool,
    pub comp_thresh_db: f32,
    pub comp_ratio: f32,
    pub comp_attack_ms: f32,
    pub comp_release_ms: f32,
    pub comp_makeup_db: f32,
}

impl Default for StageBParams {
    /// The same defaults as the stage each of these doubles.
    ///
    /// It is tempting to give the second drive a boost voicing or the second
    /// delay a slapback, but the editor derives its own B defaults from the
    /// A-side tables and pushes them at insert time, so a different default
    /// here would only ever be overwritten — a value no user would ever hear,
    /// disagreeing with what the UI shows. Two sources of truth for one knob
    /// is the bug; a clever default is not worth it. Pinned by
    /// `second_instance_defaults_match_the_stage_they_double`.
    /// Written out rather than read from [`default_params`], which would
    /// recurse — it builds this struct. The test named above is what keeps the
    /// two in step.
    fn default() -> Self {
        Self {
            drive_on: true,
            drive_model: DriveModel::Screamer,
            drive_gain: 6.0,
            drive_tone: 5.5,
            drive_level: 6.5,

            mod_on: true,
            mod_model: ModModel::Chorus,
            chorus_rate: 4.0,
            chorus_depth: 5.5,
            chorus_mix: 40.0,

            delay_on: true,
            delay_model: DelayModel::Tape,
            delay_time_ms: 420.0,
            delay_fb: 35.0,
            delay_mix: 30.0,
            delay_tone: default_delay_tone(),

            eq_on: true,
            eq_model: EqModel::Studio,
            eq_low_gain_db: 0.0,
            eq_mid1_freq_hz: default_eq_mid1_freq(),
            eq_mid1_gain_db: 0.0,
            eq_mid2_freq_hz: default_eq_mid2_freq(),
            eq_mid2_gain_db: 0.0,
            eq_high_gain_db: 0.0,

            comp_on: true,
            comp_thresh_db: default_comp_thresh(),
            comp_ratio: default_comp_ratio(),
            comp_attack_ms: default_comp_attack(),
            comp_release_ms: default_comp_release(),
            comp_makeup_db: 0.0,
        }
    }
}

impl StageBParams {
    /// Same legal ranges as the A-side fields in [`Dsp::set_params`]. Kept
    /// beside the struct so the two instances of a stage can never drift into
    /// different limits.
    fn clamped(&self) -> Self {
        Self {
            drive_on: self.drive_on,
            drive_model: self.drive_model,
            drive_gain: clamp(self.drive_gain, 0.0, 10.0),
            drive_tone: clamp(self.drive_tone, 0.0, 10.0),
            drive_level: clamp(self.drive_level, 0.0, 10.0),

            mod_on: self.mod_on,
            mod_model: self.mod_model,
            chorus_rate: clamp(self.chorus_rate, 0.0, 10.0),
            chorus_depth: clamp(self.chorus_depth, 0.0, 10.0),
            chorus_mix: clamp(self.chorus_mix, 0.0, 100.0),

            delay_on: self.delay_on,
            delay_model: self.delay_model,
            delay_time_ms: clamp(self.delay_time_ms, 40.0, 1_200.0),
            delay_fb: clamp(self.delay_fb, 0.0, 100.0),
            delay_mix: clamp(self.delay_mix, 0.0, 100.0),
            delay_tone: clamp(self.delay_tone, 0.0, 10.0),

            eq_on: self.eq_on,
            eq_model: self.eq_model,
            eq_low_gain_db: clamp(self.eq_low_gain_db, -15.0, 15.0),
            eq_mid1_freq_hz: clamp(self.eq_mid1_freq_hz, 100.0, 1_000.0),
            eq_mid1_gain_db: clamp(self.eq_mid1_gain_db, -15.0, 15.0),
            eq_mid2_freq_hz: clamp(self.eq_mid2_freq_hz, 600.0, 6_000.0),
            eq_mid2_gain_db: clamp(self.eq_mid2_gain_db, -15.0, 15.0),
            eq_high_gain_db: clamp(self.eq_high_gain_db, -15.0, 15.0),

            comp_on: self.comp_on,
            comp_thresh_db: clamp(self.comp_thresh_db, -60.0, 0.0),
            comp_ratio: clamp(self.comp_ratio, 1.0, 20.0),
            comp_attack_ms: clamp(self.comp_attack_ms, 0.1, 100.0),
            comp_release_ms: clamp(self.comp_release_ms, 10.0, 1_000.0),
            comp_makeup_db: clamp(self.comp_makeup_db, 0.0, 24.0),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_delay_tone() -> f32 {
    5.0
}
fn default_reverb_shimmer() -> f32 {
    62.0
}
fn default_nam_slim_size() -> f32 {
    100.0
}
fn default_comp_thresh() -> f32 {
    -24.0
}
fn default_comp_ratio() -> f32 {
    2.0
}
fn default_comp_attack() -> f32 {
    10.0
}
fn default_comp_release() -> f32 {
    120.0
}
fn default_eq_mid1_freq() -> f32 {
    400.0
}
fn default_eq_mid2_freq() -> f32 {
    2_000.0
}
fn default_wah_pos() -> f32 {
    4.5
}
fn default_wah_res() -> f32 {
    5.0
}
fn default_wah_sens() -> f32 {
    5.0
}

/// Defaults mirror the editor's `parameterDefaults` (Mandarin patch, "06D").
pub fn default_params() -> Params {
    Params {
        power: true,
        input_trim_db: 0.0,
        output_trim_db: 0.0,
        gate_on: true,
        drive_on: true,
        amp_on: true,
        mod_on: true,
        delay_on: true,
        reverb_on: true,
        cab_on: true,
        comp_on: true,
        eq_on: true,
        wah_on: true,
        drive_model: DriveModel::Screamer,
        amp_model: AmpModel::Mandarin,
        cab_model: CabModel::Vintage4x12,
        mic_model: MicModel::Dynamic,
        mod_model: ModModel::Chorus,
        wah_model: WahModel::CryWah,
        eq_model: EqModel::Studio,
        reverb_model: ReverbModel::Plate,
        delay_model: DelayModel::Tape,
        tone_engine: ToneEngineKind::Classic,
        stage_order: StageKind::default_path(),
        gate_thresh_db: -55.0,
        drive_gain: 6.0,
        drive_tone: 5.5,
        drive_level: 6.5,
        amp_gain: 5.0,
        amp_bass: 5.0,
        amp_middle: 5.5,
        amp_treble: 5.0,
        amp_presence: 5.0,
        amp_master: 5.0,
        chorus_rate: 4.0,
        chorus_depth: 5.5,
        chorus_mix: 40.0,
        delay_time_ms: 420.0,
        delay_fb: 35.0,
        delay_mix: 30.0,
        delay_tone: default_delay_tone(),
        reverb_decay_s: 8.5,
        reverb_mix: 55.0,
        reverb_shimmer: default_reverb_shimmer(),
        cab_mic: 20.0,
        cab_dist: 40.0,
        wah_pos: default_wah_pos(),
        wah_res: default_wah_res(),
        wah_sens: default_wah_sens(),
        comp_thresh_db: default_comp_thresh(),
        comp_ratio: default_comp_ratio(),
        comp_attack_ms: default_comp_attack(),
        comp_release_ms: default_comp_release(),
        comp_makeup_db: 0.0,
        eq_low_gain_db: 0.0,
        eq_mid1_freq_hz: default_eq_mid1_freq(),
        eq_mid1_gain_db: 0.0,
        eq_mid2_freq_hz: default_eq_mid2_freq(),
        eq_mid2_gain_db: 0.0,
        eq_high_gain_db: 0.0,
        nam_input_trim_db: 0.0,
        nam_output_trim_db: 0.0,
        nam_mix: 100.0,
        nam_loudness_norm: true,
        nam_slim_size: default_nam_slim_size(),
        stage_b: StageBParams::default(),
    }
}

/// Host-facing descriptor. Exposes the headline parameters (ids match `data.ts`).
pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Rodhareist",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: &[
            ParamDescriptor {
                id: "power",
                name: "Power",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "input_trim",
                name: "Input Trim",
                default_value: 0.0,
                min: -24.0,
                max: 24.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "output_trim",
                name: "Output Trim",
                default_value: 0.0,
                min: -24.0,
                max: 24.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "gate_thresh",
                name: "Gate",
                default_value: -55.0,
                min: -80.0,
                max: 0.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "drive_gain",
                name: "Drive",
                default_value: 6.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "drive_tone",
                name: "Drive Tone",
                default_value: 5.5,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "drive_level",
                name: "Drive Level",
                default_value: 6.5,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "amp_gain",
                name: "Amp Drive",
                default_value: 5.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "amp_bass",
                name: "Bass",
                default_value: 5.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "amp_middle",
                name: "Middle",
                default_value: 5.5,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "amp_treble",
                name: "Treble",
                default_value: 5.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "amp_presence",
                name: "Presence",
                default_value: 5.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "amp_master",
                name: "Master",
                default_value: 5.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "chorus_rate",
                name: "Chorus Rate",
                default_value: 4.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "chorus_depth",
                name: "Chorus Depth",
                default_value: 5.5,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "chorus_mix",
                name: "Chorus Mix",
                default_value: 40.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "delay_time",
                name: "Delay Time",
                default_value: 420.0,
                min: 40.0,
                max: 1200.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "delay_fb",
                name: "Delay FB",
                default_value: 35.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "delay_mix",
                name: "Delay Mix",
                default_value: 30.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "delay_tone",
                name: "Delay Tone",
                default_value: 5.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "reverb_decay",
                name: "Reverb Decay",
                default_value: 8.5,
                min: 0.5,
                max: 15.0,
                unit: "s",
            },
            ParamDescriptor {
                id: "reverb_mix",
                name: "Reverb Mix",
                default_value: 55.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "reverb_shimmer",
                name: "Shimmer",
                default_value: 62.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "cab_mic",
                name: "Cab Mic",
                default_value: 20.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "cab_dist",
                name: "Cab Distance",
                default_value: 40.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "cab_mic_type",
                name: "Microphone Type",
                default_value: 0.0,
                min: 0.0,
                max: 2.0,
                unit: "type",
            },
            ParamDescriptor {
                id: "wah_pos",
                name: "Wah Position",
                default_value: 4.5,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "wah_res",
                name: "Wah Resonance",
                default_value: 5.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "wah_sens",
                name: "Wah Sensitivity",
                default_value: 5.0,
                min: 0.0,
                max: 10.0,
                unit: "",
            },
            ParamDescriptor {
                id: "comp_thresh",
                name: "Comp Threshold",
                default_value: -24.0,
                min: -60.0,
                max: 0.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "comp_ratio",
                name: "Comp Ratio",
                default_value: 2.0,
                min: 1.0,
                max: 20.0,
                unit: ":1",
            },
            ParamDescriptor {
                id: "comp_attack",
                name: "Comp Attack",
                default_value: 10.0,
                min: 0.1,
                max: 100.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "comp_release",
                name: "Comp Release",
                default_value: 120.0,
                min: 10.0,
                max: 1000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "comp_makeup",
                name: "Comp Makeup",
                default_value: 0.0,
                min: 0.0,
                max: 24.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "eq_low_gain",
                name: "EQ Low",
                default_value: 0.0,
                min: -15.0,
                max: 15.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "eq_mid1_freq",
                name: "EQ Mid1 Freq",
                default_value: 400.0,
                min: 100.0,
                max: 1000.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "eq_mid1_gain",
                name: "EQ Mid1",
                default_value: 0.0,
                min: -15.0,
                max: 15.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "eq_mid2_freq",
                name: "EQ Mid2 Freq",
                default_value: 2000.0,
                min: 600.0,
                max: 6000.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "eq_mid2_gain",
                name: "EQ Mid2",
                default_value: 0.0,
                min: -15.0,
                max: 15.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "eq_high_gain",
                name: "EQ High",
                default_value: 0.0,
                min: -15.0,
                max: 15.0,
                unit: "dB",
            },
        ],
    }
}

/// The full guitar chain.
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    gate: NoiseGate,
    comp_stage: CompStage,
    drive: Drive,
    amp_stage: ToneStage,
    eq_stage: EqStage,
    mod_stage: ModStage,
    wah: Wah,
    delay: DelayStage,
    /// Convolution cabinet, active when `cab_model == CabModel::Ir`.
    ir: ir::IrConvolver,
    /// 0 = modeled cabinet, 1 = IR convolution. Ramped so switching cabinet
    /// engines crossfades instead of cutting. The IR path carries
    /// [`ir::PARTITION`] samples of latency the modeled path does not, so the
    /// two are briefly offset in time during the fade — audible as a short
    /// blur, not a click, and gone once the fade completes.
    cab_ir_blend: f32,
    cab_ir_step: f32,
    reverb: PlateReverb,
    cab: Cabinet,
    /// Second instances of the doublable stages (`StageKind::Drive2` …). Fully
    /// independent objects, not re-runs of the first: each carries its own
    /// filter, envelope and delay-line state, so a doubled block behaves like
    /// a second pedal on the board rather than the same pedal clocked twice.
    /// Always constructed — they cost a few kB of scalar state each, and
    /// allocating one the moment a user drops the block in would mean
    /// allocating on the control thread mid-session.
    comp_b: CompStage,
    drive_b: Drive,
    eq_b: EqStage,
    mod_b: ModStage,
    delay_b: DelayStage,
    /// Linear gains derived from `input_trim_db` / `output_trim_db`, recomputed
    /// on the control thread so the audio path only multiplies.
    in_gain: smooth::Smoothed,
    out_gain: smooth::Smoothed,
    meters: Meters,
}

/// Input/output telemetry for the editor. Written from the audio thread with
/// plain scalar arithmetic (no allocation, no locking); read opportunistically
/// by the control thread, which tolerates a torn read of a stale frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// Set when a post-trim input sample reached full scale. Sticky until the
    /// editor calls [`Dsp::clear_clip`] (click-to-reset).
    pub in_clip: bool,
    /// Set when a post-trim output sample reached full scale. Sticky.
    pub out_clip: bool,
}

#[derive(Debug, Clone)]
struct Meters {
    in_peak: f32,
    out_peak: f32,
    /// Mean-square running averages; `sqrt` is deferred to the reader so the
    /// audio path stays a multiply-add.
    in_ms: f32,
    out_ms: f32,
    /// One-pole coefficient for a ~300 ms RMS window.
    rms_coeff: f32,
    in_clip: bool,
    out_clip: bool,
}

impl Meters {
    fn new(sample_rate: f32) -> Self {
        Self {
            in_peak: 0.0,
            out_peak: 0.0,
            in_ms: 0.0,
            out_ms: 0.0,
            rms_coeff: time_constant(sample_rate, 0.300),
            in_clip: false,
            out_clip: false,
        }
    }

    fn reset(&mut self) {
        self.in_peak = 0.0;
        self.out_peak = 0.0;
        self.in_ms = 0.0;
        self.out_ms = 0.0;
        self.in_clip = false;
        self.out_clip = false;
    }
}

/// Full scale. A sample at or beyond this is reported as a clip.
const CLIP_THRESHOLD: f32 = 1.0;

/// Glide time for the global input/output trims (see `smooth.rs`).
const TRIM_SMOOTH_SECONDS: f32 = 0.010;

/// Crossfade time when the Cabinet slot switches between the modeled voicings
/// and the IR convolution engine.
const CAB_ENGINE_FADE_SECONDS: f32 = 0.020;

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut dsp = Self {
            sample_rate: sr,
            params: default_params(),
            gate: NoiseGate::new(sr),
            comp_stage: CompStage::new(sr),
            drive: Drive::new(sr),
            amp_stage: ToneStage::new(sr),
            eq_stage: EqStage::new(sr),
            mod_stage: ModStage::new(sr),
            wah: Wah::new(sr),
            delay: DelayStage::new(sr),
            ir: ir::IrConvolver::new(sr),
            cab_ir_blend: 0.0,
            cab_ir_step: 1.0 / (sr * CAB_ENGINE_FADE_SECONDS).max(1.0),
            reverb: PlateReverb::new(sr),
            cab: Cabinet::new(sr),
            comp_b: CompStage::new(sr),
            drive_b: Drive::new(sr),
            eq_b: EqStage::new(sr),
            mod_b: ModStage::new(sr),
            delay_b: DelayStage::new(sr),
            in_gain: smooth::Smoothed::new(sr, TRIM_SMOOTH_SECONDS, 1.0),
            out_gain: smooth::Smoothed::new(sr, TRIM_SMOOTH_SECONDS, 1.0),
            meters: Meters::new(sr),
        };
        dsp.apply_params();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Latest input/output peak levels (0..1), for editor VU telemetry. Decays
    /// each block; never blocks the audio thread.
    pub fn meter_levels(&self) -> (f32, f32) {
        (self.meters.in_peak, self.meters.out_peak)
    }

    /// Full input/output telemetry (peak, RMS and sticky clip flags) for the
    /// editor's gain-staging display. Both levels are measured **after** the
    /// corresponding trim, so the numbers match what the chain and the host
    /// actually see.
    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    /// Clear the sticky clip indicators (editor click-to-reset).
    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    /// Replace the whole parameter set (clamps to legal ranges) and recompute.
    pub fn set_params(&mut self, params: Params) {
        self.params = Params {
            power: params.power,
            input_trim_db: clamp(params.input_trim_db, -24.0, 24.0),
            output_trim_db: clamp(params.output_trim_db, -24.0, 24.0),
            gate_on: params.gate_on,
            drive_on: params.drive_on,
            amp_on: params.amp_on,
            mod_on: params.mod_on,
            delay_on: params.delay_on,
            reverb_on: params.reverb_on,
            cab_on: params.cab_on,
            comp_on: params.comp_on,
            eq_on: params.eq_on,
            wah_on: params.wah_on,
            drive_model: params.drive_model,
            amp_model: params.amp_model,
            cab_model: params.cab_model,
            mic_model: params.mic_model,
            mod_model: params.mod_model,
            wah_model: params.wah_model,
            eq_model: params.eq_model,
            reverb_model: params.reverb_model,
            delay_model: params.delay_model,
            tone_engine: params.tone_engine,
            stage_order: sanitize_stage_order(params.stage_order),
            gate_thresh_db: clamp(params.gate_thresh_db, -80.0, 0.0),
            drive_gain: clamp(params.drive_gain, 0.0, 10.0),
            drive_tone: clamp(params.drive_tone, 0.0, 10.0),
            drive_level: clamp(params.drive_level, 0.0, 10.0),
            amp_gain: clamp(params.amp_gain, 0.0, 10.0),
            amp_bass: clamp(params.amp_bass, 0.0, 10.0),
            amp_middle: clamp(params.amp_middle, 0.0, 10.0),
            amp_treble: clamp(params.amp_treble, 0.0, 10.0),
            amp_presence: clamp(params.amp_presence, 0.0, 10.0),
            amp_master: clamp(params.amp_master, 0.0, 10.0),
            chorus_rate: clamp(params.chorus_rate, 0.0, 10.0),
            chorus_depth: clamp(params.chorus_depth, 0.0, 10.0),
            chorus_mix: clamp(params.chorus_mix, 0.0, 100.0),
            delay_time_ms: clamp(params.delay_time_ms, 40.0, 1200.0),
            delay_fb: clamp(params.delay_fb, 0.0, 100.0),
            delay_mix: clamp(params.delay_mix, 0.0, 100.0),
            delay_tone: clamp(params.delay_tone, 0.0, 10.0),
            reverb_decay_s: clamp(params.reverb_decay_s, 0.5, 15.0),
            reverb_mix: clamp(params.reverb_mix, 0.0, 100.0),
            reverb_shimmer: clamp(params.reverb_shimmer, 0.0, 100.0),
            cab_mic: clamp(params.cab_mic, 0.0, 100.0),
            cab_dist: clamp(params.cab_dist, 0.0, 100.0),
            wah_pos: clamp(params.wah_pos, 0.0, 10.0),
            wah_res: clamp(params.wah_res, 0.0, 10.0),
            wah_sens: clamp(params.wah_sens, 0.0, 10.0),
            comp_thresh_db: clamp(params.comp_thresh_db, -60.0, 0.0),
            comp_ratio: clamp(params.comp_ratio, 1.0, 20.0),
            comp_attack_ms: clamp(params.comp_attack_ms, 0.1, 100.0),
            comp_release_ms: clamp(params.comp_release_ms, 10.0, 1_000.0),
            comp_makeup_db: clamp(params.comp_makeup_db, 0.0, 24.0),
            eq_low_gain_db: clamp(params.eq_low_gain_db, -15.0, 15.0),
            eq_mid1_freq_hz: clamp(params.eq_mid1_freq_hz, 100.0, 1_000.0),
            eq_mid1_gain_db: clamp(params.eq_mid1_gain_db, -15.0, 15.0),
            eq_mid2_freq_hz: clamp(params.eq_mid2_freq_hz, 600.0, 6_000.0),
            eq_mid2_gain_db: clamp(params.eq_mid2_gain_db, -15.0, 15.0),
            eq_high_gain_db: clamp(params.eq_high_gain_db, -15.0, 15.0),
            nam_input_trim_db: clamp(params.nam_input_trim_db, -24.0, 24.0),
            nam_output_trim_db: clamp(params.nam_output_trim_db, -24.0, 24.0),
            nam_mix: clamp(params.nam_mix, 0.0, 100.0),
            nam_loudness_norm: params.nam_loudness_norm,
            nam_slim_size: clamp(params.nam_slim_size, 0.0, 100.0),
            stage_b: params.stage_b.clamped(),
        };
        self.apply_params();
    }

    /// Route a single editor parameter (`data.ts` id) to the matching field, then
    /// recompute. Also accepts stage enable ids (`gate_on`, `drive_on`, …) and
    /// model selects (`drive_model`=0/1, `amp_model`=0/1). Returns `false` for an
    /// unknown id so a bridge can flag mismatches. Control-thread only.
    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        // Not a parameter: editor's click-to-reset for the sticky clip
        // lights, routed through the same wire so no extra transport is
        // needed. Doesn't touch `Params` — see `apply_to_params`.
        if id == "clear_clip" {
            self.clear_clip();
            return true;
        }
        let mut p = self.params.clone();
        if !apply_to_params(&mut p, id, value) {
            return false;
        }
        self.set_params(p);
        true
    }

    /// Push clamped params into each stage (control-thread only).
    /// Select a stage model by editor id (`mandarin`, `screamer`, …).
    /// Control-thread only.
    pub fn select_model(&mut self, category: &str, model_id: &str) -> bool {
        let mut p = self.params.clone();
        match category {
            "amp" => match model_id {
                "bypass" => p.tone_engine = ToneEngineKind::Bypass,
                "nam_capture" => p.tone_engine = ToneEngineKind::NamCapture,
                _ => {
                    let Some(m) = AmpModel::from_model_id(model_id) else {
                        return false;
                    };
                    p.amp_model = m;
                    p.tone_engine = ToneEngineKind::Classic;
                }
            },
            "drive" => {
                let Some(m) = DriveModel::from_model_id(model_id) else {
                    return false;
                };
                p.drive_model = m;
            }
            "mod" => {
                let Some(m) = ModModel::from_model_id(model_id) else {
                    return false;
                };
                p.mod_model = m;
            }
            "wah" => {
                let Some(m) = WahModel::from_model_id(model_id) else {
                    return false;
                };
                p.wah_model = m;
            }
            "reverb" => {
                let Some(m) = ReverbModel::from_model_id(model_id) else {
                    return false;
                };
                p.reverb_model = m;
            }
            "delay" => {
                let Some(m) = DelayModel::from_model_id(model_id) else {
                    return false;
                };
                p.delay_model = m;
            }
            // Second instances. The editor sends the same model ids as the
            // stage they double, so the model lists stay one table.
            "drive2" => {
                let Some(m) = DriveModel::from_model_id(model_id) else {
                    return false;
                };
                p.stage_b.drive_model = m;
            }
            "mod2" => {
                let Some(m) = ModModel::from_model_id(model_id) else {
                    return false;
                };
                p.stage_b.mod_model = m;
            }
            "delay2" => {
                let Some(m) = DelayModel::from_model_id(model_id) else {
                    return false;
                };
                p.stage_b.delay_model = m;
            }
            // Single-algorithm stages: accept their canonical model ids.
            "gate" if model_id == "gate" => {}
            "comp" if model_id == "softknee" => {}
            "eq" if model_id == "parametric" => {}
            "comp2" if model_id == "softknee" => {}
            "eq2" if model_id == "parametric" => {}
            "cab" => {
                let Some(m) = CabModel::from_model_id(model_id) else {
                    return false;
                };
                p.cab_model = m;
            }
            _ => return false,
        }
        self.params = p;
        self.apply_params();
        true
    }

    fn apply_params(&mut self) {
        let p = &self.params;
        self.in_gain.set_target(db_to_linear(p.input_trim_db));
        self.out_gain.set_target(db_to_linear(p.output_trim_db));
        self.gate.set_threshold_db(p.gate_thresh_db);
        self.comp_stage.configure(
            p.comp_thresh_db,
            p.comp_ratio,
            p.comp_attack_ms,
            p.comp_release_ms,
            p.comp_makeup_db,
        );
        self.eq_stage.configure(
            p.eq_model,
            p.eq_low_gain_db,
            p.eq_mid1_freq_hz,
            p.eq_mid1_gain_db,
            p.eq_mid2_freq_hz,
            p.eq_mid2_gain_db,
            p.eq_high_gain_db,
        );
        self.drive
            .configure(p.drive_model, p.drive_gain, p.drive_tone, p.drive_level);
        self.amp_stage.set_engine(p.tone_engine);
        self.amp_stage.configure_classic(
            p.amp_model,
            p.amp_gain,
            p.amp_bass,
            p.amp_middle,
            p.amp_treble,
            p.amp_presence,
            p.amp_master,
        );
        self.amp_stage.configure_nam(
            p.nam_input_trim_db,
            p.nam_output_trim_db,
            p.nam_mix,
            p.nam_loudness_norm,
            p.nam_slim_size / 100.0,
        );
        self.mod_stage
            .configure(p.mod_model, p.chorus_rate, p.chorus_depth, p.chorus_mix);
        self.wah
            .configure(p.wah_model, p.wah_pos, p.wah_res, p.wah_sens);
        self.delay.configure(
            p.delay_model,
            p.delay_time_ms,
            p.delay_fb,
            p.delay_mix,
            p.delay_tone,
        );
        self.reverb.configure(
            p.reverb_model,
            p.reverb_decay_s,
            p.reverb_mix,
            p.reverb_shimmer,
        );
        self.cab
            .configure(p.cab_model, p.mic_model, p.cab_mic, p.cab_dist);

        // Second instances. Same `configure` calls against the same stage
        // types — the only difference is which params block feeds them.
        let b = &p.stage_b;
        self.comp_b.configure(
            b.comp_thresh_db,
            b.comp_ratio,
            b.comp_attack_ms,
            b.comp_release_ms,
            b.comp_makeup_db,
        );
        self.drive_b
            .configure(b.drive_model, b.drive_gain, b.drive_tone, b.drive_level);
        self.eq_b.configure(
            b.eq_model,
            b.eq_low_gain_db,
            b.eq_mid1_freq_hz,
            b.eq_mid1_gain_db,
            b.eq_mid2_freq_hz,
            b.eq_mid2_gain_db,
            b.eq_high_gain_db,
        );
        self.mod_b
            .configure(b.mod_model, b.chorus_rate, b.chorus_depth, b.chorus_mix);
        self.delay_b.configure(
            b.delay_model,
            b.delay_time_ms,
            b.delay_fb,
            b.delay_mix,
            b.delay_tone,
        );
    }

    /// Replace the Helix path order (control thread).
    pub fn set_path_order(&mut self, order: [Option<StageKind>; PATH_SLOTS]) {
        self.params.stage_order = sanitize_stage_order(order);
    }

    /// Audio thread: call once per audio block, before the block's per-sample
    /// [`StereoEffect::process_stereo`] calls. This is the only place a
    /// pending NAM capture swap (queued by [`Dsp::load_nam_capture_json`] on
    /// the control thread) is adopted — never mid-block, never per-sample.
    pub fn begin_block(&mut self) {
        self.amp_stage.begin_block();
        self.ir.begin_block();
    }

    /// Control thread: parse and build a `.nam` capture, then queue it for the
    /// audio thread to adopt at the next [`Dsp::begin_block`]. Rejects a
    /// sample-rate mismatch (nam-rs does not resample) rather than silently
    /// mis-running. `stereo` builds two independent models (true stereo
    /// width); otherwise one model's output is mirrored to both channels.
    /// `full_rig` marks the capture as already modeling amp + cab + mic, so
    /// the host/UI can offer an explicit "Bypass Cab" action.
    pub fn load_nam_capture_json(
        &mut self,
        json: &str,
        name: impl Into<String>,
        stereo: bool,
        full_rig: bool,
    ) -> Result<NamCaptureInfo, NamLoadError> {
        let prepared =
            nam::prepare_nam_runtime(json, name.into(), self.sample_rate as f64, stereo, full_rig)?;
        let info = prepared.info();
        // Opportunistic sweep: drop whatever the audio thread has already
        // retired before handing off the new one.
        self.amp_stage.poll_nam_garbage();
        self.amp_stage.submit_nam_runtime(Box::new(prepared));
        Ok(info)
    }

    /// Control thread: drop any NAM capture the audio thread has retired.
    /// Safe to call periodically (e.g. an idle/UI timer/poll) even when
    /// nothing is pending.
    pub fn poll_nam_garbage(&mut self) {
        self.amp_stage.poll_nam_garbage();
    }

    /// Clone out a control-side NAM loader handle. Lets a control thread in a
    /// different ownership domain (the plugin-host IPC thread) prepare and
    /// submit captures without touching this `Dsp` — the audio side adopts at
    /// the next [`Dsp::begin_block`]. One control thread at a time.
    pub fn nam_loader(&self) -> NamLoader {
        self.amp_stage.nam_loader()
    }

    /// Info about the currently active NAM capture, if the Tone/Amp slot has
    /// one loaded (regardless of whether `NamCapture` is the active engine).
    pub fn nam_capture_info(&self) -> Option<NamCaptureInfo> {
        self.amp_stage.nam_capture_info()
    }

    /// Latency the NAM engine adds, in engine samples: the block it buffers
    /// for inference plus, for a capture running at a different rate, the rate
    /// adapter's interpolation delay.
    ///
    /// **Not** the receptive field. A WaveNet's dilated convolutions are
    /// causal, so the field is history the model needs warmed — which
    /// `prepare_nam_runtime` does at load — rather than a delay it imposes.
    /// Reporting it as latency put a stock capture 85 ms behind the dry
    /// signal.
    pub fn nam_latency_samples(&self) -> usize {
        self.amp_stage.nam_latency_samples()
    }

    /// Control thread: decode and build a `.wav` impulse response, then queue
    /// it for the audio thread to adopt at the next [`Dsp::begin_block`]. The
    /// IR only *sounds* once the Cabinet slot is switched to
    /// [`CabModel::Ir`] — loading and selecting are deliberately separate, so
    /// a user can audition a file against the modeled cabinet.
    pub fn load_ir_wav(
        &mut self,
        bytes: &[u8],
        name: impl Into<String>,
    ) -> Result<IrInfo, IrLoadError> {
        let prepared = ir::prepare_ir_runtime(bytes, name.into(), self.sample_rate as f64)?;
        let info = prepared.info();
        // Opportunistic sweep before handing off the new one.
        self.ir.poll_garbage();
        self.ir.submit(Box::new(prepared));
        Ok(info)
    }

    /// Control thread: drop any IR the audio thread has retired.
    pub fn poll_ir_garbage(&mut self) {
        self.ir.poll_garbage();
    }

    /// Clone out a control-side IR loader handle, for a control thread in a
    /// different ownership domain (the plugin-host IPC thread). One control
    /// thread at a time.
    pub fn ir_loader(&self) -> IrLoader {
        self.ir.loader()
    }

    /// Info about the currently active IR, if one is loaded (regardless of
    /// whether the Cabinet slot has the IR engine selected).
    pub fn ir_info(&self) -> Option<IrInfo> {
        self.ir.active_info()
    }

    /// Total latency this instance reports to the host, in samples: the NAM
    /// engine's inference block plus the IR convolution's partition, counting
    /// each only while the stage that carries it is actually in the path.
    pub fn latency_samples(&self) -> usize {
        let p = &self.params;
        let in_path = |kind: StageKind| p.stage_order.iter().flatten().any(|s| *s == kind);
        let nam =
            if p.amp_on && in_path(StageKind::Amp) && p.tone_engine == ToneEngineKind::NamCapture {
                self.nam_latency_samples()
            } else {
                0
            };
        let ir = if p.cab_on && in_path(StageKind::Cab) && p.cab_model == CabModel::Ir {
            self.ir.latency_samples()
        } else {
            0
        };
        nam + ir
    }
}

/// Pure `Params`-level routing for a single editor parameter — the field
/// mutation half of [`Dsp::apply_ui_param`], usable without a `Dsp` (the main
/// process keeps an authoritative per-insert `Params` mirror this way).
/// Returns `false` for unknown ids, including `clear_clip` (an action, not a
/// `Params` field — only a live `Dsp` can service it). Values are raw editor
/// units; clamping happens when the params are pushed into a `Dsp` via
/// `set_params`.
pub fn apply_to_params(p: &mut Params, id: &str, value: f32) -> bool {
    let on = value >= 0.5;
    match id {
        "power" => p.power = on,
        "input_trim" => p.input_trim_db = value,
        "output_trim" => p.output_trim_db = value,
        "gate_on" => p.gate_on = on,
        "drive_on" => p.drive_on = on,
        "amp_on" => p.amp_on = on,
        "mod_on" => p.mod_on = on,
        "delay_on" => p.delay_on = on,
        "reverb_on" => p.reverb_on = on,
        "cab_on" => p.cab_on = on,
        "comp_on" => p.comp_on = on,
        "eq_on" => p.eq_on = on,
        "wah_on" => p.wah_on = on,
        "drive_model" => p.drive_model = DriveModel::from_index(value.round() as u32),
        "mod_model" => p.mod_model = ModModel::from_index(value.round() as u32),
        "wah_model" => p.wah_model = WahModel::from_index(value.round() as u32),
        "eq_model" => p.eq_model = EqModel::from_index(value.round() as u32),
        "reverb_model" => p.reverb_model = ReverbModel::from_index(value.round() as u32),
        "delay_model" => p.delay_model = DelayModel::from_index(value.round() as u32),
        "amp_model" => {
            p.amp_model = AmpModel::from_index(value.round() as u32);
            p.tone_engine = ToneEngineKind::Classic;
        }
        "cab_model" => p.cab_model = CabModel::from_index(value.round() as u32),
        "cab_mic_type" => p.mic_model = MicModel::from_index(value.round() as u32),
        "tone_engine" => p.tone_engine = ToneEngineKind::from_index(value.round() as u32),
        "path_slot_0" => p.stage_order[0] = StageKind::from_index(value.round() as i32),
        "path_slot_1" => p.stage_order[1] = StageKind::from_index(value.round() as i32),
        "path_slot_2" => p.stage_order[2] = StageKind::from_index(value.round() as i32),
        "path_slot_3" => p.stage_order[3] = StageKind::from_index(value.round() as i32),
        "path_slot_4" => p.stage_order[4] = StageKind::from_index(value.round() as i32),
        "path_slot_5" => p.stage_order[5] = StageKind::from_index(value.round() as i32),
        "path_slot_6" => p.stage_order[6] = StageKind::from_index(value.round() as i32),
        "path_slot_7" => p.stage_order[7] = StageKind::from_index(value.round() as i32),
        "path_slot_8" => p.stage_order[8] = StageKind::from_index(value.round() as i32),
        "path_slot_9" => p.stage_order[9] = StageKind::from_index(value.round() as i32),
        "path_slot_10" => p.stage_order[10] = StageKind::from_index(value.round() as i32),
        "path_slot_11" => p.stage_order[11] = StageKind::from_index(value.round() as i32),
        "path_slot_12" => p.stage_order[12] = StageKind::from_index(value.round() as i32),
        "path_slot_13" => p.stage_order[13] = StageKind::from_index(value.round() as i32),
        "path_slot_14" => p.stage_order[14] = StageKind::from_index(value.round() as i32),
        "gate_thresh" => p.gate_thresh_db = value,
        "drive_gain" => p.drive_gain = value,
        "drive_tone" => p.drive_tone = value,
        "drive_level" => p.drive_level = value,
        "amp_gain" => p.amp_gain = value,
        "amp_bass" => p.amp_bass = value,
        "amp_middle" => p.amp_middle = value,
        "amp_treble" => p.amp_treble = value,
        "amp_presence" => p.amp_presence = value,
        "amp_master" => p.amp_master = value,
        "chorus_rate" => p.chorus_rate = value,
        "chorus_depth" => p.chorus_depth = value,
        "chorus_mix" => p.chorus_mix = value,
        "delay_time" => p.delay_time_ms = value,
        "delay_fb" => p.delay_fb = value,
        "delay_mix" => p.delay_mix = value,
        "delay_tone" => p.delay_tone = value,
        "reverb_decay" => p.reverb_decay_s = value,
        "reverb_mix" => p.reverb_mix = value,
        "reverb_shimmer" => p.reverb_shimmer = value,
        "cab_mic" => p.cab_mic = value,
        "cab_dist" => p.cab_dist = value,
        "wah_pos" => p.wah_pos = value,
        "wah_res" => p.wah_res = value,
        "wah_sens" => p.wah_sens = value,
        "comp_thresh" => p.comp_thresh_db = value,
        "comp_ratio" => p.comp_ratio = value,
        "comp_attack" => p.comp_attack_ms = value,
        "comp_release" => p.comp_release_ms = value,
        "comp_makeup" => p.comp_makeup_db = value,
        "eq_low_gain" => p.eq_low_gain_db = value,
        "eq_mid1_freq" => p.eq_mid1_freq_hz = value,
        "eq_mid1_gain" => p.eq_mid1_gain_db = value,
        "eq_mid2_freq" => p.eq_mid2_freq_hz = value,
        "eq_mid2_gain" => p.eq_mid2_gain_db = value,
        "eq_high_gain" => p.eq_high_gain_db = value,
        "nam_input_trim" => p.nam_input_trim_db = value,
        "nam_output_trim" => p.nam_output_trim_db = value,
        "nam_mix" => p.nam_mix = value,
        "nam_loudness_norm" => p.nam_loudness_norm = on,
        "nam_slim_size" => p.nam_slim_size = value,

        // Second instances. Same knob names with the stage's `2` suffix, so an
        // id reads as "which block" then "which knob" — `chorus2_rate` is the
        // Rate on the second Mod block.
        "drive2_on" => p.stage_b.drive_on = on,
        "drive2_model" => p.stage_b.drive_model = DriveModel::from_index(value.round() as u32),
        "drive2_gain" => p.stage_b.drive_gain = value,
        "drive2_tone" => p.stage_b.drive_tone = value,
        "drive2_level" => p.stage_b.drive_level = value,
        "mod2_on" => p.stage_b.mod_on = on,
        "mod2_model" => p.stage_b.mod_model = ModModel::from_index(value.round() as u32),
        "chorus2_rate" => p.stage_b.chorus_rate = value,
        "chorus2_depth" => p.stage_b.chorus_depth = value,
        "chorus2_mix" => p.stage_b.chorus_mix = value,
        "delay2_on" => p.stage_b.delay_on = on,
        "delay2_model" => p.stage_b.delay_model = DelayModel::from_index(value.round() as u32),
        "delay2_time" => p.stage_b.delay_time_ms = value,
        "delay2_fb" => p.stage_b.delay_fb = value,
        "delay2_mix" => p.stage_b.delay_mix = value,
        "delay2_tone" => p.stage_b.delay_tone = value,
        "eq2_on" => p.stage_b.eq_on = on,
        "eq2_model" => p.stage_b.eq_model = EqModel::from_index(value.round() as u32),
        "eq2_low_gain" => p.stage_b.eq_low_gain_db = value,
        "eq2_mid1_freq" => p.stage_b.eq_mid1_freq_hz = value,
        "eq2_mid1_gain" => p.stage_b.eq_mid1_gain_db = value,
        "eq2_mid2_freq" => p.stage_b.eq_mid2_freq_hz = value,
        "eq2_mid2_gain" => p.stage_b.eq_mid2_gain_db = value,
        "eq2_high_gain" => p.stage_b.eq_high_gain_db = value,
        "comp2_on" => p.stage_b.comp_on = on,
        "comp2_thresh" => p.stage_b.comp_thresh_db = value,
        "comp2_ratio" => p.stage_b.comp_ratio = value,
        "comp2_attack" => p.stage_b.comp_attack_ms = value,
        "comp2_release" => p.stage_b.comp_release_ms = value,
        "comp2_makeup" => p.stage_b.comp_makeup_db = value,

        _ => return false,
    }
    true
}

/// Enumerate a `Params` as `(ui id, raw value)` pairs covering every wire id
/// except `clear_clip` (an action, not state). Replaying the returned list
/// through [`apply_to_params`] / `Dsp::apply_ui_param` in order reproduces the
/// source — `tone_engine` is deliberately emitted *after* `amp_model`, since
/// applying `amp_model` resets `tone_engine` to Classic.
pub fn ui_values(p: &Params) -> Vec<(&'static str, f32)> {
    fn b(v: bool) -> f32 {
        if v { 1.0 } else { 0.0 }
    }
    fn model_index<T: PartialEq + Copy>(all: &[T], value: T) -> f32 {
        all.iter().position(|m| *m == value).unwrap_or(0) as f32
    }
    let mut out = Vec::with_capacity(crate::wire::UI_PARAM_IDS.len());
    out.push(("power", b(p.power)));
    out.push(("input_trim", p.input_trim_db));
    out.push(("output_trim", p.output_trim_db));
    out.push(("gate_on", b(p.gate_on)));
    out.push(("drive_on", b(p.drive_on)));
    out.push(("amp_on", b(p.amp_on)));
    out.push(("mod_on", b(p.mod_on)));
    out.push(("delay_on", b(p.delay_on)));
    out.push(("reverb_on", b(p.reverb_on)));
    out.push(("cab_on", b(p.cab_on)));
    out.push(("comp_on", b(p.comp_on)));
    out.push(("eq_on", b(p.eq_on)));
    out.push(("wah_on", b(p.wah_on)));
    out.push(("drive_model", model_index(DriveModel::ALL, p.drive_model)));
    // amp_model before tone_engine (see doc comment).
    out.push(("amp_model", model_index(AmpModel::ALL, p.amp_model)));
    out.push(("cab_model", model_index(CabModel::ALL, p.cab_model)));
    out.push(("mod_model", model_index(ModModel::ALL, p.mod_model)));
    out.push(("wah_model", model_index(WahModel::ALL, p.wah_model)));
    out.push(("eq_model", model_index(EqModel::ALL, p.eq_model)));
    out.push((
        "reverb_model",
        model_index(ReverbModel::ALL, p.reverb_model),
    ));
    out.push(("delay_model", model_index(DelayModel::ALL, p.delay_model)));
    out.push(("tone_engine", p.tone_engine.index() as f32));
    const PATH_SLOT_IDS: [&str; PATH_SLOTS] = [
        "path_slot_0",
        "path_slot_1",
        "path_slot_2",
        "path_slot_3",
        "path_slot_4",
        "path_slot_5",
        "path_slot_6",
        "path_slot_7",
        "path_slot_8",
        "path_slot_9",
        "path_slot_10",
        "path_slot_11",
        "path_slot_12",
        "path_slot_13",
        "path_slot_14",
    ];
    for (i, id) in PATH_SLOT_IDS.iter().enumerate() {
        out.push((
            id,
            p.stage_order[i].map(|s| s.index() as f32).unwrap_or(-1.0),
        ));
    }
    out.push(("gate_thresh", p.gate_thresh_db));
    out.push(("drive_gain", p.drive_gain));
    out.push(("drive_tone", p.drive_tone));
    out.push(("drive_level", p.drive_level));
    out.push(("amp_gain", p.amp_gain));
    out.push(("amp_bass", p.amp_bass));
    out.push(("amp_middle", p.amp_middle));
    out.push(("amp_treble", p.amp_treble));
    out.push(("amp_presence", p.amp_presence));
    out.push(("amp_master", p.amp_master));
    out.push(("chorus_rate", p.chorus_rate));
    out.push(("chorus_depth", p.chorus_depth));
    out.push(("chorus_mix", p.chorus_mix));
    out.push(("delay_time", p.delay_time_ms));
    out.push(("delay_fb", p.delay_fb));
    out.push(("delay_mix", p.delay_mix));
    out.push(("delay_tone", p.delay_tone));
    out.push(("reverb_decay", p.reverb_decay_s));
    out.push(("reverb_mix", p.reverb_mix));
    out.push(("reverb_shimmer", p.reverb_shimmer));
    out.push(("cab_mic", p.cab_mic));
    out.push(("cab_dist", p.cab_dist));
    out.push(("cab_mic_type", p.mic_model.index() as f32));
    out.push(("wah_pos", p.wah_pos));
    out.push(("wah_res", p.wah_res));
    out.push(("wah_sens", p.wah_sens));
    out.push(("comp_thresh", p.comp_thresh_db));
    out.push(("comp_ratio", p.comp_ratio));
    out.push(("comp_attack", p.comp_attack_ms));
    out.push(("comp_release", p.comp_release_ms));
    out.push(("comp_makeup", p.comp_makeup_db));
    out.push(("eq_low_gain", p.eq_low_gain_db));
    out.push(("eq_mid1_freq", p.eq_mid1_freq_hz));
    out.push(("eq_mid1_gain", p.eq_mid1_gain_db));
    out.push(("eq_mid2_freq", p.eq_mid2_freq_hz));
    out.push(("eq_mid2_gain", p.eq_mid2_gain_db));
    out.push(("eq_high_gain", p.eq_high_gain_db));
    out.push(("nam_input_trim", p.nam_input_trim_db));
    out.push(("nam_output_trim", p.nam_output_trim_db));
    out.push(("nam_mix", p.nam_mix));
    out.push(("nam_loudness_norm", b(p.nam_loudness_norm)));
    out.push(("nam_slim_size", p.nam_slim_size));

    let sb = &p.stage_b;
    out.push(("drive2_on", b(sb.drive_on)));
    out.push(("drive2_model", model_index(DriveModel::ALL, sb.drive_model)));
    out.push(("drive2_gain", sb.drive_gain));
    out.push(("drive2_tone", sb.drive_tone));
    out.push(("drive2_level", sb.drive_level));
    out.push(("mod2_on", b(sb.mod_on)));
    out.push(("mod2_model", model_index(ModModel::ALL, sb.mod_model)));
    out.push(("chorus2_rate", sb.chorus_rate));
    out.push(("chorus2_depth", sb.chorus_depth));
    out.push(("chorus2_mix", sb.chorus_mix));
    out.push(("delay2_on", b(sb.delay_on)));
    out.push(("delay2_model", model_index(DelayModel::ALL, sb.delay_model)));
    out.push(("delay2_time", sb.delay_time_ms));
    out.push(("delay2_fb", sb.delay_fb));
    out.push(("delay2_mix", sb.delay_mix));
    out.push(("delay2_tone", sb.delay_tone));
    out.push(("eq2_on", b(sb.eq_on)));
    out.push(("eq2_model", model_index(EqModel::ALL, sb.eq_model)));
    out.push(("eq2_low_gain", sb.eq_low_gain_db));
    out.push(("eq2_mid1_freq", sb.eq_mid1_freq_hz));
    out.push(("eq2_mid1_gain", sb.eq_mid1_gain_db));
    out.push(("eq2_mid2_freq", sb.eq_mid2_freq_hz));
    out.push(("eq2_mid2_gain", sb.eq_mid2_gain_db));
    out.push(("eq2_high_gain", sb.eq_high_gain_db));
    out.push(("comp2_on", b(sb.comp_on)));
    out.push(("comp2_thresh", sb.comp_thresh_db));
    out.push(("comp2_ratio", sb.comp_ratio));
    out.push(("comp2_attack", sb.comp_attack_ms));
    out.push(("comp2_release", sb.comp_release_ms));
    out.push(("comp2_makeup", sb.comp_makeup_db));
    out
}

/// Keep the first occurrence of each stage and clear later duplicates —
/// **without** packing slots left. Positions must be preserved: the editor
/// (and native state replay) delivers a path as sequential `path_slot_i`
/// writes, and packing after each write would drag later stages into
/// just-cleared slots, silently re-adding stages the user removed. Empty
/// slots are simply skipped by the process loop.
fn sanitize_stage_order(order: [Option<StageKind>; PATH_SLOTS]) -> [Option<StageKind>; PATH_SLOTS] {
    let mut out = [None; PATH_SLOTS];
    // Indexed by `StageKind::index()`, which is a discriminant — not a slot
    // number. The two happen to have the same count today; do not conflate them.
    let mut seen = [false; StageKind::COUNT];
    for (i, slot) in order.into_iter().enumerate() {
        let Some(stage) = slot else { continue };
        let idx = stage.index() as usize;
        if seen[idx] {
            continue;
        }
        seen[idx] = true;
        out[i] = Some(stage);
    }
    out
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.gate.reset();
        self.comp_stage.reset();
        self.drive.reset();
        self.amp_stage.reset();
        self.eq_stage.reset();
        self.mod_stage.reset();
        self.wah.reset();
        self.delay.reset();
        self.reverb.reset();
        self.cab.reset();
        self.ir.reset();
        self.comp_b.reset();
        self.drive_b.reset();
        self.eq_b.reset();
        self.mod_b.reset();
        self.delay_b.reset();
        self.meters.reset();
        self.in_gain.snap();
        self.out_gain.snap();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        self.in_gain.set_time(sr, TRIM_SMOOTH_SECONDS);
        self.out_gain.set_time(sr, TRIM_SMOOTH_SECONDS);
        self.gate.set_sample_rate(sr);
        self.comp_stage.set_sample_rate(sr);
        self.drive.set_sample_rate(sr);
        self.amp_stage.set_sample_rate(sr);
        self.eq_stage.set_sample_rate(sr);
        self.mod_stage.set_sample_rate(sr);
        self.wah.set_sample_rate(sr);
        self.delay.set_sample_rate(sr);
        self.reverb.set_sample_rate(sr);
        self.cab.set_sample_rate(sr);
        self.ir.set_sample_rate(sr);
        self.comp_b.set_sample_rate(sr);
        self.drive_b.set_sample_rate(sr);
        self.eq_b.set_sample_rate(sr);
        self.mod_b.set_sample_rate(sr);
        self.delay_b.set_sample_rate(sr);
        self.cab_ir_step = 1.0 / (sr * CAB_ENGINE_FADE_SECONDS).max(1.0);
        self.meters.rms_coeff = time_constant(sr, 0.300);
        self.apply_params();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.params.power {
            return (left, right);
        }

        // Input trim first: everything downstream — including a NAM capture,
        // whose gain and voicing depend on the level it is fed — sees the
        // trimmed signal, so that is also what the input meter must report.
        let in_gain = self.in_gain.tick();
        let (mut l, mut r) = (left * in_gain, right * in_gain);

        let in_level = l.abs().max(r.abs());
        self.meters.in_peak = in_level.max(self.meters.in_peak - self.meters.in_peak * 0.0005);
        self.meters.in_ms =
            in_level * in_level + self.meters.rms_coeff * (self.meters.in_ms - in_level * in_level);
        if in_level >= CLIP_THRESHOLD {
            self.meters.in_clip = true;
        }

        for slot in self.params.stage_order {
            let Some(stage) = slot else { continue };
            match stage {
                StageKind::Gate if self.params.gate_on => {
                    (l, r) = self.gate.process(l, r);
                }
                StageKind::Drive if self.params.drive_on => {
                    (l, r) = self.drive.process(l, r);
                }
                StageKind::Amp if self.params.amp_on => {
                    (l, r) = self.amp_stage.process(l, r);
                }
                StageKind::Mod if self.params.mod_on => {
                    (l, r) = self.mod_stage.process(l, r);
                }
                StageKind::Wah if self.params.wah_on => {
                    (l, r) = self.wah.process(l, r);
                }
                StageKind::Delay if self.params.delay_on => {
                    (l, r) = self.delay.process(l, r);
                }
                StageKind::Reverb if self.params.reverb_on => {
                    (l, r) = self.reverb.process(l, r);
                }
                StageKind::Cab if self.params.cab_on => {
                    // Two cabinet engines behind one slot: the modeled
                    // voicings and the IR convolution. Only the selected one
                    // runs once the crossfade has resolved.
                    let target = if self.params.cab_model == CabModel::Ir {
                        1.0
                    } else {
                        0.0
                    };
                    self.cab_ir_blend = if self.cab_ir_blend < target {
                        (self.cab_ir_blend + self.cab_ir_step).min(target)
                    } else {
                        (self.cab_ir_blend - self.cab_ir_step).max(target)
                    };
                    let blend = self.cab_ir_blend;
                    (l, r) = if blend <= 0.0 {
                        self.cab.process(l, r)
                    } else if blend >= 1.0 {
                        self.ir.process(l, r)
                    } else {
                        let modeled = self.cab.process(l, r);
                        let convolved = self.ir.process(l, r);
                        let modeled_gain = (1.0 - blend).sqrt();
                        let ir_gain = blend.sqrt();
                        (
                            modeled.0 * modeled_gain + convolved.0 * ir_gain,
                            modeled.1 * modeled_gain + convolved.1 * ir_gain,
                        )
                    };
                }
                StageKind::Comp if self.params.comp_on => {
                    (l, r) = self.comp_stage.process(l, r);
                }
                StageKind::Eq if self.params.eq_on => {
                    (l, r) = self.eq_stage.process(l, r);
                }
                StageKind::Drive2 if self.params.stage_b.drive_on => {
                    (l, r) = self.drive_b.process(l, r);
                }
                StageKind::Mod2 if self.params.stage_b.mod_on => {
                    (l, r) = self.mod_b.process(l, r);
                }
                StageKind::Delay2 if self.params.stage_b.delay_on => {
                    (l, r) = self.delay_b.process(l, r);
                }
                StageKind::Eq2 if self.params.stage_b.eq_on => {
                    (l, r) = self.eq_b.process(l, r);
                }
                StageKind::Comp2 if self.params.stage_b.comp_on => {
                    (l, r) = self.comp_b.process(l, r);
                }
                _ => {}
            }
        }

        // Guard against denormals / NaNs escaping into the engine.
        if !l.is_finite() {
            l = 0.0;
        }
        if !r.is_finite() {
            r = 0.0;
        }

        let out_gain = self.out_gain.tick();
        l *= out_gain;
        r *= out_gain;

        let out_level = l.abs().max(r.abs());
        self.meters.out_peak = out_level.max(self.meters.out_peak - self.meters.out_peak * 0.0005);
        self.meters.out_ms = out_level * out_level
            + self.meters.rms_coeff * (self.meters.out_ms - out_level * out_level);
        if out_level >= CLIP_THRESHOLD {
            self.meters.out_clip = true;
        }
        (l, r)
    }
}

// ---------------------------------------------------------------------------
// Shared realtime-safe building blocks used by the stage modules.
// ---------------------------------------------------------------------------

use biquad::Coefficients;

/// One channel's direct-form-I state.
///
/// Spelled out here rather than taken from `biquad::DirectForm1` for one
/// reason: the feedback taps have to be reachable so [`flush_denormal`] can
/// snap them to zero as a tail decays. A biquad whose `y1`/`y2` slide into the
/// subnormal range keeps running — just one to two orders of magnitude slower
/// — and this chain has dozens of them. The arithmetic is otherwise identical.
#[derive(Debug, Clone, Copy, Default)]
struct DirectForm1 {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl DirectForm1 {
    #[inline]
    fn run(&mut self, coefficients: &Coefficients<f32>, x: f32) -> f32 {
        let y = coefficients.b0 * x + coefficients.b1 * self.x1 + coefficients.b2 * self.x2
            - coefficients.a1 * self.y1
            - coefficients.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = flush_denormal(y);
        y
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// A biquad with independent left/right state but shared coefficients, so a
/// stereo stage filters each channel correctly (a single instance cannot serve
/// both channels — its state would be stepped at twice the rate).
#[derive(Debug, Clone, Default)]
pub(crate) struct StereoBiquad {
    coefficients: Option<Coefficients<f32>>,
    left: DirectForm1,
    right: DirectForm1,
}

impl StereoBiquad {
    pub(crate) fn none() -> Self {
        Self::default()
    }

    /// Retune (or clear) the filter for both channels, **keeping its running
    /// history**.
    ///
    /// Every stage's `configure` re-sets all of its filters, and `configure`
    /// runs on any parameter change — so installing a fresh `DirectForm1` here
    /// would zero x1/x2/y1/y2 mid-signal on every knob edit. That step is
    /// audible on its own, and ahead of a drive model's 30–40 dB of pre-gain it
    /// can flip the clipped sign near a zero crossing, which reads as a click
    /// while a knob is dragged or automated. `update_coefficients` retunes in
    /// place instead, so a swap is continuous.
    ///
    /// Going from a filter to `None` still steps — that is a real topology
    /// change (a band switching to true bypass), not a retune.
    pub(crate) fn set(&mut self, coefficients: Option<Coefficients<f32>>) {
        let had_filter = self.coefficients.is_some();
        self.coefficients = coefficients;
        if !had_filter {
            // First install: there is no history to carry over.
            self.reset();
        }
    }

    pub(crate) fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
    }

    #[inline]
    pub(crate) fn run(&mut self, left: f32, right: f32) -> (f32, f32) {
        match self.coefficients.as_ref() {
            Some(c) => (self.left.run(c, left), self.right.run(c, right)),
            None => (left, right),
        }
    }
}

/// A fractional-read circular delay line (preallocated). Used by chorus and the
/// tape delay for modulated / interpolated taps.
#[derive(Debug, Clone)]
pub(crate) struct InterpDelay {
    buffer: Vec<f32>,
    write: usize,
}

impl InterpDelay {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![0.0; capacity.max(2)],
            write: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }

    #[inline]
    pub(crate) fn write_sample(&mut self, sample: f32) {
        self.buffer[self.write] = sample;
        self.write += 1;
        if self.write >= self.buffer.len() {
            self.write = 0;
        }
    }

    /// Linearly-interpolated read `delay_samples` behind the write head.
    #[inline]
    pub(crate) fn read_interp(&self, delay_samples: f32) -> f32 {
        let len = self.buffer.len();
        let max_delay = (len - 2) as f32;
        let d = delay_samples.clamp(1.0, max_delay);
        let mut read_pos = self.write as f32 - d;
        while read_pos < 0.0 {
            read_pos += len as f32;
        }
        let base = read_pos.floor();
        let frac = read_pos - base;
        let i0 = base as usize % len;
        let i1 = (i0 + 1) % len;
        self.buffer[i0] * (1.0 - frac) + self.buffer[i1] * frac
    }
}

/// A minimal sine LFO with a stable per-sample increment.
#[derive(Debug, Clone)]
pub(crate) struct Lfo {
    phase: f32,
    increment: f32,
}

impl Lfo {
    pub(crate) fn new() -> Self {
        Self {
            phase: 0.0,
            increment: 0.0,
        }
    }

    pub(crate) fn set_rate(&mut self, rate_hz: f32, sample_rate: f32) {
        self.increment = (rate_hz.max(0.0) / sample_rate.max(1.0)).min(0.5);
    }

    pub(crate) fn set_phase(&mut self, phase01: f32) {
        self.phase = phase01.rem_euclid(1.0);
    }

    /// Advance and return a sine in [-1, 1].
    #[inline]
    pub(crate) fn tick(&mut self) -> f32 {
        let (left, _) = self.tick_stereo(0.0);
        left
    }

    /// Advance once and return linked L/R sines. Using one phase accumulator
    /// keeps the stereo offset locked — two independent LFOs can drift and
    /// read as a doubled / stuttering image under slow rates.
    #[inline]
    pub(crate) fn tick_stereo(&mut self, spread01: f32) -> (f32, f32) {
        let left = (self.phase * std::f32::consts::TAU).sin();
        let right = ((self.phase + spread01) * std::f32::consts::TAU).sin();
        self.phase += self.increment;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        (left, right)
    }

    pub(crate) fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// Cheap, stable soft clipper (`tanh` approximation is fine here — `tanh` itself
/// is used where accuracy matters).
#[inline]
pub(crate) fn soft_clip(x: f32) -> f32 {
    x.tanh()
}

/// Snap a decaying recursive state to zero before it reaches the subnormal
/// range — see [`builtin_dsp_core::flush_denormal`].
///
/// Every IIR state in this chain (resonators, one-poles, the halfband
/// allpasses, the biquads) decays geometrically once the player stops. Left
/// alone they cross into denormal floats, where each operation costs tens to
/// hundreds of cycles: the modeled cabinet alone measured **19x** slower on
/// the decay into silence than on signal, which is exactly the "spikes a
/// moment after I stop playing" fault. Applying this to whatever a filter
/// feeds back is what keeps a silent chain cheap.
pub(crate) use builtin_dsp_core::flush_denormal;

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_WAVENET_48K: &str = r#"{
        "version": "0.5.4", "architecture": "WaveNet",
        "config": { "layers": [{
            "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
            "kernel_size": 1, "dilations": [1], "activation": "ReLU",
            "gated": false, "head_bias": false
        }], "head": null, "head_scale": 1.0 },
        "weights": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0],
        "sample_rate": 48000.0
    }"#;

    /// Render a fixed guitar-ish burst through `dsp` and return the samples.
    /// Deterministic, so two rigs can be compared sample-for-sample.
    fn render(dsp: &mut Dsp, n: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            if i % 128 == 0 {
                dsp.begin_block();
            }
            let t = i as f32 / 48_000.0;
            let env = (-t * 3.0).exp();
            let x = ((t * 220.0 * std::f32::consts::TAU).sin() * 0.6
                + (t * 330.0 * std::f32::consts::TAU).sin() * 0.3)
                * env
                * 0.5;
            let (l, _r) = dsp.process_stereo(x, x);
            out.push(l);
        }
        out
    }

    fn rms(xs: &[f32]) -> f32 {
        (xs.iter().map(|v| v * v).sum::<f32>() / xs.len().max(1) as f32).sqrt()
    }

    /// Put `stages` in the path, in order, and nothing else.
    fn path_of(dsp: &mut Dsp, stages: &[StageKind]) {
        let mut order = [None; PATH_SLOTS];
        for (i, s) in stages.iter().enumerate() {
            order[i] = Some(*s);
        }
        dsp.set_path_order(order);
    }

    /// A second instance must start exactly where the stage it doubles starts.
    /// The editor derives its B tables from the A tables, so any drift here
    /// would put a different number in the DSP from the one on screen.
    #[test]
    fn second_instance_defaults_match_the_stage_they_double() {
        let a = default_params();
        let b = StageBParams::default();
        assert_eq!(b.drive_model, a.drive_model);
        assert_eq!(b.drive_gain, a.drive_gain);
        assert_eq!(b.drive_tone, a.drive_tone);
        assert_eq!(b.drive_level, a.drive_level);
        assert_eq!(b.mod_model, a.mod_model);
        assert_eq!(b.chorus_rate, a.chorus_rate);
        assert_eq!(b.chorus_depth, a.chorus_depth);
        assert_eq!(b.chorus_mix, a.chorus_mix);
        assert_eq!(b.delay_model, a.delay_model);
        assert_eq!(b.delay_time_ms, a.delay_time_ms);
        assert_eq!(b.delay_fb, a.delay_fb);
        assert_eq!(b.delay_mix, a.delay_mix);
        assert_eq!(b.delay_tone, a.delay_tone);
        assert_eq!(b.eq_low_gain_db, a.eq_low_gain_db);
        assert_eq!(b.eq_mid1_freq_hz, a.eq_mid1_freq_hz);
        assert_eq!(b.eq_mid1_gain_db, a.eq_mid1_gain_db);
        assert_eq!(b.eq_mid2_freq_hz, a.eq_mid2_freq_hz);
        assert_eq!(b.eq_mid2_gain_db, a.eq_mid2_gain_db);
        assert_eq!(b.eq_high_gain_db, a.eq_high_gain_db);
        assert_eq!(b.comp_thresh_db, a.comp_thresh_db);
        assert_eq!(b.comp_ratio, a.comp_ratio);
        assert_eq!(b.comp_attack_ms, a.comp_attack_ms);
        assert_eq!(b.comp_release_ms, a.comp_release_ms);
        assert_eq!(b.comp_makeup_db, a.comp_makeup_db);
    }

    /// A stage and its double are different stages, so both survive the
    /// duplicate filter — that filter exists to stop the *same* block being
    /// listed twice, not to stop a rig having two drives.
    #[test]
    fn a_stage_and_its_double_both_stay_in_the_path() {
        let mut dsp = Dsp::new(48_000.0);
        path_of(
            &mut dsp,
            &[
                StageKind::Drive,
                StageKind::Drive2,
                StageKind::Amp,
                StageKind::Drive,
            ],
        );
        let order = dsp.params().stage_order;
        assert_eq!(order[0], Some(StageKind::Drive));
        assert_eq!(order[1], Some(StageKind::Drive2));
        assert_eq!(order[2], Some(StageKind::Amp));
        // The *repeat* of Drive is still dropped — one block, one slot.
        assert_eq!(order[3], None);
    }

    /// The second instance must be its own block: its own model, its own
    /// knobs, its own filter/delay state. If it were a re-run of the first,
    /// changing only the B knobs would not move the output at all.
    #[test]
    fn a_doubled_drive_is_an_independent_block() {
        let single = {
            let mut dsp = Dsp::new(48_000.0);
            path_of(&mut dsp, &[StageKind::Drive]);
            render(&mut dsp, 12_000)
        };

        let mut doubled = Dsp::new(48_000.0);
        path_of(&mut doubled, &[StageKind::Drive, StageKind::Drive2]);
        assert!(doubled.select_model("drive2", "rat"));
        assert!(doubled.apply_ui_param("drive2_gain", 8.5));
        assert!(doubled.apply_ui_param("drive2_level", 7.0));
        assert_eq!(doubled.params().stage_b.drive_model, DriveModel::Rat);
        // The first drive is untouched by anything addressed to the second.
        assert_eq!(doubled.params().drive_model, DriveModel::Screamer);
        assert_eq!(doubled.params().drive_gain, default_params().drive_gain);
        let stacked = render(&mut doubled, 12_000);

        assert!(stacked.iter().all(|v| v.is_finite()));
        assert!(
            rms(&stacked) > rms(&single) * 1.05,
            "a second drive at gain 8.5 did not change the sound: \
             single {:.5} vs stacked {:.5}",
            rms(&single),
            rms(&stacked)
        );

        // The same rig with only the second block switched off must be the
        // single-drive rig, sample for sample: proof the extra sound came from
        // the B instance rather than from the A one being configured twice.
        // A fresh `Dsp` rather than a `reset()` — comparing two rigs from cold
        // is the honest comparison; a reset one carries settled smoother state
        // the cold one has not reached yet.
        let mut off = Dsp::new(48_000.0);
        path_of(&mut off, &[StageKind::Drive, StageKind::Drive2]);
        assert!(off.select_model("drive2", "rat"));
        assert!(off.apply_ui_param("drive2_gain", 8.5));
        assert!(off.apply_ui_param("drive2_level", 7.0));
        assert!(off.apply_ui_param("drive2_on", 0.0));
        let bypassed = render(&mut off, 12_000);
        for (i, (a, b)) in single.iter().zip(bypassed.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1.0e-6,
                "bypassing Drive2 did not restore the single-drive rig at {i}: {a} vs {b}"
            );
        }
    }

    /// Each doublable stage must actually run its B object when the B block is
    /// in the path — a missing `process` arm would be a block that looks
    /// placed and does nothing.
    #[test]
    fn every_doubled_stage_processes_its_own_instance() {
        // Settings extreme enough that the B block cannot be mistaken for a
        // no-op, paired with the block that must carry them.
        let cases: [(StageKind, &[(&str, f32)]); 5] = [
            (
                StageKind::Drive2,
                &[("drive2_gain", 10.0), ("drive2_level", 9.0)],
            ),
            (
                StageKind::Mod2,
                &[("chorus2_mix", 100.0), ("chorus2_depth", 10.0)],
            ),
            (
                StageKind::Delay2,
                &[("delay2_mix", 100.0), ("delay2_fb", 60.0)],
            ),
            (
                StageKind::Eq2,
                &[("eq2_low_gain", 15.0), ("eq2_high_gain", 15.0)],
            ),
            (
                StageKind::Comp2,
                &[("comp2_thresh", -50.0), ("comp2_makeup", 18.0)],
            ),
        ];

        for (stage, edits) in cases {
            let mut dsp = Dsp::new(48_000.0);
            path_of(&mut dsp, &[stage]);
            for (id, v) in edits {
                assert!(dsp.apply_ui_param(id, *v), "`{id}` was not routed");
            }
            let with = render(&mut dsp, 12_000);
            assert!(
                with.iter().all(|v| v.is_finite()),
                "{stage:?} went non-finite"
            );

            let mut dry = Dsp::new(48_000.0);
            path_of(&mut dry, &[]);
            let without = render(&mut dry, 12_000);

            let changed = with
                .iter()
                .zip(without.iter())
                .any(|(a, b)| (a - b).abs() > 1.0e-4);
            assert!(changed, "{stage:?} in the path did not process anything");
        }
    }

    #[test]
    fn tone_engine_is_mutually_exclusive_via_select_model_and_ui_param() {
        let mut dsp = Dsp::new(48_000.0);
        assert_eq!(dsp.params().tone_engine, ToneEngineKind::Classic);

        assert!(dsp.select_model("amp", "nam_capture"));
        assert_eq!(dsp.params().tone_engine, ToneEngineKind::NamCapture);

        assert!(dsp.select_model("amp", "bypass"));
        assert_eq!(dsp.params().tone_engine, ToneEngineKind::Bypass);

        // Picking a classic amp model always snaps the engine back to Classic.
        assert!(dsp.select_model("amp", "plexi"));
        assert_eq!(dsp.params().tone_engine, ToneEngineKind::Classic);
        assert_eq!(dsp.params().amp_model, AmpModel::Plexi);

        assert!(dsp.apply_ui_param("tone_engine", 1.0));
        assert_eq!(dsp.params().tone_engine, ToneEngineKind::NamCapture);
    }

    #[test]
    fn nam_capture_loads_and_swaps_at_block_boundary_not_mid_block() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.apply_ui_param("tone_engine", 1.0);

        let info = dsp
            .load_nam_capture_json(TINY_WAVENET_48K, "Test Capture", false, true)
            .expect("matching sample rate must load");
        assert_eq!(info.name, "Test Capture");
        assert!(info.full_rig);
        assert!(
            dsp.nam_capture_info().is_none(),
            "not adopted before begin_block"
        );

        dsp.begin_block();
        assert!(
            dsp.nam_capture_info().is_some(),
            "adopted at block boundary"
        );

        for _ in 0..256 {
            let (l, r) = dsp.process_stereo(0.2, -0.2);
            assert!(l.is_finite() && r.is_finite());
        }

        dsp.poll_nam_garbage();
    }

    /// A capture built at another rate loads and runs through the rate
    /// adapter; only a ratio the adapter cannot bridge is still refused.
    #[test]
    fn nam_capture_adapts_a_mismatched_rate_and_still_refuses_an_absurd_one() {
        let mut dsp = Dsp::new(44_100.0);
        let info = dsp
            .load_nam_capture_json(TINY_WAVENET_48K, "Other Rate", false, false)
            .expect("a 48 kHz capture must load in a 44.1 kHz engine");
        assert_eq!(
            info.sample_rate, 48_000.0,
            "info reports the capture's rate"
        );
        dsp.begin_block();
        dsp.apply_ui_param("tone_engine", ToneEngineKind::NamCapture.index() as f32);
        for n in 0..8_000 {
            let x = (n as f32 * 0.02).sin() * 0.4;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite(), "resampled capture blew up");
        }

        // 48 kHz capture in a 6 kHz engine is an 8x ratio — past what the
        // adapter supports, so it stays a clean error rather than a load.
        let mut slow = Dsp::new(6_000.0);
        let err = slow
            .load_nam_capture_json(TINY_WAVENET_48K, "Way Off", false, false)
            .expect_err("an 8x rate ratio must be refused");
        assert!(matches!(err, NamLoadError::SampleRateMismatch { .. }));
    }

    /// Build a float-WAV IR file around raw samples (mirrors `ir::tests`).
    fn test_ir_wav(len: usize, rate: u32) -> Vec<u8> {
        let samples: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / len as f32;
                ((i as f32 * 0.7).sin() * (1.0 - t) * (1.0 - t)) * 0.5
            })
            .collect();
        let data: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        fmt.extend_from_slice(&1u16.to_le_bytes()); // mono
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * 4).to_le_bytes());
        fmt.extend_from_slice(&4u16.to_le_bytes());
        fmt.extend_from_slice(&32u16.to_le_bytes());
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// Loading an IR, selecting the IR cabinet engine, and crossfading back to
    /// a modeled voicing must all stay finite — and only the IR selection may
    /// report convolution latency.
    #[test]
    fn ir_cabinet_loads_selects_and_crossfades_back() {
        let mut dsp = Dsp::new(48_000.0);
        let info = dsp
            .load_ir_wav(&test_ir_wav(512, 48_000), "Test Cab IR")
            .expect("a well-formed IR must load");
        assert_eq!(info.name, "Test Cab IR");
        assert_eq!(info.latency_samples, IR_PARTITION_SAMPLES);
        assert!(dsp.ir_info().is_none(), "not adopted before begin_block");
        dsp.begin_block();
        assert!(dsp.ir_info().is_some(), "adopted at the block boundary");

        // Still on a modeled voicing: no convolution latency is reported.
        assert_eq!(dsp.latency_samples(), 0);

        assert!(dsp.select_model("cab", "ir"));
        assert_eq!(dsp.params().cab_model, CabModel::Ir);
        assert_eq!(dsp.latency_samples(), IR_PARTITION_SAMPLES);

        let mut peak = 0.0f32;
        for n in 0..24_000 {
            if n % 128 == 0 {
                dsp.begin_block();
            }
            let x = (n as f32 * 0.03).sin() * 0.4;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite(), "IR cabinet went non-finite");
            peak = peak.max(l.abs()).max(r.abs());
        }
        assert!(peak > 1.0e-4, "IR cabinet produced silence");
        assert!(peak < 8.0, "IR cabinet ran away: {peak}");

        // Crossfade back to a modeled cabinet.
        assert!(dsp.select_model("cab", "modern_412"));
        assert_eq!(dsp.latency_samples(), 0);
        for n in 0..8_000 {
            if n % 128 == 0 {
                dsp.begin_block();
            }
            let x = (n as f32 * 0.03).sin() * 0.4;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(
                l.is_finite() && r.is_finite(),
                "cab crossfade went non-finite"
            );
        }
        dsp.poll_ir_garbage();
    }

    /// With the IR engine selected but no file loaded, the cabinet slot must
    /// pass audio through rather than muting the rig.
    #[test]
    fn ir_cabinet_without_a_file_does_not_mute_the_rig() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.select_model("cab", "ir"));
        assert_eq!(
            dsp.latency_samples(),
            IR_PARTITION_SAMPLES,
            "the IR slot's block ring runs whether or not a file is loaded, and \
             its latency must be reported so the host compensates it"
        );
        let mut peak = 0.0f32;
        for n in 0..12_000 {
            if n % 128 == 0 {
                dsp.begin_block();
            }
            let x = (n as f32 * 0.03).sin() * 0.4;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
            if n > 4_000 {
                peak = peak.max(l.abs()).max(r.abs());
            }
        }
        assert!(peak > 1.0e-3, "an unloaded IR slot muted the rig: {peak}");
    }

    /// A malformed or unusable IR file is a clean error, never a panic and
    /// never a half-adopted runtime.
    #[test]
    fn a_bad_ir_file_is_a_clean_error() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.load_ir_wav(b"not a wav file at all", "bad").is_err());
        assert!(dsp.ir_info().is_none());
        dsp.begin_block();
        assert!(dsp.ir_info().is_none(), "a failed load must adopt nothing");
    }

    fn silence_tail_is_finite(dsp: &mut Dsp) {
        for _ in 0..8_000 {
            let (l, r) = dsp.process_stereo(0.0, 0.0);
            assert!(l.is_finite() && r.is_finite());
        }
    }

    #[test]
    fn full_chain_stays_finite_and_bounded() {
        let mut dsp = Dsp::new(48_000.0);
        let mut worst = 0.0f32;
        for n in 0..48_000 {
            // A gnarly test signal: decaying pluck + noise-ish alternation.
            let t = n as f32 / 48_000.0;
            let x = (t * 220.0 * std::f32::consts::TAU).sin() * (-t * 3.0).exp()
                + if n % 2 == 0 { 0.05 } else { -0.05 };
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
            worst = worst.max(l.abs()).max(r.abs());
        }
        // The chain must not explode (reverb + delay feedback stay stable).
        assert!(worst < 8.0, "output blew up: peak={worst}");
    }

    #[test]
    fn boost_amp_cab_chain_adds_saturation_without_runaway_clipping() {
        let make = |drive_on: bool| {
            let mut dsp = Dsp::new(48_000.0);
            let mut p = default_params();
            p.stage_order = [None; PATH_SLOTS];
            p.stage_order[0] = Some(StageKind::Drive);
            p.stage_order[1] = Some(StageKind::Amp);
            p.stage_order[2] = Some(StageKind::Cab);
            p.drive_on = drive_on;
            p.drive_model = DriveModel::Screamer;
            p.drive_gain = 2.0;
            p.drive_tone = 6.5;
            p.drive_level = 9.0;
            p.amp_model = AmpModel::Recto;
            p.amp_gain = 7.5;
            p.amp_master = 5.0;
            p.cab_model = CabModel::Modern4x12;
            p.mic_model = MicModel::Dynamic;
            dsp.set_params(p);
            dsp.reset();
            dsp
        };
        let mut boosted = make(true);
        let mut bare = make(false);
        let mut difference = 0.0;
        let mut peak = 0.0f32;
        for n in 0..24_000 {
            let t = n as f32 / 48_000.0;
            let x = (t * 110.0 * std::f32::consts::TAU).sin() * 0.55
                + (t * 1_700.0 * std::f32::consts::TAU).sin() * 0.04;
            let with_boost = boosted.process_stereo(x, x).0;
            let without = bare.process_stereo(x, x).0;
            assert!(with_boost.is_finite());
            peak = peak.max(with_boost.abs());
            if n > 8_000 {
                difference += (with_boost - without).powi(2);
            }
        }
        let rms_difference = (difference / 16_000.0).sqrt();
        assert!(rms_difference > 0.005, "boost did not drive the amp");
        assert!(peak < 4.0, "boost caused uncontrolled clipping: {peak}");
    }

    #[test]
    fn amp_cab_chain_is_stable_at_required_rates_and_block_sizes() {
        for &sample_rate in &[44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            for &block_size in &[1usize, 16, 64, 128, 512, 2_048] {
                let mut dsp = Dsp::new(sample_rate);
                let mut p = default_params();
                p.stage_order = [None; PATH_SLOTS];
                p.stage_order[0] = Some(StageKind::Amp);
                p.stage_order[1] = Some(StageKind::Cab);
                p.amp_model = AmpModel::Slate;
                p.amp_gain = 9.0;
                p.amp_master = 7.0;
                p.cab_model = CabModel::Oversized4x12;
                p.mic_model = MicModel::Condenser;
                dsp.set_params(p);
                dsp.reset();
                let mut sample = 0usize;
                let mut peak = 0.0f32;
                while sample < 4_096 {
                    dsp.begin_block();
                    for _ in 0..block_size.min(4_096 - sample) {
                        let x = (sample as f32 * 220.0 * std::f32::consts::TAU / sample_rate).sin()
                            * 0.7;
                        let (left, right) = dsp.process_stereo(x, -x);
                        assert!(
                            left.is_finite() && right.is_finite(),
                            "rate={sample_rate} block={block_size}"
                        );
                        peak = peak.max(left.abs()).max(right.abs());
                        sample += 1;
                    }
                }
                assert!(
                    peak > 0.001 && peak < 4.0,
                    "rate={sample_rate} block={block_size} peak={peak}"
                );
            }
        }
    }

    #[test]
    fn power_off_is_bit_transparent() {
        let mut dsp = Dsp::new(48_000.0);
        let mut p = default_params();
        p.power = false;
        dsp.set_params(p);
        for n in 0..1_000 {
            let x = (n as f32 * 0.01).sin();
            assert_eq!(dsp.process_stereo(x, -x), (x, -x));
        }
    }

    #[test]
    fn apply_ui_param_covers_data_ts_ids() {
        let mut dsp = Dsp::new(48_000.0);
        // Every id present in editorui/src/data.ts must be routable.
        for id in [
            "gate_thresh",
            "drive_gain",
            "drive_tone",
            "drive_level",
            "amp_gain",
            "amp_bass",
            "amp_middle",
            "amp_treble",
            "amp_presence",
            "amp_master",
            "chorus_rate",
            "chorus_depth",
            "chorus_mix",
            "delay_time",
            "delay_fb",
            "delay_mix",
            "reverb_decay",
            "reverb_mix",
            "reverb_shimmer",
            "cab_mic",
            "cab_dist",
            // Global gain staging (editorui/src/globals.ts).
            "power",
            "input_trim",
            "output_trim",
        ] {
            assert!(dsp.apply_ui_param(id, 5.0), "id `{id}` was not routed");
        }
        assert!(!dsp.apply_ui_param("does_not_exist", 1.0));
    }

    #[test]
    fn model_selection_switches_voicing() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_ui_param("amp_model", 1.0));
        assert_eq!(dsp.params().amp_model, AmpModel::Plexi);
        assert!(dsp.apply_ui_param("drive_model", 1.0));
        assert_eq!(dsp.params().drive_model, DriveModel::Minotaur);
        assert!(dsp.select_model("amp", "recto"));
        assert_eq!(dsp.params().amp_model, AmpModel::Recto);
        assert!(dsp.select_model("drive", "fuzz"));
        assert_eq!(dsp.params().drive_model, DriveModel::Fuzz);
        assert!(dsp.apply_ui_param("cab_model", 1.0));
        assert_eq!(dsp.params().cab_model, CabModel::American2x12);
        assert!(dsp.select_model("cab", "modern_412"));
        assert_eq!(dsp.params().cab_model, CabModel::Modern4x12);
        assert!(!dsp.select_model("cab", "not_a_cab"));
    }

    #[test]
    fn path_order_reorders_processing_slots() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_ui_param("path_slot_0", 2.0)); // Amp first
        assert!(dsp.apply_ui_param("path_slot_1", 0.0)); // Gate
        assert!(dsp.apply_ui_param("path_slot_2", -1.0)); // clear
        // Remaining slots still have defaults until overwritten — sanitize packs.
        let mut order = [None; PATH_SLOTS];
        order[0] = Some(StageKind::Amp);
        order[1] = Some(StageKind::Cab);
        dsp.set_path_order(order);
        let mut expected = [None; PATH_SLOTS];
        expected[0] = Some(StageKind::Amp);
        expected[1] = Some(StageKind::Cab);
        assert_eq!(dsp.params().stage_order, expected);
        let (l, r) = dsp.process_stereo(0.2, -0.2);
        assert!(l.is_finite() && r.is_finite());
    }

    #[test]
    fn reverb_tail_decays_to_silence() {
        let mut dsp = Dsp::new(48_000.0);
        // Isolate the reverb so we can assert the tail dies out.
        let mut p = default_params();
        p.gate_on = false;
        p.drive_on = false;
        p.amp_on = false;
        p.mod_on = false;
        p.delay_on = false;
        p.cab_on = false;
        p.reverb_decay_s = 2.0;
        p.reverb_mix = 100.0;
        dsp.set_params(p);
        // Excite, then run a long silent tail.
        for _ in 0..64 {
            let _ = dsp.process_stereo(0.5, 0.5);
        }
        silence_tail_is_finite(&mut dsp);
        let mut tail = 0.0f32;
        for _ in 0..2_000 {
            let (l, r) = dsp.process_stereo(0.0, 0.0);
            tail = tail.max(l.abs()).max(r.abs());
        }
        assert!(tail < 0.2, "reverb tail did not decay: {tail}");
    }

    /// Every reverb voicing must stay finite and bounded under sustained
    /// excitation — the Shimmer voicing has a second (octave-up) feedback path,
    /// so this specifically guards it from running away.
    #[test]
    fn all_reverb_models_stay_bounded() {
        for &model in ReverbModel::ALL {
            let mut dsp = Dsp::new(48_000.0);
            let mut p = default_params();
            p.gate_on = false;
            p.drive_on = false;
            p.amp_on = false;
            p.mod_on = false;
            p.delay_on = false;
            p.cab_on = false;
            p.reverb_model = model;
            p.reverb_decay_s = 15.0; // worst case for tail buildup
            p.reverb_mix = 100.0;
            p.reverb_shimmer = 100.0;
            dsp.set_params(p);
            let mut peak = 0.0f32;
            for n in 0..48_000 {
                // Continuous 220 Hz excitation feeds the shimmer loop hard.
                let x = (n as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.6;
                let (l, r) = dsp.process_stereo(x, -x);
                assert!(l.is_finite() && r.is_finite(), "{model:?} went non-finite");
                peak = peak.max(l.abs()).max(r.abs());
            }
            assert!(peak < 8.0, "{model:?} ran away: peak={peak}");
        }
    }

    fn render_reverb_impulse(model: ReverbModel, left: f32, right: f32) -> Vec<(f32, f32)> {
        let mut dsp = Dsp::new(48_000.0);
        let mut p = default_params();
        p.stage_order = [None; PATH_SLOTS];
        p.stage_order[0] = Some(StageKind::Reverb);
        p.reverb_on = true;
        p.reverb_model = model;
        p.reverb_decay_s = 4.0;
        p.reverb_mix = 100.0;
        p.reverb_shimmer = 72.0;
        dsp.set_params(p);
        // Snap all smoothed topology controls to the selected model so this
        // measures that model, not the short transition from the default Plate.
        dsp.reset();

        let mut output = Vec::with_capacity(48_000);
        for index in 0..48_000 {
            let input_l = if index == 0 { left } else { 0.0 };
            let input_r = if index == 0 { right } else { 0.0 };
            output.push(dsp.process_stereo(input_l, input_r));
        }
        output
    }

    #[test]
    fn every_reverb_model_is_selectable_audible_and_distinct() {
        let ids = [
            ("plate", ReverbModel::Plate),
            ("room", ReverbModel::Room),
            ("hall", ReverbModel::Hall),
            ("shimmer", ReverbModel::Shimmer),
        ];
        let mut rendered = Vec::with_capacity(ids.len());

        for (id, expected) in ids {
            let mut selected = Dsp::new(48_000.0);
            assert!(selected.select_model("reverb", id), "`{id}` was rejected");
            assert_eq!(selected.params().reverb_model, expected);

            let output = render_reverb_impulse(expected, 1.0, 0.25);
            let energy: f32 = output.iter().map(|(l, r)| l * l + r * r).sum();
            assert!(
                energy > 1.0e-4,
                "{expected:?} produced no usable wet impulse response"
            );
            assert!(
                output.iter().all(|(l, r)| l.is_finite() && r.is_finite()),
                "{expected:?} produced non-finite output"
            );
            rendered.push((expected, output));
        }

        for left in 0..rendered.len() {
            for right in (left + 1)..rendered.len() {
                let difference: f32 = rendered[left]
                    .1
                    .iter()
                    .zip(rendered[right].1.iter())
                    .map(|(&(ll, lr), &(rl, rr))| {
                        let dl = ll - rl;
                        let dr = lr - rr;
                        dl * dl + dr * dr
                    })
                    .sum::<f32>()
                    / rendered[left].1.len() as f32;
                assert!(
                    difference.sqrt() > 1.0e-4,
                    "{:?} and {:?} collapsed to the same algorithm",
                    rendered[left].0,
                    rendered[right].0
                );
            }
        }
    }

    #[test]
    fn reverb_wet_path_does_not_cancel_wide_antiphase_input() {
        for &model in ReverbModel::ALL {
            let output = render_reverb_impulse(model, 1.0, -1.0);
            let energy: f32 = output.iter().map(|(l, r)| l * l + r * r).sum();
            assert!(
                energy > 1.0e-4,
                "{model:?} erased anti-phase stereo input from its wet path"
            );
        }
    }

    /// Every delay voicing must survive maximum feedback through the full
    /// chain, and `select_model("delay", …)` must reach each of them.
    #[test]
    fn all_delay_models_stay_bounded_and_are_selectable() {
        for &model in DelayModel::ALL {
            let mut dsp = Dsp::new(48_000.0);
            let mut p = default_params();
            p.gate_on = false;
            p.drive_on = false;
            p.amp_on = false;
            p.mod_on = false;
            p.reverb_on = false;
            p.cab_on = false;
            p.delay_model = model;
            p.delay_time_ms = 90.0; // short + hot feedback is the worst case
            p.delay_fb = 100.0;
            p.delay_mix = 100.0;
            p.delay_tone = 10.0; // brightest feedback path
            dsp.set_params(p);
            let mut peak = 0.0f32;
            for n in 0..48_000 {
                let x = (n as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.6;
                let (l, r) = dsp.process_stereo(x, -x);
                assert!(l.is_finite() && r.is_finite(), "{model:?} went non-finite");
                peak = peak.max(l.abs()).max(r.abs());
            }
            assert!(peak < 8.0, "{model:?} ran away: peak={peak}");
        }

        let mut dsp = Dsp::new(48_000.0);
        for (id, expected) in [
            ("tape", DelayModel::Tape),
            ("digital", DelayModel::Digital),
            ("analog", DelayModel::Analog),
            ("ping_pong", DelayModel::PingPong),
            ("dual", DelayModel::Dual),
        ] {
            assert!(dsp.select_model("delay", id), "delay model `{id}` rejected");
            assert_eq!(dsp.params().delay_model, expected);
        }
        assert!(!dsp.select_model("delay", "not_a_delay"));
    }

    // ---- Dedicated drive-topology battery (ds_one / super_drive /
    // metal_core / tight_rift) --------------------------------------------

    const TOPOLOGY_MODELS: [DriveModel; 4] = [
        DriveModel::DsOne,
        DriveModel::SuperDrive,
        DriveModel::MetalCore,
        DriveModel::TightRift,
    ];

    fn drive_only_dsp(model: DriveModel, gain: f32, sr: f32) -> Dsp {
        let mut dsp = Dsp::new(sr);
        let mut p = default_params();
        p.stage_order = [None; PATH_SLOTS];
        p.stage_order[0] = Some(StageKind::Drive);
        p.drive_model = model;
        p.drive_gain = gain;
        dsp.set_params(p);
        dsp.reset();
        dsp
    }

    /// Deterministic white-ish noise without pulling in a rand dependency.
    fn lcg_noise(state: &mut u32) -> f32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*state >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0
    }

    /// 1) Finite output over silence, impulse, sine, noise and hot input.
    #[test]
    fn topology_models_are_finite_for_hostile_inputs() {
        for model in TOPOLOGY_MODELS {
            let mut dsp = drive_only_dsp(model, 10.0, 48_000.0);
            let mut noise = 0x1234_5678u32;
            for n in 0..24_000 {
                let x = match n / 4_000 {
                    0 => 0.0,                   // silence
                    1 if n % 4_000 == 0 => 1.0, // impulse
                    1 => 0.0,
                    2 => (n as f32 * 0.05).sin() * 0.5, // sine
                    3 => lcg_noise(&mut noise) * 0.3,   // noise
                    4 => (n as f32 * 0.11).sin() * 4.0, // hot input
                    _ => lcg_noise(&mut noise) * 4.0,   // hot noise
                };
                let (l, r) = dsp.process_stereo(x, x);
                assert!(l.is_finite() && r.is_finite(), "{model:?} sample {n}");
            }
        }
    }

    /// 2) No runaway: repeated full-scale blocks at max drive stay bounded.
    #[test]
    fn topology_models_do_not_run_away_at_max_drive() {
        for model in TOPOLOGY_MODELS {
            let mut dsp = drive_only_dsp(model, 10.0, 48_000.0);
            let mut peak: f32 = 0.0;
            for n in 0..48_000 {
                let x = if (n / 64) % 2 == 0 { 1.0 } else { -1.0 };
                let (l, r) = dsp.process_stereo(x, x);
                peak = peak.max(l.abs()).max(r.abs());
            }
            assert!(peak < 4.0, "{model:?} unbounded: peak={peak}");
            assert!(peak > 0.01, "{model:?} silent at max drive");
        }
    }

    /// 4) Harmonic generation: the distorted sine must differ materially from
    /// any pure rescaling of the input (i.e. real waveshaping happened).
    #[test]
    fn topology_models_generate_harmonics() {
        for model in TOPOLOGY_MODELS {
            let mut dsp = drive_only_dsp(model, 8.0, 48_000.0);
            let mut input = Vec::new();
            let mut output = Vec::new();
            for n in 0..24_000 {
                let x = (n as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.5;
                let (l, _) = dsp.process_stereo(x, x);
                if n > 8_000 {
                    input.push(x);
                    output.push(l);
                }
            }
            // Best-fit scale, then residual energy: pure gain would leave ~0.
            let dot: f32 = input.iter().zip(&output).map(|(a, b)| a * b).sum();
            let in_e: f32 = input.iter().map(|a| a * a).sum();
            let scale = dot / in_e.max(1.0e-9);
            let resid: f32 = input
                .iter()
                .zip(&output)
                .map(|(a, b)| (b - a * scale).powi(2))
                .sum();
            let out_e: f32 = output.iter().map(|b| b * b).sum();
            assert!(
                resid / out_e.max(1.0e-9) > 0.02,
                "{model:?} output is just a rescaled input (residual ratio {})",
                resid / out_e.max(1.0e-9)
            );
        }
    }

    /// 5) Model distinction: same input, materially different outputs.
    #[test]
    fn topology_models_are_mutually_distinct() {
        let mut outputs: Vec<Vec<f32>> = Vec::new();
        for model in TOPOLOGY_MODELS {
            let mut dsp = drive_only_dsp(model, 7.0, 48_000.0);
            let mut buf = Vec::new();
            let mut noise = 0xBEEF_CAFEu32;
            for n in 0..12_000 {
                let x = (n as f32 * 0.04).sin() * 0.4 + lcg_noise(&mut noise) * 0.05;
                let (l, _) = dsp.process_stereo(x, x);
                if n > 4_000 {
                    buf.push(l);
                }
            }
            outputs.push(buf);
        }
        for i in 0..outputs.len() {
            for j in (i + 1)..outputs.len() {
                let diff: f32 = outputs[i]
                    .iter()
                    .zip(&outputs[j])
                    .map(|(a, b)| (a - b).powi(2))
                    .sum::<f32>()
                    / outputs[i].len() as f32;
                assert!(
                    diff.sqrt() > 0.01,
                    "{:?} and {:?} sound effectively identical (rms diff {})",
                    TOPOLOGY_MODELS[i],
                    TOPOLOGY_MODELS[j],
                    diff.sqrt()
                );
            }
        }
    }

    /// 6) Instance isolation: resetting/re-configuring one instance must not
    /// disturb another.
    #[test]
    fn topology_model_instances_are_isolated() {
        let mut victim = drive_only_dsp(DriveModel::MetalCore, 7.0, 48_000.0);
        let mut control = drive_only_dsp(DriveModel::MetalCore, 7.0, 48_000.0);
        let mut other = drive_only_dsp(DriveModel::TightRift, 9.0, 48_000.0);

        // Warm all three identically.
        for n in 0..4_000 {
            let x = (n as f32 * 0.05).sin() * 0.4;
            let _ = victim.process_stereo(x, x);
            let _ = control.process_stereo(x, x);
            let _ = other.process_stereo(x, x);
        }
        // Hammer `other`: reset + retune. `victim` must keep tracking `control`.
        other.reset();
        assert!(other.apply_ui_param("drive_gain", 1.0));
        for n in 0..4_000 {
            let x = (n as f32 * 0.05).sin() * 0.4;
            let (vl, _) = victim.process_stereo(x, x);
            let (cl, _) = control.process_stereo(x, x);
            let _ = other.process_stereo(x, x);
            assert!(
                (vl - cl).abs() < 1.0e-6,
                "instance state leaked at sample {n}: {vl} vs {cl}"
            );
        }
    }

    /// 7) State restore: every topology model round-trips through the
    /// persistence blob with its parameters intact.
    #[test]
    fn topology_models_restore_from_state_blob() {
        for model in TOPOLOGY_MODELS {
            let mut p = default_params();
            p.drive_model = model;
            p.drive_gain = 8.25;
            p.drive_tone = 3.5;
            let json = crate::RodhareistState::new(p.clone()).to_json().unwrap();
            let restored = crate::RodhareistState::from_json(&json).unwrap();
            assert_eq!(restored.params.drive_model, model);
            assert_eq!(restored.params.drive_gain, 8.25);
            assert_eq!(restored.params.drive_tone, 3.5);
        }
    }

    /// 8) Sample-rate sweep incl. high rates.
    #[test]
    fn topology_models_are_stable_across_sample_rates() {
        for &sr in &[44_100.0f32, 48_000.0, 96_000.0, 192_000.0] {
            for model in TOPOLOGY_MODELS {
                let mut dsp = drive_only_dsp(model, 9.0, sr);
                let mut peak: f32 = 0.0;
                for n in 0..(sr as usize / 4) {
                    let x = (n as f32 * 2.0 * std::f32::consts::PI * 220.0 / sr).sin() * 0.6;
                    let (l, r) = dsp.process_stereo(x, x);
                    assert!(l.is_finite() && r.is_finite(), "{model:?}@{sr}");
                    peak = peak.max(l.abs());
                }
                assert!(peak > 1.0e-3 && peak < 4.0, "{model:?}@{sr} peak={peak}");
            }
        }
    }

    /// 9) Block-size independence: the per-sample API must produce the same
    /// stream regardless of how callers group samples around `begin_block`.
    #[test]
    fn topology_models_are_block_size_invariant() {
        for &block in &[1usize, 16, 64, 128, 512, 2048] {
            let mut chunked = drive_only_dsp(DriveModel::TightRift, 8.0, 48_000.0);
            let mut reference = drive_only_dsp(DriveModel::TightRift, 8.0, 48_000.0);
            reference.begin_block();
            let mut n = 0usize;
            while n < 8_192 {
                chunked.begin_block();
                for _ in 0..block.min(8_192 - n) {
                    let x = (n as f32 * 0.03).sin() * 0.5;
                    let (a, _) = chunked.process_stereo(x, x);
                    let (b, _) = reference.process_stereo(x, x);
                    assert!((a - b).abs() < 1.0e-6, "block={block} sample {n}");
                    n += 1;
                }
            }
        }
    }

    /// 10) Abrupt Drive/Tone automation: no NaN, no single-sample spike.
    #[test]
    fn topology_models_survive_abrupt_automation() {
        /// Largest sample-to-sample step over a 382 Hz tone, either with both
        /// knobs slammed between their extremes every 6000 samples, or held at
        /// one setting for the whole run.
        fn max_step(model: DriveModel, held: Option<f32>) -> f32 {
            let mut dsp = drive_only_dsp(model, held.unwrap_or(2.0), 48_000.0);
            if let Some(knob) = held {
                assert!(dsp.apply_ui_param("drive_tone", knob));
            }
            let mut prev = 0.0f32;
            let mut worst = 0.0f32;
            for n in 0..48_000 {
                if held.is_none() && n % 6_000 == 0 {
                    let hi = (n / 6_000) % 2 == 0;
                    assert!(dsp.apply_ui_param("drive_gain", if hi { 10.0 } else { 0.0 }));
                    assert!(dsp.apply_ui_param("drive_tone", if hi { 10.0 } else { 0.0 }));
                }
                let x = (n as f32 * 0.05).sin() * 0.5;
                let (l, _) = dsp.process_stereo(x, x);
                assert!(l.is_finite(), "{model:?} NaN during automation");
                if n > 100 {
                    worst = worst.max((l - prev).abs());
                }
                prev = l;
            }
            worst
        }

        for model in TOPOLOGY_MODELS {
            // A clipped voicing already slews most of its amplitude in a couple
            // of samples, so an absolute bound would only measure how loud the
            // model is. Judge the automated run against what the same model does
            // with the knobs held at the extremes the automation visits: knob
            // motion must not add a discontinuity of its own.
            let natural = max_step(model, Some(0.0)).max(max_step(model, Some(10.0)));
            let automated = max_step(model, None);
            assert!(
                automated < natural + 0.35,
                "{model:?} automation glitch: step={automated} vs {natural} held still"
            );
        }
    }

    /// Retuning a running filter must not restart it. Installing a fresh
    /// `DirectForm1` zeroes x1/x2/y1/y2, which steps the signal on every knob
    /// edit — `configure` re-sets every filter on any parameter change, so this
    /// is the difference between a silent retune and a click per edit.
    #[test]
    fn biquad_retune_keeps_its_running_state() {
        let coeffs =
            |hz: f32| builtin_dsp_core::make_eq_coefficients("lowpass", hz, 0.0, 0.707, 48_000.0);
        let mut swapped = StereoBiquad::none();
        let mut untouched = StereoBiquad::none();
        swapped.set(coeffs(4_000.0));
        untouched.set(coeffs(4_000.0));

        let tone = |n: usize| (n as f32 * 900.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
        for n in 0..2_000 {
            let _ = swapped.run(tone(n), tone(n));
            let _ = untouched.run(tone(n), tone(n));
        }

        // Re-set the *same* coefficients: a state-preserving retune is then a
        // complete no-op, so the two must stay sample-identical.
        swapped.set(coeffs(4_000.0));
        for n in 2_000..2_400 {
            let (a, _) = swapped.run(tone(n), tone(n));
            let (b, _) = untouched.run(tone(n), tone(n));
            assert!(
                (a - b).abs() < 1.0e-6,
                "re-setting identical coefficients restarted the filter at {n}: {a} vs {b}"
            );
        }

        // A genuine retune may change the output, but only continuously.
        let before = swapped.run(tone(2_400), tone(2_400)).0;
        swapped.set(coeffs(1_200.0));
        let after = swapped.run(tone(2_401), tone(2_401)).0;
        assert!(
            (after - before).abs() < 0.1,
            "retune stepped the signal: {before} -> {after}"
        );
    }

    /// Every drive voicing must process finite, audible output at high gain —
    /// guards each new model's shape/voicing arm as the list grows.
    #[test]
    fn every_drive_model_processes_finite_and_audible() {
        for (i, model) in DriveModel::ALL.iter().enumerate() {
            let mut dsp = Dsp::new(48_000.0);
            let mut p = default_params();
            p.stage_order = [None; PATH_SLOTS];
            p.stage_order[0] = Some(StageKind::Drive);
            p.drive_model = *model;
            p.drive_gain = 9.0;
            dsp.set_params(p);
            dsp.reset();
            assert_eq!(
                DriveModel::from_index(i as u32),
                *model,
                "ALL order / from_index drifted for {model:?}"
            );
            let mut peak: f32 = 0.0;
            for n in 0..4_000 {
                let x = (n as f32 * 0.03).sin() * 0.4;
                let (l, r) = dsp.process_stereo(x, x);
                assert!(l.is_finite() && r.is_finite(), "{model:?} NaN");
                if n > 2_000 {
                    peak = peak.max(l.abs());
                }
            }
            assert!(peak > 1.0e-3, "{model:?} produced silence");
            assert!(peak < 4.0, "{model:?} runaway gain: {peak}");
        }
    }

    /// A delay-time edit mid-stream must not click: the read head glides
    /// (tape-style pitch bend), so consecutive output samples stay close.
    #[test]
    fn delay_time_edits_do_not_click() {
        let mut dsp = Dsp::new(48_000.0);
        let mut p = default_params();
        p.stage_order = [None; PATH_SLOTS];
        p.stage_order[0] = Some(StageKind::Delay);
        p.delay_mix = 100.0;
        p.delay_fb = 20.0;
        dsp.set_params(p);
        dsp.reset();

        let mut prev = 0.0f32;
        let mut max_step = 0.0f32;
        for n in 0..96_000 {
            // Steady tone in; jump the delay time knob mid-stream.
            if n == 48_000 {
                assert!(dsp.apply_ui_param("delay_time", 1_100.0));
            }
            let x = (n as f32 * 0.05).sin() * 0.4;
            let (l, _) = dsp.process_stereo(x, x);
            assert!(l.is_finite());
            if n > 48_000 {
                max_step = max_step.max((l - prev).abs());
            }
            prev = l;
        }
        // A hard head jump on a 0.4 tone produces near full-scale steps; the
        // slewed head keeps sample-to-sample motion in normal signal range.
        assert!(max_step < 0.2, "delay time edit clicked: step={max_step}");
    }

    /// A bare chain (every stage out of the path) must pass a signal through at
    /// exactly the combined trim gain, so the trims are usable for gain staging
    /// rather than being an approximate "level" control.
    fn bare_dsp() -> Dsp {
        let mut dsp = Dsp::new(48_000.0);
        let mut p = default_params();
        p.stage_order = [None; PATH_SLOTS];
        dsp.set_params(p);
        dsp
    }

    #[test]
    fn input_trim_scales_the_signal_entering_the_chain() {
        let mut dsp = bare_dsp();
        assert!(dsp.apply_ui_param("input_trim", 6.0));
        // Trims glide (~10 ms); snap to target so the assertion measures the
        // settled gain, not the first sample of the ramp.
        dsp.reset();
        let (l, _) = dsp.process_stereo(0.1, 0.1);
        // +6 dB ≈ ×1.9953.
        assert!((l - 0.199_53).abs() < 1.0e-3, "input trim not applied: {l}");
    }

    #[test]
    fn output_trim_scales_the_signal_leaving_the_chain() {
        let mut dsp = bare_dsp();
        assert!(dsp.apply_ui_param("output_trim", -6.0));
        dsp.reset();
        let (l, _) = dsp.process_stereo(0.4, 0.4);
        assert!(
            (l - 0.200_47).abs() < 1.0e-3,
            "output trim not applied: {l}"
        );
    }

    /// Live trim edits must glide, not jump — the zipper-noise guard for the
    /// now-wired editor knobs.
    #[test]
    fn trim_edits_glide_without_a_sample_step() {
        let mut dsp = bare_dsp();
        // Settle at unity.
        for _ in 0..256 {
            let _ = dsp.process_stereo(0.5, 0.5);
        }
        let (before, _) = dsp.process_stereo(0.5, 0.5);
        dsp.apply_ui_param("input_trim", 24.0); // worst-case jump: +24 dB
        let (first, _) = dsp.process_stereo(0.5, 0.5);
        // One sample later the output may move only a small fraction of the
        // full 16x step.
        assert!(
            (first - before).abs() < 0.05,
            "trim jumped {before} -> {first} in one sample"
        );
        // But it does converge to the target.
        let mut last = first;
        for _ in 0..48_000 {
            let (l, _) = dsp.process_stereo(0.5, 0.5);
            last = l;
        }
        let expected = 0.5 * db_to_linear(24.0);
        assert!(
            (last - expected).abs() / expected < 0.01,
            "trim did not converge: got {last}, expected {expected}"
        );
    }

    #[test]
    fn trims_are_clamped_to_their_declared_range() {
        let mut dsp = bare_dsp();
        dsp.apply_ui_param("input_trim", 999.0);
        dsp.apply_ui_param("output_trim", -999.0);
        assert_eq!(dsp.params().input_trim_db, 24.0);
        assert_eq!(dsp.params().output_trim_db, -24.0);
    }

    #[test]
    fn global_bypass_is_a_true_passthrough_ignoring_trims() {
        let mut dsp = bare_dsp();
        dsp.apply_ui_param("input_trim", 12.0);
        dsp.apply_ui_param("power", 0.0);
        let (l, r) = dsp.process_stereo(0.25, -0.25);
        assert_eq!((l, r), (0.25, -0.25));
    }

    #[test]
    fn input_meter_reports_the_post_trim_level() {
        let mut dsp = bare_dsp();
        dsp.apply_ui_param("input_trim", 6.0);
        dsp.reset();
        for _ in 0..64 {
            let _ = dsp.process_stereo(0.5, 0.5);
        }
        let frame = dsp.meter_frame();
        assert!(
            frame.in_peak > 0.9 && frame.in_peak < 1.05,
            "input meter should track post-trim level, got {}",
            frame.in_peak
        );
    }

    #[test]
    fn rms_of_a_dc_level_converges_to_that_level() {
        let mut dsp = bare_dsp();
        // 300 ms window at 48 kHz — run well past it.
        for _ in 0..48_000 {
            let _ = dsp.process_stereo(0.5, 0.5);
        }
        let frame = dsp.meter_frame();
        assert!(
            (frame.in_rms - 0.5).abs() < 0.01,
            "input rms should converge to 0.5, got {}",
            frame.in_rms
        );
        assert!(frame.in_rms <= frame.in_peak + 1.0e-4);
    }

    // ---- Mod-slot models (chorus / phaser / flanger / tremolo) and Wah ----

    fn mod_only_dsp(model: ModModel) -> Dsp {
        let mut dsp = Dsp::new(48_000.0);
        let mut p = default_params();
        p.stage_order = [None; PATH_SLOTS];
        p.stage_order[0] = Some(StageKind::Mod);
        p.mod_model = model;
        p.chorus_rate = 6.0;
        p.chorus_depth = 7.0;
        p.chorus_mix = 70.0;
        dsp.set_params(p);
        dsp.reset();
        dsp
    }

    /// Every mod model must process finite, non-silent output.
    #[test]
    fn every_mod_model_processes_finite_and_audible() {
        for (i, model) in ModModel::ALL.iter().enumerate() {
            assert_eq!(
                ModModel::from_index(i as u32),
                *model,
                "ALL order / from_index drifted for {model:?}"
            );
            let mut dsp = mod_only_dsp(*model);
            let mut peak: f32 = 0.0;
            for n in 0..12_000 {
                let x = (n as f32 * 0.05).sin() * 0.4;
                let (l, r) = dsp.process_stereo(x, x);
                assert!(l.is_finite() && r.is_finite(), "{model:?} NaN");
                if n > 4_000 {
                    peak = peak.max(l.abs());
                }
            }
            assert!(peak > 1.0e-3, "{model:?} produced silence");
            assert!(peak < 4.0, "{model:?} runaway gain: {peak}");
        }
    }

    /// The four mod algorithms must not sound the same.
    #[test]
    fn mod_models_are_mutually_distinct() {
        let mut outputs: Vec<Vec<f32>> = Vec::new();
        for model in ModModel::ALL {
            let mut dsp = mod_only_dsp(*model);
            let mut buf = Vec::new();
            for n in 0..24_000 {
                let x = (n as f32 * 0.05).sin() * 0.4;
                let (l, _) = dsp.process_stereo(x, x);
                if n > 8_000 {
                    buf.push(l);
                }
            }
            outputs.push(buf);
        }
        for i in 0..outputs.len() {
            for j in (i + 1)..outputs.len() {
                let diff: f32 = outputs[i]
                    .iter()
                    .zip(&outputs[j])
                    .map(|(a, b)| (a - b).powi(2))
                    .sum::<f32>()
                    / outputs[i].len() as f32;
                assert!(
                    diff.sqrt() > 0.005,
                    "{:?} and {:?} sound effectively identical (rms diff {})",
                    ModModel::ALL[i],
                    ModModel::ALL[j],
                    diff.sqrt()
                );
            }
        }
    }

    /// Tremolo must actually modulate amplitude: over one slow LFO cycle the
    /// per-window envelope has to rise and fall well outside measurement noise.
    #[test]
    fn tremolo_modulates_amplitude() {
        let mut dsp = mod_only_dsp(ModModel::Tremolo);
        assert!(dsp.apply_ui_param("chorus_rate", 2.0)); // slow-ish
        assert!(dsp.apply_ui_param("chorus_depth", 9.0));
        let mut window_peaks = Vec::new();
        let mut peak: f32 = 0.0;
        for n in 0..96_000 {
            let x = (n as f32 * 0.08).sin() * 0.5;
            let (l, _) = dsp.process_stereo(x, x);
            peak = peak.max(l.abs());
            if n % 2_400 == 2_399 {
                window_peaks.push(peak);
                peak = 0.0;
            }
        }
        let hi = window_peaks.iter().cloned().fold(0.0f32, f32::max);
        let lo = window_peaks.iter().cloned().fold(f32::MAX, f32::min);
        assert!(hi > lo * 1.5, "tremolo did not modulate: hi={hi} lo={lo}");
    }

    fn wah_only_dsp(model: WahModel, pos: f32) -> Dsp {
        let mut dsp = Dsp::new(48_000.0);
        let mut p = default_params();
        p.stage_order = [None; PATH_SLOTS];
        p.stage_order[0] = Some(StageKind::Wah);
        p.wah_model = model;
        p.wah_pos = pos;
        p.wah_res = 6.0;
        p.wah_sens = 7.0;
        dsp.set_params(p);
        dsp.reset();
        dsp
    }

    /// Both wah models stay finite and audible for normal input.
    #[test]
    fn wah_models_process_finite_and_audible() {
        for model in WahModel::ALL {
            let mut dsp = wah_only_dsp(*model, 5.0);
            let mut peak: f32 = 0.0;
            for n in 0..24_000 {
                let x = (n as f32 * 0.06).sin() * 0.4;
                let (l, r) = dsp.process_stereo(x, x);
                assert!(l.is_finite() && r.is_finite(), "{model:?} NaN");
                if n > 8_000 {
                    peak = peak.max(l.abs());
                }
            }
            assert!(peak > 1.0e-3, "{model:?} produced silence");
            assert!(peak < 6.0, "{model:?} runaway resonance: {peak}");
        }
    }

    /// The pedal position must actually move the filter: heel and toe settings
    /// pass a fixed low tone with materially different gain.
    #[test]
    fn cry_wah_position_sweeps_the_filter() {
        let level_at = |pos: f32| {
            let mut dsp = wah_only_dsp(WahModel::CryWah, pos);
            let mut peak: f32 = 0.0;
            for n in 0..24_000 {
                // ~330 Hz — near the heel resonance, far below the toe.
                let x = (n as f32 * 2.0 * std::f32::consts::PI * 330.0 / 48_000.0).sin() * 0.3;
                let (l, _) = dsp.process_stereo(x, x);
                if n > 12_000 {
                    peak = peak.max(l.abs());
                }
            }
            peak
        };
        let heel = level_at(0.5);
        let toe = level_at(9.5);
        assert!(
            heel > toe * 1.5,
            "wah position had no effect at 330 Hz: heel={heel} toe={toe}"
        );
    }

    /// Touch wah reacts to level: a loud passage must sweep the filter away
    /// from where a quiet passage sits (different spectral gain at the probe).
    #[test]
    fn touch_wah_follows_the_envelope() {
        let mut dsp = wah_only_dsp(WahModel::TouchWah, 2.0);
        let probe = |dsp: &mut Dsp, amp: f32| {
            let mut peak: f32 = 0.0;
            for n in 0..24_000 {
                let x = (n as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * amp;
                let (l, _) = dsp.process_stereo(x, x);
                if n > 12_000 {
                    peak = peak.max(l.abs() / amp); // normalized gain
                }
            }
            peak
        };
        let quiet_gain = probe(&mut dsp, 0.02);
        dsp.reset();
        let loud_gain = probe(&mut dsp, 0.6);
        assert!(
            (quiet_gain - loud_gain).abs() > quiet_gain * 0.2,
            "touch wah ignored level: quiet={quiet_gain} loud={loud_gain}"
        );
    }

    /// Mod model switching mid-stream stays finite and produces the newly
    /// selected sound (regression guard for stale cross-model state).
    #[test]
    fn mod_model_switching_is_glitch_safe() {
        let mut dsp = mod_only_dsp(ModModel::Chorus);
        let count = ModModel::ALL.len();
        for n in 0..(8_000 * count) {
            if n % 8_000 == 0 {
                let next = ModModel::from_index(((n / 8_000) % count) as u32);
                assert!(dsp.select_model("mod", mod_model_id(next)));
            }
            let x = (n as f32 * 0.05).sin() * 0.4;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite(), "switch glitch at {n}");
        }
    }

    /// Editor id for a mod model — the inverse of
    /// [`ModModel::from_model_id`], kept here because only the tests need it.
    fn mod_model_id(model: ModModel) -> &'static str {
        match model {
            ModModel::Chorus => "chorus",
            ModModel::Phaser => "phaser",
            ModModel::Flanger => "flanger",
            ModModel::Tremolo => "tremolo",
            ModModel::MolamSwirl => "molam_swirl",
            ModModel::PhinVibe => "phin_vibe",
            ModModel::KhaenSwirl => "khaen_swirl",
            ModModel::BiLam => "bi_lam",
            ModModel::IsanJet => "isan_jet",
            ModModel::SoftPhase => "soft_phase",
            ModModel::WideVibe => "wide_vibe",
        }
    }

    /// Every mod model must be reachable by the id the editor sends, and land
    /// on the index the wire carries.
    #[test]
    fn every_mod_model_round_trips_through_its_editor_id() {
        for (i, &model) in ModModel::ALL.iter().enumerate() {
            let id = mod_model_id(model);
            assert_eq!(ModModel::from_model_id(id), Some(model), "`{id}`");
            assert_eq!(ModModel::from_index(i as u32), model, "index {i}");
        }
    }

    #[test]
    fn clip_flags_are_sticky_until_cleared() {
        let mut dsp = bare_dsp();
        let _ = dsp.process_stereo(0.2, 0.2);
        assert!(!dsp.meter_frame().in_clip);

        let _ = dsp.process_stereo(1.5, 1.5);
        // Still set several quiet samples later.
        for _ in 0..512 {
            let _ = dsp.process_stereo(0.01, 0.01);
        }
        assert!(dsp.meter_frame().in_clip, "clip flag should latch");
        assert!(dsp.meter_frame().out_clip);

        dsp.clear_clip();
        assert!(!dsp.meter_frame().in_clip);
        assert!(!dsp.meter_frame().out_clip);
    }
}
