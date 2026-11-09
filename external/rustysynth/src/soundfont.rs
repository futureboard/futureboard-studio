#![allow(dead_code)]

use std::io::Read;

use crate::binary_reader::BinaryReader;
use crate::error::SoundFontError;
use crate::four_cc::FourCC;
use crate::instrument::Instrument;
use crate::preset::Preset;
use crate::sample_header::SampleHeader;
use crate::soundfont_info::SoundFontInfo;
use crate::soundfont_parameters::SoundFontParameters;
use crate::soundfont_sampledata::SoundFontSampleData;
use crate::LoopMode;

/// Reperesents a SoundFont.
#[derive(Debug)]
#[non_exhaustive]
pub struct SoundFont {
    pub(crate) info: SoundFontInfo,
    pub(crate) bits_per_sample: i32,
    pub(crate) wave_data: Vec<i16>,
    pub(crate) sample_headers: Vec<SampleHeader>,
    pub(crate) presets: Vec<Preset>,
    pub(crate) instruments: Vec<Instrument>,
}

impl SoundFont {
    /// Loads a SoundFont from the stream.
    ///
    /// # Arguments
    ///
    /// * `reader` - The data stream used to load the SoundFont.
    pub fn new<R: Read>(reader: &mut R) -> Result<Self, SoundFontError> {
        let chunk_id = BinaryReader::read_four_cc(reader)?;
        if chunk_id != b"RIFF" {
            return Err(SoundFontError::RiffChunkNotFound);
        }

        let _size = BinaryReader::read_i32(reader);

        let form_type = BinaryReader::read_four_cc(reader)?;
        if form_type != b"sfbk" {
            return Err(SoundFontError::InvalidRiffChunkType {
                expected: FourCC::from_bytes(*b"sfbk"),
                actual: form_type,
            });
        }

        let info = SoundFontInfo::new(reader)?;
        let sample_data = SoundFontSampleData::new(reader)?;
        let parameters = SoundFontParameters::new(reader)?;

        let mut sound_font = Self {
            info,
            bits_per_sample: sample_data.bits_per_sample,
            wave_data: sample_data.wave_data,
            sample_headers: parameters.sample_headers,
            presets: parameters.presets,
            instruments: parameters.instruments,
        };

        sound_font.sanitize_regions()?;

        Ok(sound_font)
    }

    /// Drop instrument regions that would panic the oscillator, instead of
    /// rejecting the whole bank. Real-world SF2 packs (Polyphone edits, GM
    /// banks) often leave inverted/empty loop points on NoLoop regions;
    /// FluidSynth-style loaders ignore those regions and keep playing.
    fn sanitize_regions(&mut self) -> Result<(), SoundFontError> {
        // https://github.com/sinshu/rustysynth/issues/22
        // https://github.com/sinshu/rustysynth/issues/33
        // https://github.com/sinshu/rustysynth/pull/51
        let wave_len = self.wave_data.len();
        for instrument in &mut self.instruments {
            instrument.regions.retain(|region| {
                let start = region.get_sample_start();
                let end = region.get_sample_end();
                let start_loop = region.get_sample_start_loop();
                let end_loop = region.get_sample_end_loop();
                let loop_mode = region.get_sample_modes();

                if start < 0 || end as usize >= wave_len || end <= start {
                    return false;
                }
                // The rate is stored unsigned and read as i32, so a value past
                // 2^31-1 arrives negative. Zero or negative walks the
                // oscillator backwards out of the sample. A huge positive rate
                // still plays; the oscillator wraps a step longer than the loop.
                if region.sample_sample_rate <= 0 {
                    return false;
                }
                // Loop points are only meaningful when looping is enabled.
                if loop_mode != LoopMode::NoLoop
                    && (start_loop < 0 || end_loop as usize >= wave_len || start_loop >= end_loop)
                {
                    return false;
                }
                true
            });
        }
        // Futureboard: an instrument left with no regions stays in the list.
        // Preset regions hold indices into `instruments`, so removing one
        // shifted every later index, pointing presets at the wrong instrument
        // or past the end of the list (a panic on the first note-on, in the
        // audio callback). An empty instrument simply plays nothing.
        if self
            .instruments
            .iter()
            .all(|instrument| instrument.regions.is_empty())
        {
            return Err(SoundFontError::SanityCheckFailed);
        }
        Ok(())
    }

    /// Gets the information of the SoundFont.
    pub fn get_info(&self) -> &SoundFontInfo {
        &self.info
    }

    /// Gets the bits per sample of the sample data.
    pub fn get_bits_per_sample(&self) -> i32 {
        self.bits_per_sample
    }

    /// Gets the sample data.
    pub fn get_wave_data(&self) -> &[i16] {
        &self.wave_data[..]
    }

    /// Gets the samples of the SoundFont.
    pub fn get_sample_headers(&self) -> &[SampleHeader] {
        &self.sample_headers[..]
    }

    /// Gets the presets of the SoundFont.
    pub fn get_presets(&self) -> &[Preset] {
        &self.presets[..]
    }

    /// Gets the instruments of the SoundFont.
    pub fn get_instruments(&self) -> &[Instrument] {
        &self.instruments[..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{fs::File, path::PathBuf};

    fn samples_dir_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("samples")
    }

    #[test]
    fn test_load_reject_sf3() {
        let path = samples_dir_path().join("dummy.sf3");
        let mut file = File::open(&path).unwrap();
        assert!(matches!(
            SoundFont::new(&mut file),
            Err(SoundFontError::UnsupportedSampleFormat)
        ));
    }

    // smpl sub-chunk exists, but is zero-length.
    #[test]
    fn test_load_empty_samples() {
        let path = samples_dir_path().join("test_empty_samples.sf2");
        let mut file = File::open(&path).unwrap();
        assert!(matches!(
            SoundFont::new(&mut file),
            Err(SoundFontError::SampleDataNotFound)
        ));
    }
}

/// Futureboard: malformed and truncated fonts must be rejected with an error,
/// never a panic (release builds abort on panic) or an allocation sized by an
/// untrusted chunk header.
#[cfg(test)]
mod malformed_tests {
    use super::*;

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::io::{Cursor, ErrorKind};
    use std::sync::Arc;

    use crate::{Synthesizer, SynthesizerSettings};

    thread_local! {
        /// Largest single allocation this thread has asked for.
        static LARGEST_ALLOCATION: Cell<usize> = const { Cell::new(0) };
    }

    fn note_allocation(size: usize) {
        let _ = LARGEST_ALLOCATION.try_with(|largest| largest.set(largest.get().max(size)));
    }

    struct TrackingAllocator;

    unsafe impl GlobalAlloc for TrackingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note_allocation(layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note_allocation(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note_allocation(new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: TrackingAllocator = TrackingAllocator;

    const GEN_INSTRUMENT: u16 = 41;
    const GEN_SAMPLE_ID: u16 = 53;
    const GEN_SAMPLE_MODES: u16 = 54;
    const SAMPLE_FRAMES: i32 = 256;
    /// SF2 requires 46 zero frames after each sample.
    const SAMPLE_PADDING: i32 = 46;

    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn list(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        chunk(b"LIST", &body)
    }

    fn info() -> Vec<u8> {
        list(b"INFO", &chunk(b"ifil", &[2, 0, 1, 0]))
    }

    fn sdta() -> Vec<u8> {
        let frames = (SAMPLE_FRAMES + SAMPLE_PADDING) as usize;
        let mut samples = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let value = if frame < SAMPLE_FRAMES as usize {
                (frame as i16 - 128) * 100
            } else {
                0
            };
            samples.extend_from_slice(&value.to_le_bytes());
        }
        list(b"sdta", &chunk(b"smpl", &samples))
    }

    fn riff(parts: &[&[u8]]) -> Vec<u8> {
        let mut body = b"sfbk".to_vec();
        for part in parts {
            body.extend_from_slice(part);
        }
        chunk(b"RIFF", &body)
    }

    /// The hydra tables of a font, each list's terminator record included
    /// except for the generator and sample-header lists, whose terminators
    /// [`Tables::pdta`] appends.
    struct Tables {
        /// First preset bag of each preset.
        phdr: Vec<u16>,
        /// First preset generator of each preset bag.
        pbag: Vec<u16>,
        pgen: Vec<(u16, u16)>,
        /// First instrument bag of each instrument.
        inst: Vec<u16>,
        /// First instrument generator of each instrument bag.
        ibag: Vec<u16>,
        igen: Vec<(u16, u16)>,
        /// Start and end frame of each sample.
        shdr: Vec<(i32, i32)>,
        /// Per-sample rate. Missing entries are 44_100.
        rates: Vec<i32>,
    }

    impl Tables {
        /// One preset playing one instrument playing one sample.
        fn valid() -> Self {
            Self {
                phdr: vec![0, 1],
                pbag: vec![0, 1],
                pgen: vec![(GEN_INSTRUMENT, 0)],
                inst: vec![0, 1],
                ibag: vec![0, 1],
                igen: vec![(GEN_SAMPLE_ID, 0)],
                shdr: vec![(0, SAMPLE_FRAMES)],
                rates: Vec::new(),
            }
        }

        fn pdta(&self) -> Vec<u8> {
            fn name() -> [u8; 20] {
                let mut name = [0u8; 20];
                name[..4].copy_from_slice(b"Test");
                name
            }
            fn bags(starts: &[u16]) -> Vec<u8> {
                starts
                    .iter()
                    .flat_map(|start| [start.to_le_bytes(), 0u16.to_le_bytes()].concat())
                    .collect()
            }
            fn generators(list: &[(u16, u16)]) -> Vec<u8> {
                list.iter()
                    .chain(std::iter::once(&(0, 0)))
                    .flat_map(|(kind, value)| [kind.to_le_bytes(), value.to_le_bytes()].concat())
                    .collect()
            }

            let mut phdr = Vec::new();
            for start in &self.phdr {
                phdr.extend_from_slice(&name());
                phdr.extend_from_slice(&0u16.to_le_bytes()); // patch
                phdr.extend_from_slice(&0u16.to_le_bytes()); // bank
                phdr.extend_from_slice(&start.to_le_bytes());
                phdr.extend_from_slice(&[0u8; 12]); // library, genre, morphology
            }
            let mut inst = Vec::new();
            for start in &self.inst {
                inst.extend_from_slice(&name());
                inst.extend_from_slice(&start.to_le_bytes());
            }
            let mut shdr = Vec::new();
            for (index, (start, end)) in
                self.shdr.iter().chain(std::iter::once(&(0, 0))).enumerate()
            {
                let rate = if index < self.shdr.len() {
                    self.rates.get(index).copied().unwrap_or(44_100)
                } else {
                    44_100
                };
                shdr.extend_from_slice(&name());
                shdr.extend_from_slice(&start.to_le_bytes());
                shdr.extend_from_slice(&end.to_le_bytes());
                shdr.extend_from_slice(&start.to_le_bytes()); // loop start
                shdr.extend_from_slice(&(end - 1).max(0).to_le_bytes()); // loop end
                shdr.extend_from_slice(&rate.to_le_bytes());
                shdr.extend_from_slice(&[60, 0]); // original pitch, correction
                shdr.extend_from_slice(&0u16.to_le_bytes()); // link
                shdr.extend_from_slice(&1u16.to_le_bytes()); // mono
            }

            let mut data = Vec::new();
            data.extend(chunk(b"phdr", &phdr));
            data.extend(chunk(b"pbag", &bags(&self.pbag)));
            data.extend(chunk(b"pmod", &[0u8; 10]));
            data.extend(chunk(b"pgen", &generators(&self.pgen)));
            data.extend(chunk(b"inst", &inst));
            data.extend(chunk(b"ibag", &bags(&self.ibag)));
            data.extend(chunk(b"imod", &[0u8; 10]));
            data.extend(chunk(b"igen", &generators(&self.igen)));
            data.extend(chunk(b"shdr", &shdr));
            list(b"pdta", &data)
        }

        fn load(&self) -> Result<SoundFont, SoundFontError> {
            let bytes = riff(&[&info(), &sdta(), &self.pdta()]);
            SoundFont::new(&mut Cursor::new(bytes))
        }
    }

    #[test]
    fn a_well_formed_synthetic_font_loads() {
        let font = Tables::valid().load().expect("valid font");
        assert_eq!(font.get_presets().len(), 1);
        assert_eq!(font.get_instruments().len(), 1);
        assert_eq!(font.get_instruments()[0].get_regions().len(), 1);
    }

    #[test]
    fn an_instrument_bag_past_the_generator_list_is_rejected() {
        let mut tables = Tables::valid();
        tables.ibag = vec![0, 40];
        assert!(matches!(
            tables.load(),
            Err(SoundFontError::InvalidZoneList)
        ));

        let mut tables = Tables::valid();
        tables.ibag = vec![9, 10];
        assert!(matches!(
            tables.load(),
            Err(SoundFontError::InvalidZoneList)
        ));
    }

    #[test]
    fn a_preset_bag_past_the_generator_list_is_rejected() {
        let mut tables = Tables::valid();
        tables.pbag = vec![0, 40];
        assert!(matches!(
            tables.load(),
            Err(SoundFontError::InvalidZoneList)
        ));
    }

    #[test]
    fn an_instrument_past_the_bag_list_is_rejected() {
        let mut tables = Tables::valid();
        tables.inst = vec![0, 9];
        assert!(matches!(
            tables.load(),
            Err(SoundFontError::InvalidInstrumentList)
        ));

        let mut tables = Tables::valid();
        tables.inst = vec![5, 6];
        assert!(matches!(
            tables.load(),
            Err(SoundFontError::InvalidInstrumentList)
        ));
    }

    #[test]
    fn a_preset_past_the_bag_list_is_rejected() {
        let mut tables = Tables::valid();
        tables.phdr = vec![0, 9];
        assert!(matches!(
            tables.load(),
            Err(SoundFontError::InvalidPresetList)
        ));
    }

    #[test]
    fn out_of_range_sample_and_instrument_ids_are_rejected() {
        let mut tables = Tables::valid();
        tables.igen = vec![(GEN_SAMPLE_ID, 7)];
        assert!(matches!(
            tables.load(),
            Err(SoundFontError::InvalidSampleId { .. })
        ));

        let mut tables = Tables::valid();
        tables.pgen = vec![(GEN_INSTRUMENT, 7)];
        assert!(matches!(
            tables.load(),
            Err(SoundFontError::InvalidInstrumentId { .. })
        ));
    }

    #[test]
    fn a_non_positive_sample_rate_is_not_played() {
        for rate in [0, -1, i32::MIN] {
            let mut tables = Tables::valid();
            tables.rates = vec![rate];
            assert!(
                matches!(tables.load(), Err(SoundFontError::SanityCheckFailed)),
                "rate {rate} was accepted"
            );
        }
    }

    #[test]
    fn a_huge_sample_rate_does_not_read_past_the_sample() {
        let mut tables = Tables::valid();
        tables.rates = vec![50_000_000];
        // Sample modes must precede sample id: a zone whose last generator is
        // not the sample id is treated as a global zone.
        tables.ibag = vec![0, 2];
        tables.igen = vec![(GEN_SAMPLE_MODES, 1), (GEN_SAMPLE_ID, 0)];
        let font = Arc::new(tables.load().expect("huge rate still loads"));
        let mut synth =
            Synthesizer::new(&font, &SynthesizerSettings::new(44_100)).expect("synthesizer");
        synth.note_on(0, 127, 127);
        let (mut left, mut right) = (vec![0.0f32; 4_096], vec![0.0f32; 4_096]);
        synth.render(&mut left, &mut right);
        assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
    }

    #[test]
    fn sanitizing_an_instrument_keeps_preset_instrument_indices_valid() {
        // Instrument 0's only sample runs past the sample data, so sanitizing
        // empties it; the preset plays instrument 1. Removing instrument 0
        // from the list used to leave the preset pointing past its end.
        let mut tables = Tables::valid();
        tables.shdr = vec![(0, 10_000), (0, SAMPLE_FRAMES)];
        tables.inst = vec![0, 1, 2];
        tables.ibag = vec![0, 1, 2];
        tables.igen = vec![(GEN_SAMPLE_ID, 0), (GEN_SAMPLE_ID, 1)];
        tables.pgen = vec![(GEN_INSTRUMENT, 1)];
        let font = tables.load().expect("one playable instrument remains");

        let instrument = font.get_presets()[0].get_regions()[0].get_instrument_id();
        assert!(font.get_instruments()[0].get_regions().is_empty());
        assert!(!font.get_instruments()[instrument].get_regions().is_empty());

        let font = Arc::new(font);
        let mut synth =
            Synthesizer::new(&font, &SynthesizerSettings::new(44_100)).expect("synthesizer");
        synth.note_on(0, 60, 100);
        let (mut left, mut right) = (vec![0f32; 256], vec![0f32; 256]);
        synth.render(&mut left, &mut right);
        assert!(left.iter().chain(&right).any(|sample| *sample != 0.0));
    }

    fn truncated_chunk_error(bytes: Vec<u8>) -> (SoundFontError, usize) {
        LARGEST_ALLOCATION.with(|largest| largest.set(0));
        let result = SoundFont::new(&mut Cursor::new(bytes));
        let largest = LARGEST_ALLOCATION.with(|largest| largest.get());
        (result.expect_err("truncated font must not load"), largest)
    }

    /// A chunk header claiming `declared` bytes, followed by far fewer.
    fn truncated_chunk(id: &[u8; 4], declared: u32) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&declared.to_le_bytes());
        out.extend_from_slice(&[0x11; 1000]);
        out
    }

    /// A LIST header whose size covers the rest of the (truncated) file.
    fn open_list(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = b"LIST".to_vec();
        out.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn truncated_chunks_fail_without_allocating_their_declared_size() {
        let declared = 0xFFFF_FFF0u32;
        let smpl_then_sm24 = [
            chunk(b"smpl", &[1u8; 200]),
            truncated_chunk(b"sm24", declared),
        ]
        .concat();
        let cases = [
            (
                "smpl",
                riff(&[
                    &info(),
                    &open_list(b"sdta", &truncated_chunk(b"smpl", declared)),
                ]),
            ),
            (
                "sm24",
                riff(&[&info(), &open_list(b"sdta", &smpl_then_sm24)]),
            ),
            (
                "INAM",
                riff(&[&open_list(b"INFO", &truncated_chunk(b"INAM", declared))]),
            ),
            (
                "pmod",
                riff(&[
                    &info(),
                    &sdta(),
                    &open_list(b"pdta", &truncated_chunk(b"pmod", declared)),
                ]),
            ),
        ];
        for (name, bytes) in cases {
            let (error, largest) = truncated_chunk_error(bytes);
            assert!(
                matches!(&error, SoundFontError::IoError(e) if e.kind() == ErrorKind::UnexpectedEof),
                "{name}: {error:?}"
            );
            assert!(
                largest < 1 << 20,
                "{name}: allocated {largest} bytes for a chunk that held 1000"
            );
        }
    }
}
