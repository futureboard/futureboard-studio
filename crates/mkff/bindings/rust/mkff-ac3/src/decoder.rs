//! The public decoder: framing of packets into syncframes, substream selection, channel
//! assembly and downmixing.

use mkff_audio_core::{AudioBuffer, Decoder, Error, Result};

use crate::frame::{Options, Substream};
use crate::header::{check_crc, parse_frame, FrameInfo, StreamType};
use crate::imdct::Imdct;
use crate::layout::{base_layout, output_order, Channel};

/// Downmix applied to the decoded programme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Downmix {
    /// Two-channel Lo/Ro downmix (clause 6.8.2) with the stream's centre and surround mix
    /// levels, normalised so that no output can exceed full scale. The LFE is dropped.
    Stereo,
    /// The Lo/Ro downmix summed to one channel (`(Lo + Ro) / 2`).
    Mono,
}

/// AC-3 / E-AC-3 decoder implementing [`mkff_audio_core::Decoder`].
///
/// A packet holds one or more complete syncframes; for E-AC-3 an independent substream frame
/// followed by its dependent substream frames (as an MP4 / Matroska sample does). Several
/// consecutive programmes in one packet are decoded and concatenated as long as their channel
/// layout stays the same.
///
/// Output:
///
/// - All channels of the programme in WAV order, no downmix: `L R C LFE Ls Rs` for 5.1
///   (the coded channels a mode has, in that order: 2/1 gives `L R S`, with S reported as
///   [`Channel::Cs`]); with the dependent substreams of a 7.1 E-AC-3 programme
///   `L R C LFE Lrs Rrs Ls Rs` (rear surrounds as the WAV back pair, surrounds as the side pair).
///   See [`crate::layout::output_order`] for the other extension channels. The 1+1 dual mono
///   mode decodes to two channels (Ch1, Ch2).
/// - Planar f32 in the A/52 scale (full scale = ±1.0), not clipped.
/// - Dynamic range control is off by default ([`set_drc`](Self::set_drc)); dialogue
///   normalization is never applied ([`FrameInfo::dialnorm`] reports it).
/// - Zero-bit mantissas are dithered when the stream asks for it, with a deterministic
///   xorshift generator (uniform, ±0.707); [`set_dither(false)`](Self::set_dither) uses zeros.
/// - Only programme 1 (independent substream 0 and its dependent substreams) is decoded; other
///   independent substreams are skipped.
pub struct Ac3Decoder {
    imdct: Imdct,
    /// 0: independent substream 0 (or the AC-3 stream); 1 + id: dependent substream `id`.
    subs: Vec<Substream>,
    downmix: Option<Downmix>,
    opt: Options,
    check_crc: bool,
    sample_rate: u32,
    layout: Vec<Channel>,
    last_info: Option<FrameInfo>,
    /// (substream slot, coded channel index) for each output channel, rebuilt per programme.
    sources: Vec<(usize, usize)>,
}

impl Default for Ac3Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Ac3Decoder {
    /// A decoder with no downmix, no dynamic range control, dither on and CRC checking on.
    pub fn new() -> Self {
        Self {
            imdct: Imdct::new(),
            subs: (0..9).map(|_| Substream::new()).collect(),
            downmix: None,
            opt: Options {
                drc_scale: 0.0,
                dither: true,
            },
            check_crc: true,
            sample_rate: 0,
            layout: Vec::new(),
            last_info: None,
            sources: Vec::new(),
        }
    }

    /// Downmix to stereo or mono (`None`, the default, outputs every channel). Dependent
    /// substreams are ignored while downmixing (their programme's 5.1 core is downmixed).
    pub fn set_downmix(&mut self, downmix: Option<Downmix>) {
        self.downmix = downmix;
    }

    /// Dynamic range control: the fraction (0.0..=1.0, larger values exaggerate) of the
    /// stream's `dynrng` gains to apply. 0 (the default) disables it.
    pub fn set_drc(&mut self, scale: f32) {
        self.opt.drc_scale = if scale.is_finite() {
            scale.max(0.0)
        } else {
            0.0
        };
    }

    /// Dither zero-bit mantissas (default on, as the bit stream's `dithflag` requests).
    pub fn set_dither(&mut self, dither: bool) {
        self.opt.dither = dither;
    }

    /// Verify the CRC words of each syncframe and reject damaged frames (default on).
    pub fn set_crc_check(&mut self, check: bool) {
        self.check_crc = check;
    }

    /// Channel locations of the output, in output order (empty until a frame was decoded).
    pub fn layout(&self) -> &[Channel] {
        &self.layout
    }

    /// Header of the last independent syncframe decoded.
    pub fn last_frame_info(&self) -> Option<&FrameInfo> {
        self.last_info.as_ref()
    }

    /// Decode the frames of a packet; see the type documentation.
    fn decode_packet(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        let mut out: Option<AudioBuffer> = None;
        let mut pos = 0;
        // dependent substream slots decoded for the current programme, in stream order
        let mut deps: [usize; 8] = [0; 8];
        let mut ndeps = 0;
        let mut have_program = false;
        let mut skipping = false;
        while pos < packet.len() {
            let rest = &packet[pos..];
            if rest.iter().all(|&b| b == 0) {
                break; // zero padding after the last frame
            }
            let info = parse_frame(rest).ok_or_else(|| {
                if rest.len() < 8 {
                    Error::EndOfData
                } else {
                    Error::invalid("no AC-3 / E-AC-3 syncframe")
                }
            })?;
            if info.frame_size > rest.len() {
                return Err(Error::EndOfData);
            }
            let frame = &rest[..info.frame_size];
            pos += info.frame_size;
            if self.check_crc && !check_crc(frame, &info) {
                return Err(Error::invalid("syncframe CRC mismatch"));
            }
            match info.stream_type {
                StreamType::Ac3 | StreamType::Independent | StreamType::Converted => {
                    if have_program {
                        self.emit(&mut out, &deps[..ndeps])?;
                        have_program = false;
                    }
                    ndeps = 0;
                    skipping = info.substream_id != 0;
                    if skipping {
                        continue;
                    }
                    self.subs[0].decode_frame(frame, &info, &mut self.imdct, &self.opt)?;
                    self.sample_rate = info.sample_rate;
                    self.last_info = Some(info);
                    have_program = true;
                }
                StreamType::Dependent => {
                    if skipping || !have_program || self.downmix.is_some() {
                        continue;
                    }
                    if info.blocks_per_frame != self.subs[0].blocks
                        || info.sample_rate != self.sample_rate
                    {
                        return Err(Error::invalid(
                            "dependent substream does not match its programme",
                        ));
                    }
                    let slot = 1 + usize::from(info.substream_id);
                    self.subs[slot].decode_frame(frame, &info, &mut self.imdct, &self.opt)?;
                    if ndeps < 8 && !deps[..ndeps].contains(&slot) {
                        deps[ndeps] = slot;
                        ndeps += 1;
                    }
                }
            }
        }
        if have_program {
            self.emit(&mut out, &deps[..ndeps])?;
        }
        Ok(out.unwrap_or_else(|| AudioBuffer::empty(self.sample_rate, self.channels())))
    }

    /// Append the programme just decoded (substream 0 plus `deps`) to `out`.
    fn emit(&mut self, out: &mut Option<AudioBuffer>, deps: &[usize]) -> Result<()> {
        let n = self.subs[0].blocks * 256;
        let buf = if let Some(dm) = self.downmix {
            self.layout = match dm {
                Downmix::Stereo => vec![Channel::L, Channel::R],
                Downmix::Mono => vec![Channel::C],
            };
            downmix(&self.subs[0], dm, n)
        } else {
            // locations of every coded channel, later substreams replacing earlier ones
            let s0 = &self.subs[0];
            let mut locs: Vec<(Channel, (usize, usize))> = base_layout(s0.acmod, s0.lfeon)
                .into_iter()
                .enumerate()
                .map(|(i, c)| (c, (0, i)))
                .collect();
            for &slot in deps {
                let Some(info) = self.subs[slot].info.as_ref() else {
                    continue;
                };
                let coded = info.coded_channels();
                if coded.len() != self.subs[slot].channels() {
                    return Err(Error::invalid("chanmap does not match the coded channels"));
                }
                for (i, c) in coded.into_iter().enumerate() {
                    match locs.iter_mut().find(|(l, _)| *l == c) {
                        Some(e) => e.1 = (slot, i),
                        None => locs.push((c, (slot, i))),
                    }
                }
            }
            let order = output_order(&locs.iter().map(|(c, _)| *c).collect::<Vec<_>>());
            self.sources.clear();
            for c in &order {
                if let Some((_, src)) = locs.iter().find(|(l, _)| l == c) {
                    self.sources.push(*src);
                }
            }
            self.layout = order;
            let mut buf = AudioBuffer::empty(self.sample_rate, 0);
            buf.channels = self
                .sources
                .iter()
                .map(|&(slot, i)| self.subs[slot].pcm[i][..n].to_vec())
                .collect();
            buf
        };
        match out {
            None => *out = Some(buf),
            Some(o) => {
                if o.channels.len() != buf.channels.len() {
                    return Err(Error::invalid("channel layout changed within a packet"));
                }
                o.append(&buf);
            }
        }
        Ok(())
    }
}

/// Lo/Ro (and mono) downmix of an independent substream (clause 6.8.2), normalised so that
/// the coefficients feeding each output sum to at most 1. The LFE is not mixed in.
fn downmix(s: &Substream, mode: Downmix, n: usize) -> AudioBuffer {
    const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let pcm = &s.pcm;
    let mut lo = vec![0f32; n];
    let mut ro = vec![0f32; n];
    let mix = |dst: &mut [f32], src: &[f32], g: f32| {
        for (d, v) in dst.iter_mut().zip(&src[..n]) {
            *d += g * v;
        }
    };
    match s.acmod {
        0 => {
            // 1+1: Ch1 left, Ch2 right
            mix(&mut lo, &pcm[0], 1.0);
            mix(&mut ro, &pcm[1], 1.0);
        }
        1 => {
            mix(&mut lo, &pcm[0], M3DB);
            mix(&mut ro, &pcm[0], M3DB);
        }
        acmod => {
            let (clev, slev) = (s.clev, s.slev);
            let has_c = acmod & 1 != 0;
            // coded order: L [C] R [S | Ls Rs]
            let (r, first_s) = if has_c { (2, 3) } else { (1, 2) };
            let surrounds = match acmod {
                4 | 5 => 1,
                6 | 7 => 2,
                _ => 0,
            };
            let sgain = if surrounds == 1 { M3DB * slev } else { slev };
            let norm = 1.0
                / (1.0 + if has_c { clev } else { 0.0 } + if surrounds > 0 { sgain } else { 0.0 });
            mix(&mut lo, &pcm[0], norm);
            mix(&mut ro, &pcm[r], norm);
            if has_c {
                mix(&mut lo, &pcm[1], clev * norm);
                mix(&mut ro, &pcm[1], clev * norm);
            }
            match surrounds {
                1 => {
                    mix(&mut lo, &pcm[first_s], sgain * norm);
                    mix(&mut ro, &pcm[first_s], sgain * norm);
                }
                2 => {
                    mix(&mut lo, &pcm[first_s], sgain * norm);
                    mix(&mut ro, &pcm[first_s + 1], sgain * norm);
                }
                _ => {}
            }
        }
    }
    let sample_rate = s.info.as_ref().map_or(0, |i| i.sample_rate);
    let mut buf = AudioBuffer::empty(sample_rate, 0);
    match mode {
        Downmix::Stereo => buf.channels = vec![lo, ro],
        Downmix::Mono => {
            for (a, b) in lo.iter_mut().zip(&ro) {
                *a = 0.5 * (*a + b);
            }
            buf.channels = vec![lo];
        }
    }
    buf
}

impl Decoder for Ac3Decoder {
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        self.decode_packet(packet)
    }

    fn reset(&mut self) {
        for s in &mut self.subs {
            s.reset();
        }
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> usize {
        match self.downmix {
            Some(Downmix::Stereo) => 2,
            Some(Downmix::Mono) => 1,
            None => self.layout.len(),
        }
    }
}
