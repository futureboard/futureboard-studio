//! The Vorbis packet decoder (spec §4.3).

use crate::bitread::BitReader;
use crate::floor::{inverse_db_table, FloorData};
use crate::header::{
    is_header, split_config, Comments, IdentHeader, PACKET_COMMENT, PACKET_IDENT, PACKET_SETUP,
};
use crate::imdct::Imdct;
use crate::residue;
use crate::setup::{Floor, Setup};
use mkff_audio_core::{AudioBuffer, Decoder, Error, Result};

/// The Vorbis power-complementary window slope of length `m` (rising half; spec §4.3.1).
fn window_slope(m: usize) -> Vec<f32> {
    let h = std::f64::consts::FRAC_PI_2;
    (0..m)
        .map(|i| {
            let s = ((i as f64 + 0.5) / m as f64 * h).sin();
            (h * s * s).sin() as f32
        })
        .collect()
}

/// For each output (WAV / SMPTE order) channel, the Vorbis channel it comes from (spec §4.3.9).
fn wav_channel_order(channels: usize) -> Vec<usize> {
    match channels {
        3 => vec![0, 2, 1],
        5 => vec![0, 2, 1, 3, 4],
        6 => vec![0, 2, 1, 5, 3, 4],
        7 => vec![0, 2, 1, 6, 5, 3, 4],
        8 => vec![0, 2, 1, 7, 5, 6, 3, 4],
        n => (0..n).collect(),
    }
}

/// Everything derived from the setup header, plus the decode state and scratch buffers.
struct State {
    setup: Setup,
    blocksize: [usize; 2],
    mdct: [Imdct; 2],
    /// Window slopes of length `blocksize_0 / 2` and `blocksize_1 / 2`.
    slopes: [Vec<f32>; 2],
    inv_db: [f32; 256],
    /// Per-channel spectrum (`blocksize_1 / 2`).
    spectra: Vec<Vec<f32>>,
    /// Per-channel windowed right half of the previous block.
    overlap: Vec<Vec<f32>>,
    /// Block size of the previous packet; `None` before the first audio packet.
    prev_n: Option<usize>,
    time: Vec<f32>,
    floor_data: Vec<FloorData>,
    floor_used: Vec<bool>,
    no_residue: Vec<bool>,
    classes: Vec<u8>,
    interleaved: Vec<f32>,
    /// Vorbis channel → output channel.
    out_index: Vec<usize>,
}

impl State {
    fn new(ident: &IdentHeader, setup: Setup) -> Self {
        let ch = usize::from(ident.channels);
        let bs = [ident.blocksize_0, ident.blocksize_1];
        let order = wav_channel_order(ch);
        let mut out_index = vec![0; ch];
        for (wav, &v) in order.iter().enumerate() {
            out_index[v] = wav;
        }
        Self {
            setup,
            blocksize: bs,
            mdct: [Imdct::new(bs[0] / 2), Imdct::new(bs[1] / 2)],
            slopes: [window_slope(bs[0] / 2), window_slope(bs[1] / 2)],
            inv_db: inverse_db_table(),
            spectra: vec![vec![0.0; bs[1] / 2]; ch],
            overlap: vec![vec![0.0; bs[1] / 2]; ch],
            prev_n: None,
            time: vec![0.0; bs[1]],
            floor_data: vec![FloorData::default(); ch],
            floor_used: vec![false; ch],
            no_residue: vec![false; ch],
            classes: Vec::new(),
            interleaved: Vec::new(),
            out_index,
        }
    }

    /// Block size of an audio packet, `None` for header packets or an unknown mode.
    fn packet_block_size(&self, packet: &[u8]) -> Option<usize> {
        let mut r = BitReader::new(packet);
        if r.read_bit().ok()? {
            return None;
        }
        let mode = self
            .setup
            .modes
            .get(r.read(self.setup.mode_bits).ok()? as usize)?;
        Some(self.blocksize[usize::from(mode.blockflag)])
    }

    fn decode_audio(&mut self, packet: &[u8], sample_rate: u32) -> Result<AudioBuffer> {
        let channels = self.spectra.len();
        let mut r = BitReader::new(packet);
        if r.read_bit()? {
            return Err(Error::invalid("vorbis: not an audio packet"));
        }
        let mode_number = r.read(self.setup.mode_bits)? as usize;
        let mode = *self
            .setup
            .modes
            .get(mode_number)
            .ok_or_else(|| Error::invalid("vorbis: mode number out of range"))?;
        let long = mode.blockflag;
        let n = self.blocksize[usize::from(long)];
        let half = n / 2;
        let (prev_long, next_long) = if long {
            (r.read_bit()?, r.read_bit()?)
        } else {
            (false, false)
        };

        let State {
            setup,
            blocksize,
            mdct,
            slopes,
            inv_db,
            spectra,
            overlap,
            prev_n,
            time,
            floor_data,
            floor_used,
            no_residue,
            classes,
            interleaved,
            out_index,
        } = self;
        let setup = &*setup;
        let mapping = &setup.mappings[usize::from(mode.mapping)];
        let books = &setup.codebooks;

        // floors
        for ch in 0..channels {
            let submap = usize::from(mapping.mux[ch]);
            let floor = &setup.floors[usize::from(mapping.submaps[submap].0)];
            let used = match floor {
                Floor::Zero(f) => f.decode(&mut r, books, &mut floor_data[ch]),
                Floor::One(f) => f.decode(&mut r, books, &mut floor_data[ch]),
            };
            floor_used[ch] = used;
            no_residue[ch] = !used;
        }
        // nonzero vector propagation
        for &(m, a) in &mapping.coupling {
            let (m, a) = (usize::from(m), usize::from(a));
            if !no_residue[m] || !no_residue[a] {
                no_residue[m] = false;
                no_residue[a] = false;
            }
        }

        // residues
        for s in spectra.iter_mut() {
            s[..half].fill(0.0);
        }
        for (submap, &(_, res_idx)) in mapping.submaps.iter().enumerate() {
            let res = &setup.residues[usize::from(res_idx)];
            let mut skip = Vec::with_capacity(channels);
            let mut vecs: Vec<&mut [f32]> = Vec::with_capacity(channels);
            for (ch, s) in spectra.iter_mut().enumerate() {
                if usize::from(mapping.mux[ch]) == submap {
                    skip.push(no_residue[ch]);
                    vecs.push(&mut s[..half]);
                }
            }
            if vecs.is_empty() {
                continue;
            }
            if res.kind == 2 {
                if skip.iter().all(|&s| s) {
                    continue;
                }
                let nch = vecs.len();
                interleaved.clear();
                interleaved.resize(nch * half, 0.0);
                residue::decode(
                    res,
                    books,
                    &mut r,
                    &mut [&mut interleaved[..]],
                    &[false],
                    classes,
                );
                for (k, v) in vecs.iter_mut().enumerate() {
                    for (d, frame) in v.iter_mut().zip(interleaved.chunks_exact(nch)) {
                        *d = frame[k];
                    }
                }
            } else {
                residue::decode(res, books, &mut r, &mut vecs, &skip, classes);
            }
        }

        // inverse coupling
        for &(m, a) in mapping.coupling.iter().rev() {
            let (m, a) = (usize::from(m), usize::from(a));
            let (mag, ang) = if m < a {
                let (lo, hi) = spectra.split_at_mut(a);
                (&mut lo[m], &mut hi[0])
            } else {
                let (lo, hi) = spectra.split_at_mut(m);
                (&mut hi[0], &mut lo[a])
            };
            for (mv, av) in mag[..half].iter_mut().zip(&mut ang[..half]) {
                let (mm, aa) = (*mv, *av);
                if mm > 0.0 {
                    if aa > 0.0 {
                        *av = mm - aa;
                    } else {
                        *av = mm;
                        *mv = mm + aa;
                    }
                } else if aa > 0.0 {
                    *av = mm + aa;
                } else {
                    *av = mm;
                    *mv = mm - aa;
                }
            }
        }

        // floor curves × residue, IMDCT, window, overlap-add
        let prev = *prev_n;
        let out_len = prev.map_or(0, |p| p / 4 + n / 4);
        let mut out = AudioBuffer {
            sample_rate,
            channels: vec![Vec::new(); channels],
        };
        let bs0 = blocksize[0];
        let (lws, lwe, lslope) = if long && !prev_long {
            (n / 4 - bs0 / 4, n / 4 + bs0 / 4, &slopes[0])
        } else {
            (0, half, &slopes[usize::from(long)])
        };
        let (rws, rwe, rslope) = if long && !next_long {
            (3 * n / 4 - bs0 / 4, 3 * n / 4 + bs0 / 4, &slopes[0])
        } else {
            (half, n, &slopes[usize::from(long)])
        };
        let time = &mut time[..n];
        for ch in 0..channels {
            let spec = &mut spectra[ch][..half];
            if floor_used[ch] {
                match &setup.floors[usize::from(mapping.submaps[usize::from(mapping.mux[ch])].0)] {
                    Floor::Zero(f) => f.apply(&floor_data[ch], spec, long),
                    Floor::One(f) => f.apply(&floor_data[ch], spec, inv_db),
                }
                mdct[usize::from(long)].inverse(spec, time);
                time[..lws].fill(0.0);
                for (t, w) in time[lws..lwe].iter_mut().zip(lslope.iter()) {
                    *t *= w;
                }
                for (t, w) in time[rws..rwe].iter_mut().zip(rslope.iter().rev()) {
                    *t *= w;
                }
                time[rwe..].fill(0.0);
            } else {
                time.fill(0.0);
            }

            let ov = &mut overlap[ch];
            if let Some(pn) = prev {
                let mut o = vec![0.0f32; out_len];
                if pn == n {
                    for ((d, a), b) in o.iter_mut().zip(&ov[..half]).zip(&time[..half]) {
                        *d = a + b;
                    }
                } else if pn > n {
                    let off = pn / 4 - n / 4;
                    o[..off].copy_from_slice(&ov[..off]);
                    for ((d, a), b) in o[off..]
                        .iter_mut()
                        .zip(&ov[off..off + half])
                        .zip(&time[..half])
                    {
                        *d = a + b;
                    }
                } else {
                    let s = n / 4 - pn / 4;
                    let ph = pn / 2;
                    for ((d, a), b) in o[..ph].iter_mut().zip(&ov[..ph]).zip(&time[s..s + ph]) {
                        *d = a + b;
                    }
                    o[ph..].copy_from_slice(&time[s + ph..half]);
                }
                out.channels[out_index[ch]] = o;
            }
            ov[..half].copy_from_slice(&time[half..]);
        }
        *prev_n = Some(n);
        Ok(out)
    }
}

/// A Vorbis I decoder.
///
/// Packets are raw Vorbis packets (Ogg packets, Matroska / WebM blocks, MP4 samples). Each audio
/// packet returns the samples it completes: the first audio packet after construction or
/// [`reset`](Decoder::reset) returns an empty buffer, every later one returns
/// `previous_blocksize / 4 + current_blocksize / 4` samples per channel. Containers trim the
/// stream end (Ogg granule position, Matroska / MP4 durations) themselves.
///
/// Output channels are in WAV / SMPTE order (Vorbis' own order for 3–8 channels is converted,
/// spec §4.3.9); more than 8 channels are returned in stream order.
pub struct VorbisDecoder {
    ident: Option<IdentHeader>,
    comments: Option<Comments>,
    state: Option<Box<State>>,
}

impl VorbisDecoder {
    /// Create a decoder from a codec configuration.
    ///
    /// `config` holds the three header packets in one of the layouts [`split_config`] accepts
    /// (Matroska / WebM `CodecPrivate` Xiph lacing, 16-bit length prefixes, or plain
    /// concatenation), or is empty: then the first three packets given to
    /// [`decode`](Decoder::decode) must be the identification, comment and setup headers (the Ogg
    /// order) and each returns an empty buffer.
    ///
    /// A comment header that fails to parse is ignored.
    pub fn new(config: &[u8]) -> Result<Self> {
        match split_config(config)? {
            None => Ok(Self {
                ident: None,
                comments: None,
                state: None,
            }),
            Some([ident, comment, setup]) => {
                let mut d = Self::from_headers(ident, setup)?;
                d.comments = Comments::parse(comment).ok();
                Ok(d)
            }
        }
    }

    /// Create a decoder from the identification and setup header packets.
    pub fn from_headers(ident: &[u8], setup: &[u8]) -> Result<Self> {
        let ident = IdentHeader::parse(ident)?;
        let setup = Setup::parse(setup, &ident)?;
        let state = Box::new(State::new(&ident, setup));
        Ok(Self {
            ident: Some(ident),
            comments: None,
            state: Some(state),
        })
    }

    /// The identification header, once known.
    pub fn ident(&self) -> Option<&IdentHeader> {
        self.ident.as_ref()
    }

    /// The comment header, if one was given and parsed.
    pub fn comments(&self) -> Option<&Comments> {
        self.comments.as_ref()
    }

    /// Whether all headers needed for audio decode have been seen.
    pub fn is_ready(&self) -> bool {
        self.state.is_some()
    }

    /// The block size (`blocksize_0` or `blocksize_1`) of an audio packet, from its mode number.
    /// `None` for header packets, an unknown mode, or before the setup header is known.
    pub fn packet_block_size(&self, packet: &[u8]) -> Option<usize> {
        self.state.as_ref()?.packet_block_size(packet)
    }

    /// Samples per channel that audio packet `cur` completes when it follows audio packet `prev`
    /// (`prev_blocksize / 4 + cur_blocksize / 4`), as used for Ogg granule positions. `None` if
    /// either is not an audio packet of this stream.
    pub fn packet_samples(&self, prev: &[u8], cur: &[u8]) -> Option<usize> {
        Some(self.packet_block_size(prev)? / 4 + self.packet_block_size(cur)? / 4)
    }

    fn empty(&self) -> AudioBuffer {
        AudioBuffer::empty(self.sample_rate(), self.channels())
    }
}

impl Decoder for VorbisDecoder {
    /// Decode one packet. Header packets (also those arriving after the decoder is configured)
    /// and empty packets return an empty buffer.
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        if packet.is_empty() {
            return Ok(self.empty());
        }
        if self.state.is_none() {
            if is_header(packet, PACKET_IDENT) {
                self.ident = Some(IdentHeader::parse(packet)?);
            } else if is_header(packet, PACKET_COMMENT) {
                self.comments = Comments::parse(packet).ok();
            } else if is_header(packet, PACKET_SETUP) {
                let ident = self.ident.as_ref().ok_or_else(|| {
                    Error::invalid("vorbis: setup header before identification header")
                })?;
                let setup = Setup::parse(packet, ident)?;
                self.state = Some(Box::new(State::new(ident, setup)));
            } else {
                return Err(Error::invalid(
                    "vorbis: audio packet before the setup header",
                ));
            }
            return Ok(self.empty());
        }
        if packet[0] & 1 == 1 {
            return Ok(self.empty());
        }
        let rate = self.sample_rate();
        match self.state.as_mut() {
            Some(state) => state.decode_audio(packet, rate),
            None => Err(Error::invalid("vorbis: decoder not configured")),
        }
    }

    fn reset(&mut self) {
        if let Some(s) = self.state.as_mut() {
            s.prev_n = None;
        }
    }

    fn sample_rate(&self) -> u32 {
        self.ident.as_ref().map_or(0, |i| i.sample_rate)
    }

    fn channels(&self) -> usize {
        self.ident.as_ref().map_or(0, |i| usize::from(i.channels))
    }
}
