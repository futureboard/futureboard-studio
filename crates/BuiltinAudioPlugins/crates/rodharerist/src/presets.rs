//! Rodhareist's factory preset bank.
//!
//! Every preset except Blank is a complete rig on one signal path, in
//! pedalboard order:
//!
//! ```txt
//! Gate → Comp → Wah → Phaser → Drive → Amp → Cab → EQ → Mod → Delay → Reverb
//! ```
//!
//! A preset switches on the blocks its sound uses; the rest stay on the path
//! bypassed, already set to something musical, so stepping on one never
//! lands on a silly setting. The phaser is the second modulation block
//! (`Mod B`), set to the Vibe Phase model and placed before the drive, where
//! a phase pedal sits on a real board.
//!
//! Each preset is a rig a player would actually leave on a board: the amp
//! does the work, pedals sit at musical settings, and time effects stay
//! low enough to play through. Levels are measured, not guessed: every
//! preset's output trim is the value `examples/preset_audit.rs` reports
//! for it. Re-run the audit after changing any preset's sound and copy
//! its `trim` column back here.
//!
//! Built once, on first use, off the audio thread.

use std::sync::OnceLock;

use crate::dsp::{
    AmpModel, CabModel, DelayModel, DriveModel, EqModel, Line2Mode, MicModel, ModModel, PATH_SLOTS,
    Params, ReverbModel, StageKind, ToneEngineKind, WahModel, default_params,
};

/// One factory preset.
#[derive(Debug, Clone)]
pub struct FactoryPreset {
    /// Bank position: group number and letter, e.g. `"03B"`.
    pub id: &'static str,
    pub name: &'static str,
    pub params: Params,
}

/// The factory bank, in bank order.
pub fn factory_presets() -> &'static [FactoryPreset] {
    static BANK: OnceLock<Vec<FactoryPreset>> = OnceLock::new();
    BANK.get_or_init(bank)
}

/// The signal path every preset uses.
pub const RIG_PATH: [StageKind; 11] = [
    StageKind::Gate,
    StageKind::Comp,
    StageKind::Wah,
    StageKind::Mod2,
    StageKind::Drive,
    StageKind::Amp,
    StageKind::Cab,
    StageKind::Eq,
    StageKind::Mod,
    StageKind::Delay,
    StageKind::Reverb,
];

/// The rig every preset starts from: the full path, every effect bypassed
/// and set to a usable middle-of-the-road sound.
fn base() -> Params {
    let mut p = default_params();
    p.stage_order = [None; PATH_SLOTS];
    for (slot, kind) in RIG_PATH.iter().enumerate() {
        p.stage_order[slot] = Some(*kind);
    }
    p.output_trim_db = 0.0;
    p.tone_engine = ToneEngineKind::Classic;

    p.gate_on = false;
    p.gate_thresh_db = -58.0;

    p.comp_on = false;
    p.comp_thresh_db = -20.0;
    p.comp_ratio = 3.0;
    p.comp_attack_ms = 15.0;
    p.comp_release_ms = 150.0;
    p.comp_makeup_db = 3.0;

    p.wah_on = false;
    p.wah_model = WahModel::CryWah;
    p.wah_pos = 5.0;
    p.wah_res = 5.5;
    p.wah_sens = 5.5;

    let b = &mut p.stage_b;
    b.mod_on = false;
    b.mod_model = ModModel::Phaser;
    b.chorus_rate = 3.0;
    b.chorus_depth = 5.5;
    b.chorus_mix = 50.0;
    // The second drive/delay/EQ/comp blocks are not on the rig path.
    b.drive_on = false;
    b.delay_on = false;
    b.eq_on = false;
    b.comp_on = false;

    p.drive_on = false;
    p.drive_model = DriveModel::Screamer;
    p.drive_gain = 3.0;
    p.drive_tone = 5.5;
    p.drive_level = 7.0;

    p.amp_on = true;
    p.cab_on = true;
    p.mic_model = MicModel::Dynamic;

    p.eq_on = false;
    p.eq_model = EqModel::Studio;
    p.eq_low_gain_db = 0.0;
    p.eq_mid1_freq_hz = 400.0;
    p.eq_mid1_gain_db = 0.0;
    p.eq_mid2_freq_hz = 2000.0;
    p.eq_mid2_gain_db = 0.0;
    p.eq_high_gain_db = 0.0;

    p.mod_on = false;
    p.mod_model = ModModel::Chorus;
    p.chorus_rate = 3.0;
    p.chorus_depth = 5.0;
    p.chorus_mix = 30.0;

    p.delay_on = false;
    p.delay_model = DelayModel::Analog;
    p.delay_time_ms = 380.0;
    p.delay_fb = 25.0;
    p.delay_mix = 18.0;
    p.delay_tone = 5.0;

    p.reverb_on = false;
    p.reverb_model = ReverbModel::Room;
    p.reverb_decay_s = 1.6;
    p.reverb_mix = 15.0;
    p.reverb_shimmer = 0.0;
    p
}

/// Amp: model, then gain, bass, middle, treble, presence, master (0..10).
fn amp(p: &mut Params, model: AmpModel, [gain, bass, mid, treble, presence, master]: [f32; 6]) {
    p.amp_model = model;
    p.amp_gain = gain;
    p.amp_bass = bass;
    p.amp_middle = mid;
    p.amp_treble = treble;
    p.amp_presence = presence;
    p.amp_master = master;
}

/// Cab: model, mic, mic position and distance (0..100 %).
fn cab(p: &mut Params, model: CabModel, mic: MicModel, position: f32, distance: f32) {
    p.cab_model = model;
    p.mic_model = mic;
    p.cab_mic = position;
    p.cab_dist = distance;
}

/// Drive pedal on: model, gain, tone, level (0..10).
fn drive(p: &mut Params, model: DriveModel, gain: f32, tone: f32, level: f32) {
    p.drive_on = true;
    p.drive_model = model;
    p.drive_gain = gain;
    p.drive_tone = tone;
    p.drive_level = level;
}

/// Compressor on: threshold dB, ratio, attack ms, release ms, makeup dB.
fn comp(p: &mut Params, thresh: f32, ratio: f32, attack: f32, release: f32, makeup: f32) {
    p.comp_on = true;
    p.comp_thresh_db = thresh;
    p.comp_ratio = ratio;
    p.comp_attack_ms = attack;
    p.comp_release_ms = release;
    p.comp_makeup_db = makeup;
}

/// Mod A on: model, rate, depth (0..10), mix %.
fn modulation(p: &mut Params, model: ModModel, rate: f32, depth: f32, mix: f32) {
    p.mod_on = true;
    p.mod_model = model;
    p.chorus_rate = rate;
    p.chorus_depth = depth;
    p.chorus_mix = mix;
}

/// Phaser (Mod B) on: rate, depth (0..10), mix %.
fn phaser(p: &mut Params, rate: f32, depth: f32, mix: f32) {
    p.stage_b.mod_on = true;
    p.stage_b.mod_model = ModModel::Phaser;
    p.stage_b.chorus_rate = rate;
    p.stage_b.chorus_depth = depth;
    p.stage_b.chorus_mix = mix;
}

/// Delay on: model, time ms, feedback %, mix %, tone (0..10).
fn delay(p: &mut Params, model: DelayModel, time: f32, fb: f32, mix: f32, tone: f32) {
    p.delay_on = true;
    p.delay_model = model;
    p.delay_time_ms = time;
    p.delay_fb = fb;
    p.delay_mix = mix;
    p.delay_tone = tone;
}

/// Reverb on: model, decay s, mix %.
fn reverb(p: &mut Params, model: ReverbModel, decay: f32, mix: f32) {
    p.reverb_on = true;
    p.reverb_model = model;
    p.reverb_decay_s = decay;
    p.reverb_mix = mix;
}

/// EQ on: low dB, mid 1 (Hz, dB), mid 2 (Hz, dB), high dB.
fn eq(p: &mut Params, low: f32, mid1: (f32, f32), mid2: (f32, f32), high: f32) {
    p.eq_on = true;
    p.eq_low_gain_db = low;
    p.eq_mid1_freq_hz = mid1.0;
    p.eq_mid1_gain_db = mid1.1;
    p.eq_mid2_freq_hz = mid2.0;
    p.eq_mid2_gain_db = mid2.1;
    p.eq_high_gain_db = high;
}

fn gate(p: &mut Params, thresh: f32) {
    p.gate_on = true;
    p.gate_thresh_db = thresh;
}

/// Wah on: model, pedal position, resonance, sensitivity (0..10).
fn wah(p: &mut Params, model: WahModel, position: f32, resonance: f32, sensitivity: f32) {
    p.wah_on = true;
    p.wah_model = model;
    p.wah_pos = position;
    p.wah_res = resonance;
    p.wah_sens = sensitivity;
}

fn line2(p: &mut Params, mode: Line2Mode, mix: f32) {
    p.line2_mode = mode;
    p.line2_mix = mix;
}

fn preset(
    id: &'static str,
    name: &'static str,
    trim_db: f32,
    voice: impl FnOnce(&mut Params),
) -> FactoryPreset {
    let mut params = base();
    voice(&mut params);
    params.output_trim_db = trim_db;
    FactoryPreset { id, name, params }
}

use AmpModel as A;
use CabModel as C;
use DelayModel as D;
use DriveModel as Dr;
use MicModel as M;
use ModModel as Mo;
use ReverbModel as R;

fn bank() -> Vec<FactoryPreset> {
    vec![
        preset("00A", "Blank", 0.0, |p| {
            p.stage_order = [None; PATH_SLOTS];
            p.amp_on = false;
            p.cab_on = false;
        }),
        // ── 01 Clean ────────────────────────────────────────────────────────
        preset("01A", "Studio Twin", 6.0, |p| {
            comp(p, -18.0, 2.2, 14.0, 140.0, 2.0);
            amp(p, A::Twin, [3.2, 4.5, 5.0, 6.0, 4.5, 6.5]);
            cab(p, C::American2x12, M::Dynamic, 40.0, 22.0);
            reverb(p, R::Room, 1.4, 14.0);
        }),
        preset("01B", "Princeton Jazz", 4.5, |p| {
            comp(p, -16.0, 2.0, 20.0, 180.0, 1.5);
            amp(p, A::Princeton, [4.2, 5.2, 5.8, 5.0, 3.8, 5.8]);
            cab(p, C::American1x12, M::Ribbon, 32.0, 36.0);
            eq(p, 0.5, (350.0, 0.8), (2800.0, -1.0), -0.5);
            reverb(p, R::Room, 1.3, 12.0);
        }),
        preset("01C", "Country Slap", 7.0, |p| {
            comp(p, -22.0, 3.0, 8.0, 90.0, 2.5);
            amp(p, A::Twin, [3.4, 4.2, 4.8, 6.5, 5.0, 6.5]);
            cab(p, C::American2x12, M::Dynamic, 52.0, 18.0);
            delay(p, D::Tape, 110.0, 8.0, 14.0, 6.0);
            reverb(p, R::Room, 1.2, 10.0);
        }),
        preset("01D", "Funk Touch", 7.5, |p| {
            comp(p, -20.0, 2.5, 12.0, 110.0, 2.0);
            wah(p, WahModel::TouchWah, 3.5, 5.0, 5.5);
            amp(p, A::Twin, [2.8, 4.2, 5.2, 6.2, 4.8, 6.8]);
            cab(p, C::American2x12, M::Dynamic, 46.0, 20.0);
            reverb(p, R::Room, 1.0, 8.0);
        }),
        // ── 02 Edge ─────────────────────────────────────────────────────────
        preset("02A", "Jangle 15", 3.5, |p| {
            amp(p, A::Normal15, [4.0, 4.2, 6.2, 6.8, 5.2, 6.0]);
            cab(p, C::OpenBack, M::Condenser, 44.0, 32.0);
            modulation(p, Mo::Chorus, 2.4, 3.8, 18.0);
            reverb(p, R::Room, 1.5, 12.0);
        }),
        preset("02B", "Hundred Clean", 4.0, |p| {
            amp(p, A::Hundred, [3.6, 4.0, 4.8, 6.2, 5.8, 6.2]);
            cab(p, C::Brit4x12, M::Dynamic, 36.0, 20.0);
            reverb(p, R::Room, 1.3, 10.0);
        }),
        preset("02C", "Surf Trem", 2.0, |p| {
            amp(p, A::TopBoost, [4.4, 4.5, 6.0, 6.8, 5.5, 6.0]);
            cab(p, C::OpenBack, M::Ribbon, 38.0, 34.0);
            modulation(p, Mo::Tremolo, 4.0, 4.5, 38.0);
            reverb(p, R::Hall, 2.2, 16.0);
        }),
        preset("02D", "Phase Rhythm", 8.5, |p| {
            phaser(p, 3.2, 4.5, 32.0);
            amp(p, A::Twin, [3.0, 4.2, 5.0, 6.2, 4.8, 6.5]);
            cab(p, C::American2x12, M::Dynamic, 48.0, 20.0);
            reverb(p, R::Room, 1.2, 10.0);
        }),
        // ── 03 Crunch ───────────────────────────────────────────────────────
        preset("03A", "Tweed Edge", 1.0, |p| {
            amp(p, A::TweedCombo, [6.2, 5.5, 5.8, 5.2, 4.0, 5.2]);
            cab(p, C::Tweed1x12, M::Ribbon, 28.0, 34.0);
            reverb(p, R::Room, 1.2, 10.0);
        }),
        preset("03B", "Bluesbreaker", 1.5, |p| {
            drive(p, Dr::Breaker, 2.8, 5.2, 6.5);
            amp(p, A::Bassman, [5.2, 5.2, 6.0, 5.5, 4.2, 5.5]);
            cab(p, C::Tweed1x12, M::Ribbon, 26.0, 36.0);
            delay(p, D::Analog, 320.0, 16.0, 12.0, 4.5);
            reverb(p, R::Room, 1.3, 12.0);
        }),
        preset("03C", "Plexi Crunch", -1.5, |p| {
            amp(p, A::Plexi, [6.0, 4.5, 6.2, 6.5, 5.5, 5.5]);
            cab(p, C::Brit4x12, M::Dynamic, 38.0, 18.0);
            reverb(p, R::Room, 1.2, 8.0);
        }),
        preset("03D", "Mandarin", -2.0, |p| {
            amp(p, A::Mandarin, [5.8, 5.2, 6.2, 5.5, 4.8, 5.5]);
            cab(p, C::Vintage2x12, M::Ribbon, 32.0, 26.0);
            reverb(p, R::Room, 1.3, 10.0);
        }),
        preset("03E", "Wah Rock", 2.0, |p| {
            wah(p, WahModel::CryWah, 4.8, 5.5, 5.0);
            amp(p, A::Jcm, [5.5, 4.5, 6.2, 6.2, 5.2, 5.5]);
            cab(p, C::Brit4x12, M::Dynamic, 40.0, 18.0);
            delay(p, D::Analog, 340.0, 18.0, 14.0, 4.5);
            reverb(p, R::Room, 1.3, 10.0);
        }),
        // ── 04 Rhythm ───────────────────────────────────────────────────────
        preset("04A", "JCM Rhythm", 0.0, |p| {
            gate(p, -52.0);
            drive(p, Dr::Screamer, 1.2, 5.5, 7.5);
            amp(p, A::Jcm, [6.5, 4.5, 6.5, 6.0, 5.5, 5.0]);
            cab(p, C::Brit4x12, M::Dynamic, 36.0, 18.0);
        }),
        preset("04B", "Recto Tight", 2.0, |p| {
            gate(p, -48.0);
            drive(p, Dr::Screamer, 1.0, 5.5, 8.0);
            amp(p, A::Recto, [6.5, 4.5, 4.0, 6.0, 6.0, 4.8]);
            cab(p, C::Oversized4x12, M::Dynamic, 44.0, 14.0);
            eq(p, -1.5, (320.0, -1.0), (1800.0, 1.2), 0.0);
        }),
        preset("04C", "Mark Lead", 0.0, |p| {
            gate(p, -46.0);
            amp(p, A::Mark, [6.8, 4.2, 6.5, 5.5, 5.8, 4.6]);
            cab(p, C::Modern4x12, M::Dynamic, 42.0, 16.0);
            eq(p, -1.5, (800.0, 1.5), (2400.0, 1.0), 0.5);
        }),
        preset("04D", "Invader", 4.0, |p| {
            gate(p, -48.0);
            drive(p, Dr::Screamer, 0.8, 6.0, 7.5);
            amp(p, A::Invader, [7.0, 4.2, 3.5, 6.2, 6.2, 4.4]);
            cab(p, C::Uber4x12, M::Dynamic, 46.0, 12.0);
            eq(p, -1.5, (280.0, -1.2), (1700.0, 1.2), 0.5);
        }),
        // ── 05 Lead ─────────────────────────────────────────────────────────
        preset("05A", "Plexi Lead", -0.5, |p| {
            amp(p, A::Plexi, [6.5, 4.2, 6.5, 6.2, 5.5, 5.5]);
            cab(p, C::Brit4x12, M::Ribbon, 34.0, 24.0);
            delay(p, D::Tape, 380.0, 22.0, 16.0, 4.5);
            reverb(p, R::Plate, 2.4, 12.0);
        }),
        preset("05B", "Wide Lead", 4.0, |p| {
            amp(p, A::Boutique, [5.2, 4.8, 6.2, 5.5, 4.8, 5.8]);
            cab(p, C::Vintage2x12, M::Ribbon, 36.0, 26.0);
            delay(p, D::Digital, 400.0, 24.0, 18.0, 5.0);
            reverb(p, R::Plate, 2.6, 14.0);
            line2(p, Line2Mode::WetDryWet, 60.0);
        }),
        preset("05C", "Slate Lead", 2.0, |p| {
            amp(p, A::Slate, [6.4, 4.8, 5.8, 5.8, 5.5, 5.0]);
            cab(p, C::Slo4x12, M::Ribbon, 38.0, 22.0);
            delay(p, D::Analog, 360.0, 22.0, 16.0, 4.5);
            reverb(p, R::Plate, 2.8, 14.0);
        }),
        preset("05D", "Chorus Lead", 4.0, |p| {
            amp(p, A::Jcm, [6.0, 4.5, 6.0, 6.0, 5.5, 5.2]);
            cab(p, C::Brit4x12, M::Condenser, 40.0, 28.0);
            modulation(p, Mo::Chorus, 2.2, 4.0, 16.0);
            delay(p, D::Digital, 420.0, 22.0, 14.0, 5.5);
            reverb(p, R::Hall, 3.2, 14.0);
        }),
        preset("05E", "Fuzz Lead", -2.5, |p| {
            drive(p, Dr::Fuzz, 6.0, 4.2, 5.5);
            amp(p, A::Mandarin, [4.2, 5.0, 6.0, 5.2, 4.5, 5.2]);
            cab(p, C::Vintage2x12, M::Ribbon, 26.0, 28.0);
            delay(p, D::Analog, 400.0, 22.0, 14.0, 4.0);
            reverb(p, R::Room, 1.5, 12.0);
        }),
        // ── 06 Thai ─────────────────────────────────────────────────────────
        preset("06A", "Phin Echo", 4.0, |p| {
            drive(p, Dr::SuperDrive, 3.5, 5.8, 6.2);
            amp(p, A::Twin, [3.4, 4.0, 5.8, 6.2, 5.0, 6.5]);
            cab(p, C::OpenBack, M::Dynamic, 46.0, 26.0);
            eq(p, -1.5, (500.0, 1.2), (2400.0, 1.5), 0.5);
            delay(p, D::Tape, 280.0, 24.0, 16.0, 4.5);
            reverb(p, R::Room, 1.6, 12.0);
        }),
        preset("06B", "Molam", 11.0, |p| {
            drive(p, Dr::Breaker, 2.8, 5.5, 6.0);
            amp(p, A::Twin, [3.2, 4.2, 5.6, 6.0, 4.8, 6.5]);
            cab(p, C::OpenBack, M::Ribbon, 36.0, 32.0);
            modulation(p, Mo::MolamSwirl, 4.5, 5.0, 30.0);
            delay(p, D::Analog, 320.0, 20.0, 14.0, 4.2);
            reverb(p, R::Room, 1.6, 12.0);
        }),
        preset("06C", "Khaen", 10.0, |p| {
            amp(p, A::Twin, [2.8, 4.2, 5.2, 5.8, 4.6, 6.8]);
            cab(p, C::OpenBack, M::Condenser, 40.0, 36.0);
            modulation(p, Mo::KhaenSwirl, 3.8, 5.0, 28.0);
            delay(p, D::PingPong, 400.0, 22.0, 14.0, 4.5);
            reverb(p, R::Hall, 3.0, 14.0);
        }),
        // ── 07 Bass ─────────────────────────────────────────────────────────
        preset("07A", "Bass Foundation", 0.5, |p| {
            comp(p, -18.0, 4.0, 25.0, 160.0, 3.0);
            amp(p, A::Bassman, [3.6, 6.5, 5.0, 4.0, 3.5, 6.0]);
            cab(p, C::BassCabinet, M::Ribbon, 42.0, 28.0);
            eq(p, 1.0, (220.0, -1.5), (1200.0, 1.0), -1.0);
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_is_a_full_rig_with_a_unique_id() {
        let bank = factory_presets();
        assert!(bank.len() >= 20);
        let mut ids: Vec<&str> = bank.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), bank.len(), "duplicate preset id");
        for preset in bank {
            if preset.id == "00A" {
                assert!(preset.params.stage_order.iter().all(|slot| slot.is_none()));
                assert!(!preset.params.amp_on && !preset.params.cab_on);
                continue;
            }
            let path: Vec<StageKind> = preset
                .params
                .stage_order
                .iter()
                .flatten()
                .copied()
                .collect();
            assert_eq!(path, RIG_PATH, "{}", preset.name);
            assert!(
                preset.params.amp_on && preset.params.cab_on,
                "{}",
                preset.name
            );
        }
    }

    #[test]
    fn the_bank_covers_every_effect_family() {
        let bank = factory_presets();
        let any = |f: fn(&Params) -> bool| bank.iter().any(|p| f(&p.params));
        assert!(any(|p| p.comp_on));
        assert!(any(|p| p.wah_on && p.wah_model == WahModel::TouchWah));
        assert!(any(|p| p.wah_on && p.wah_model == WahModel::CryWah));
        assert!(any(
            |p| p.stage_b.mod_on && p.stage_b.mod_model == ModModel::Phaser
        ));
        assert!(any(|p| p.drive_on));
        assert!(any(|p| p.mod_on));
        assert!(any(|p| p.delay_on));
        assert!(any(|p| p.reverb_on));
        assert!(any(|p| p.eq_on));
        assert!(any(|p| p.gate_on));
    }

    #[test]
    fn every_value_is_inside_its_wire_range() {
        let ranges = crate::descriptor().params;
        for preset in factory_presets() {
            for (id, value) in crate::ui_values(&preset.params) {
                if let Some(d) = ranges.iter().find(|d| d.id == id) {
                    assert!(
                        (d.min..=d.max).contains(&value),
                        "{} {id} = {value} outside {}..{}",
                        preset.name,
                        d.min,
                        d.max
                    );
                }
            }
        }
    }

    #[test]
    fn the_bank_uses_the_new_amps_and_one_wet_dry_wet_rig() {
        let bank = factory_presets();
        assert!(
            bank.iter()
                .any(|p| p.params.amp_model == AmpModel::Princeton)
        );
        assert!(bank.iter().any(|p| p.params.amp_model == AmpModel::Hundred));
        assert!(bank.iter().any(|p| p.params.amp_model == AmpModel::Mark));
        assert!(
            bank.iter()
                .any(|p| p.params.amp_model == AmpModel::Normal15)
        );
        let wide: Vec<_> = bank
            .iter()
            .filter(|p| p.params.line2_mode == Line2Mode::WetDryWet)
            .collect();
        assert_eq!(wide.len(), 1);
        assert!((wide[0].params.line2_mix - 60.0).abs() < f32::EPSILON);
    }
}
