//! A synthetic SoundFont for tests.
//!
//! Behind the `test-support` feature so tests in this crate and in the audio
//! engine can exercise the real load → preset select → render path without
//! shipping (or depending on the presence of) a multi-megabyte `.sf2` file.
//!
//! The bank is deliberately minimal but valid SF2: one looping sine sample,
//! one instrument, and presets at the banks a General MIDI player must handle —
//! a melodic bank 0 preset and a drum bank 128 preset.

use std::sync::Arc;

use rustysynth::SoundFont;

/// Bank/patch of the melodic preset in [`sound_font`].
pub const MELODIC_PRESET: (i32, i32) = (0, 0);
/// Bank/patch of the drum preset in [`sound_font`], reachable only on the
/// percussion channel. Its instrument is a stereo pair — a zone panned hard
/// left and one hard right — in exclusive class [`DRUM_CHOKE_GROUP`], the way
/// a General MIDI kit's hi-hats are.
pub const DRUM_PRESET: (i32, i32) = (128, 0);
/// The drum preset's exclusive class (choke group).
pub const DRUM_CHOKE_GROUP: u16 = 1;
/// Bank name reported by [`sound_font`].
pub const BANK_NAME: &str = "Futureboard Test Bank";

const SAMPLE_RATE: i32 = 44_100;
const SAMPLE_FRAMES: usize = 2_048;
/// SF2 requires at least 46 zero frames of separation after each sample.
const SAMPLE_PADDING: usize = 46;

const GEN_PAN: u16 = 17;
const GEN_INSTRUMENT: u16 = 41;
const GEN_KEY_RANGE: u16 = 43;
const GEN_SAMPLE_ID: u16 = 53;
const GEN_SAMPLE_MODES: u16 = 54;
const GEN_EXCLUSIVE_CLASS: u16 = 57;
const GEN_OVERRIDING_ROOT_KEY: u16 = 58;

/// Serialized bytes of the synthetic bank.
pub fn sf2_bytes() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"sfbk");
    body.extend_from_slice(&info_list());
    body.extend_from_slice(&sdta_list());
    body.extend_from_slice(&pdta_list());

    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// The synthetic bank, parsed.
pub fn sound_font() -> Arc<SoundFont> {
    let bytes = sf2_bytes();
    Arc::new(SoundFont::new(&mut std::io::Cursor::new(bytes)).expect("synthetic SoundFont parses"))
}

/// Writes the synthetic bank to `path` so tests can exercise file loading.
pub fn write_sf2(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::write(path, sf2_bytes())
}

fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 9);
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn list_chunk(list_type: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(data.len() + 4);
    body.extend_from_slice(list_type);
    body.extend_from_slice(data);
    chunk(b"LIST", &body)
}

/// A NUL-terminated, even-length string field.
fn zstr(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    if bytes.len() % 2 == 1 {
        bytes.push(0);
    }
    bytes
}

/// A fixed 20-byte name field, as used by phdr / inst / shdr records.
fn name20(value: &str) -> [u8; 20] {
    let mut out = [0u8; 20];
    let bytes = value.as_bytes();
    let len = bytes.len().min(19);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

fn info_list() -> Vec<u8> {
    let mut data = Vec::new();
    let mut ifil = Vec::new();
    ifil.extend_from_slice(&2i16.to_le_bytes());
    ifil.extend_from_slice(&1i16.to_le_bytes());
    data.extend_from_slice(&chunk(b"ifil", &ifil));
    data.extend_from_slice(&chunk(b"isng", &zstr("EMU8000")));
    data.extend_from_slice(&chunk(b"INAM", &zstr(BANK_NAME)));
    list_chunk(b"INFO", &data)
}

fn sdta_list() -> Vec<u8> {
    let mut samples = Vec::with_capacity((SAMPLE_FRAMES + SAMPLE_PADDING) * 2);
    for frame in 0..SAMPLE_FRAMES {
        // One full sine period across the sample, so looping it is continuous.
        let phase = frame as f64 / SAMPLE_FRAMES as f64 * std::f64::consts::TAU;
        let value = (phase.sin() * 24_000.0) as i16;
        samples.extend_from_slice(&value.to_le_bytes());
    }
    samples.extend(std::iter::repeat_n(0u8, SAMPLE_PADDING * 2));
    list_chunk(b"sdta", &chunk(b"smpl", &samples))
}

fn generator(kind: u16, value: u16) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&kind.to_le_bytes());
    out[2..].copy_from_slice(&value.to_le_bytes());
    out
}

fn zone(generator_index: u16) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&generator_index.to_le_bytes());
    out[2..].copy_from_slice(&0u16.to_le_bytes());
    out
}

/// One `phdr` record: name, patch, bank, first preset zone, then the unused
/// library / genre / morphology fields.
fn preset_header(name: &str, patch: u16, bank: u16, zone_start: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(38);
    out.extend_from_slice(&name20(name));
    out.extend_from_slice(&patch.to_le_bytes());
    out.extend_from_slice(&bank.to_le_bytes());
    out.extend_from_slice(&zone_start.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out
}

fn instrument_header(name: &str, zone_start: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(22);
    out.extend_from_slice(&name20(name));
    out.extend_from_slice(&zone_start.to_le_bytes());
    out
}

fn pdta_list() -> Vec<u8> {
    // Two presets (melodic + drum) sharing one instrument, plus the terminator
    // record each SF2 list ends with.
    let mut phdr = Vec::new();
    phdr.extend_from_slice(&preset_header(
        "Test Tone",
        MELODIC_PRESET.1 as u16,
        MELODIC_PRESET.0 as u16,
        0,
    ));
    phdr.extend_from_slice(&preset_header(
        "Test Kit",
        DRUM_PRESET.1 as u16,
        DRUM_PRESET.0 as u16,
        1,
    ));
    phdr.extend_from_slice(&preset_header("EOP", 0, 0, 2));

    let mut pbag = Vec::new();
    pbag.extend_from_slice(&zone(0));
    pbag.extend_from_slice(&zone(1));
    pbag.extend_from_slice(&zone(2));

    let mut pgen = Vec::new();
    pgen.extend_from_slice(&generator(GEN_INSTRUMENT, 0));
    pgen.extend_from_slice(&generator(GEN_INSTRUMENT, 1));
    pgen.extend_from_slice(&generator(0, 0)); // terminator

    let mut inst = Vec::new();
    inst.extend_from_slice(&instrument_header("Test Instrument", 0));
    inst.extend_from_slice(&instrument_header("Test Kit Pair", 1));
    inst.extend_from_slice(&instrument_header("EOI", 3));

    // Zone 0: the tone. Zones 1 and 2: the kit's left and right halves.
    let mut ibag = Vec::new();
    ibag.extend_from_slice(&zone(0));
    ibag.extend_from_slice(&zone(4));
    ibag.extend_from_slice(&zone(10));
    ibag.extend_from_slice(&zone(16));

    let mut igen = Vec::new();
    igen.extend_from_slice(&generator(GEN_KEY_RANGE, 0x7F00));
    igen.extend_from_slice(&generator(GEN_OVERRIDING_ROOT_KEY, 60));
    igen.extend_from_slice(&generator(GEN_SAMPLE_MODES, 1)); // loop continuously
    igen.extend_from_slice(&generator(GEN_SAMPLE_ID, 0)); // must be last in the zone
    for pan in [-500i16, 500] {
        igen.extend_from_slice(&generator(GEN_KEY_RANGE, 0x7F00));
        igen.extend_from_slice(&generator(GEN_PAN, pan as u16));
        igen.extend_from_slice(&generator(GEN_EXCLUSIVE_CLASS, DRUM_CHOKE_GROUP));
        igen.extend_from_slice(&generator(GEN_OVERRIDING_ROOT_KEY, 60));
        igen.extend_from_slice(&generator(GEN_SAMPLE_MODES, 1));
        igen.extend_from_slice(&generator(GEN_SAMPLE_ID, 0));
    }
    igen.extend_from_slice(&generator(0, 0)); // terminator

    let mut shdr = Vec::new();
    shdr.extend_from_slice(&sample_header(
        "Sine",
        0,
        SAMPLE_FRAMES as i32,
        0,
        SAMPLE_FRAMES as i32 - 1,
    ));
    shdr.extend_from_slice(&sample_header("EOS", 0, 0, 0, 0));

    let mut data = Vec::new();
    data.extend_from_slice(&chunk(b"phdr", &phdr));
    data.extend_from_slice(&chunk(b"pbag", &pbag));
    data.extend_from_slice(&chunk(b"pmod", &[0u8; 10]));
    data.extend_from_slice(&chunk(b"pgen", &pgen));
    data.extend_from_slice(&chunk(b"inst", &inst));
    data.extend_from_slice(&chunk(b"ibag", &ibag));
    data.extend_from_slice(&chunk(b"imod", &[0u8; 10]));
    data.extend_from_slice(&chunk(b"igen", &igen));
    data.extend_from_slice(&chunk(b"shdr", &shdr));
    list_chunk(b"pdta", &data)
}

fn sample_header(name: &str, start: i32, end: i32, start_loop: i32, end_loop: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(46);
    out.extend_from_slice(&name20(name));
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&end.to_le_bytes());
    out.extend_from_slice(&start_loop.to_le_bytes());
    out.extend_from_slice(&end_loop.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.push(60); // original pitch
    out.push(0); // pitch correction
    out.extend_from_slice(&0u16.to_le_bytes()); // sample link
    out.extend_from_slice(&1u16.to_le_bytes()); // monoSample
    out
}
