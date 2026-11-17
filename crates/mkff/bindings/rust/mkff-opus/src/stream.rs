//! One Opus stream (mono or stereo): SILK, CELT and hybrid frames, mode transitions and
//! redundancy (RFC 6716 §4.5), following the reference `opus_decoder.c` (see `NOTICE`).

use mkff_audio_core::Result;

use crate::celt::CeltDecoder;
use crate::packet::{self, Bandwidth, Mode};
use crate::range::RangeDecoder;
use crate::silk::{SilkControl, SilkDecoder};

const F20: usize = 960;
const F5: usize = 240;
const F2_5: usize = 120;
/// Samples per channel of the largest packet (120 ms at 48 kHz).
pub const MAX_PACKET_SAMPLES: usize = 5760;

/// `smooth_fade` with the output in place of the first input: `io = w² in2 + (1 - w²) io`.
fn fade_into_first(io: &mut [f32], in2: &[f32], overlap: usize, channels: usize) {
    for (i, w) in crate::celt::window().iter().enumerate().take(overlap) {
        let wi = w * w;
        for c in 0..channels {
            let k = i * channels + c;
            io[k] = wi * in2[k] + (1.0 - wi) * io[k];
        }
    }
}

/// `smooth_fade` with the output in place of the second input: `io = w² io + (1 - w²) in1`.
fn fade_into_second(in1: &[f32], io: &mut [f32], overlap: usize, channels: usize) {
    for (i, w) in crate::celt::window().iter().enumerate().take(overlap) {
        let wi = w * w;
        for c in 0..channels {
            let k = i * channels + c;
            io[k] = wi * io[k] + (1.0 - wi) * in1[k];
        }
    }
}

/// The decoder of one elementary Opus stream (`OpusDecoder`), output at 48 kHz.
pub struct StreamDecoder {
    channels: usize,
    celt: CeltDecoder,
    silk: SilkDecoder,
    stream_channels: usize,
    bandwidth: Bandwidth,
    mode: Mode,
    prev_mode: Option<Mode>,
    frame_size: usize,
    prev_redundancy: bool,
    range_final: u32,
    pcm_silk: Vec<i16>,
    pcm_transition: Vec<f32>,
    redundant_audio: Vec<f32>,
}

impl StreamDecoder {
    /// A decoder with `channels` (1 or 2) output channels.
    pub fn new(channels: usize) -> Self {
        let channels = channels.clamp(1, 2);
        StreamDecoder {
            channels,
            celt: CeltDecoder::new(channels),
            silk: SilkDecoder::new(),
            stream_channels: channels,
            bandwidth: Bandwidth::Fullband,
            mode: Mode::CeltOnly,
            prev_mode: None,
            frame_size: F2_5,
            prev_redundancy: false,
            range_final: 0,
            pcm_silk: vec![0; MAX_PACKET_SAMPLES * 2],
            pcm_transition: vec![0.0; F5 * 2],
            redundant_audio: vec![0.0; F5 * 2],
        }
    }

    /// Output channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Forget all state (`OPUS_RESET_STATE`).
    pub fn reset(&mut self) {
        self.celt.reset();
        self.silk.reset();
        self.stream_channels = self.channels;
        self.bandwidth = Bandwidth::Fullband;
        self.mode = Mode::CeltOnly;
        self.prev_mode = None;
        self.frame_size = F2_5;
        self.prev_redundancy = false;
        self.range_final = 0;
    }

    /// The final range coder state (XOR of the main and redundant frames) of the last frame,
    /// as `OPUS_GET_FINAL_RANGE` reports it.
    pub fn final_range(&self) -> u32 {
        self.range_final
    }

    fn decode_frame(
        &mut self,
        data: Option<&[u8]>,
        pcm: &mut [f32],
        frame_size: usize,
    ) -> Result<usize> {
        let ch = self.channels;
        let mut frame_size = frame_size;
        let data = match data {
            Some(d) if d.len() > 1 => Some(d),
            _ => {
                frame_size = frame_size.min(self.frame_size);
                None
            }
        };
        let mut len = data.map_or(0, <[u8]>::len);
        let audiosize;
        let mode;
        match data {
            Some(_) => {
                audiosize = self.frame_size;
                mode = self.mode;
            }
            None => {
                audiosize = frame_size;
                match self.prev_mode {
                    None => {
                        pcm[..audiosize * ch].fill(0.0);
                        return Ok(audiosize);
                    }
                    Some(m) => mode = m,
                }
            }
        }
        // CELT / hybrid concealment of more than 20 ms: several calls.
        if data.is_none() && frame_size > F20 && mode != Mode::SilkOnly {
            let mut done = 0;
            while done < frame_size {
                let n = F20.min(frame_size - done);
                let n = if n < F20 { n.max(F2_5) } else { n };
                self.decode_frame(None, &mut pcm[done * ch..], n)?;
                done += n;
            }
            return Ok(frame_size);
        }
        let mut transition = false;
        if data.is_some() {
            if let Some(prev) = self.prev_mode {
                if (mode == Mode::CeltOnly && prev != Mode::CeltOnly && !self.prev_redundancy)
                    || (mode != Mode::CeltOnly && prev == Mode::CeltOnly)
                {
                    transition = true;
                    if mode == Mode::CeltOnly {
                        let mut tr = std::mem::take(&mut self.pcm_transition);
                        self.decode_frame(None, &mut tr, F5.min(audiosize))?;
                        self.pcm_transition = tr;
                    }
                }
            }
        }
        if audiosize > frame_size {
            return Err(mkff_audio_core::Error::invalid(
                "opus: frame larger than the output",
            ));
        }
        let frame_size = audiosize;
        let mut ec = RangeDecoder::new(data.unwrap_or(&[]));

        // SILK.
        if mode != Mode::CeltOnly {
            if self.prev_mode == Some(Mode::CeltOnly) {
                self.silk.reset();
            }
            let internal_rate = if mode == Mode::SilkOnly {
                match self.bandwidth {
                    Bandwidth::Narrowband => 8000,
                    Bandwidth::Mediumband => 12000,
                    _ => 16000,
                }
            } else {
                16000
            };
            let ctl = SilkControl {
                channels_api: ch,
                channels_internal: self.stream_channels,
                internal_rate,
                payload_ms: (1000 * audiosize / 48000).max(10) as i32,
            };
            let mut decoded = 0usize;
            while decoded < frame_size {
                let first = decoded == 0;
                let out = &mut self.pcm_silk[decoded * ch..];
                let n = if data.is_some() {
                    self.silk.decode(&ctl, Some(&mut ec), first, out)
                } else {
                    self.silk.decode(&ctl, None, first, out)
                };
                if n == 0 {
                    self.pcm_silk[decoded * ch..frame_size * ch].fill(0);
                    break;
                }
                decoded += n;
            }
        }

        let mut redundancy = false;
        let mut celt_to_silk = false;
        let mut redundancy_bytes = 0usize;
        if mode != Mode::CeltOnly
            && data.is_some()
            && ec.tell() + 17 + 20 * i32::from(mode == Mode::Hybrid) <= 8 * len as i32
        {
            redundancy = if mode == Mode::Hybrid {
                ec.bit_logp(12)
            } else {
                true
            };
            if redundancy {
                celt_to_silk = ec.bit_logp(1);
                redundancy_bytes = if mode == Mode::Hybrid {
                    ec.uint(256) as usize + 2
                } else {
                    len.saturating_sub(((ec.tell() + 7) >> 3) as usize)
                };
                if redundancy_bytes > len || (len - redundancy_bytes) * 8 < ec.tell() as usize {
                    len = 0;
                    redundancy_bytes = 0;
                    redundancy = false;
                } else {
                    len -= redundancy_bytes;
                }
                ec.shrink_storage(redundancy_bytes);
            }
        }
        let start_band = if mode != Mode::CeltOnly { 17 } else { 0 };
        let end_band = match self.bandwidth {
            Bandwidth::Narrowband => 13,
            Bandwidth::Mediumband | Bandwidth::Wideband => 17,
            Bandwidth::SuperWideband => 19,
            Bandwidth::Fullband => 21,
        };
        self.celt.set_end_band(end_band);
        self.celt.set_stream_channels(self.stream_channels);
        if redundancy {
            transition = false;
        }
        if transition && mode != Mode::CeltOnly {
            let mut tr = std::mem::take(&mut self.pcm_transition);
            self.decode_frame(None, &mut tr, F5.min(audiosize))?;
            self.pcm_transition = tr;
        }
        let mut redundant_rng = 0u32;
        let red_data: &[u8] = match data {
            Some(d) if redundancy => &d[len..len + redundancy_bytes],
            _ => &[],
        };
        if redundancy && celt_to_silk {
            self.celt.set_start_band(0);
            let mut rec = RangeDecoder::new(red_data);
            self.celt
                .decode(&mut rec, red_data.len(), &mut self.redundant_audio, F5)?;
            redundant_rng = self.celt.final_range();
        }
        self.celt.set_start_band(start_band);

        if mode != Mode::SilkOnly {
            let celt_frame_size = F20.min(frame_size);
            if Some(mode) != self.prev_mode && self.prev_mode.is_some() && !self.prev_redundancy {
                self.celt.reset();
            }
            if data.is_some() {
                self.celt.decode(&mut ec, len, pcm, celt_frame_size)?;
            } else {
                self.celt.decode_lost(pcm, celt_frame_size);
            }
        } else {
            pcm[..frame_size * ch].fill(0.0);
            if self.prev_mode == Some(Mode::Hybrid)
                && !(redundancy && celt_to_silk && self.prev_redundancy)
            {
                self.celt.set_start_band(0);
                let silence = [0xffu8, 0xff];
                let mut sec = RangeDecoder::new(&silence);
                self.celt.decode(&mut sec, 2, pcm, F2_5)?;
                // Only the first 2.5 ms carry the fade-out; the rest stays silent.
                pcm[F2_5 * ch..frame_size * ch].fill(0.0);
            }
        }
        if mode != Mode::CeltOnly {
            for (p, s) in pcm[..frame_size * ch].iter_mut().zip(&self.pcm_silk) {
                *p += f32::from(*s) * (1.0 / 32768.0);
            }
        }
        if redundancy && !celt_to_silk {
            self.celt.reset();
            self.celt.set_start_band(0);
            let mut rec = RangeDecoder::new(red_data);
            self.celt
                .decode(&mut rec, red_data.len(), &mut self.redundant_audio, F5)?;
            redundant_rng = self.celt.final_range();
            let off = ch * (frame_size - F2_5);
            fade_into_first(
                &mut pcm[off..],
                &self.redundant_audio[ch * F2_5..],
                F2_5,
                ch,
            );
        }
        if redundancy && celt_to_silk {
            pcm[..F2_5 * ch].copy_from_slice(&self.redundant_audio[..F2_5 * ch]);
            fade_into_second(
                &self.redundant_audio[ch * F2_5..],
                &mut pcm[ch * F2_5..],
                F2_5,
                ch,
            );
        }
        if transition {
            if audiosize >= F5 {
                pcm[..ch * F2_5].copy_from_slice(&self.pcm_transition[..ch * F2_5]);
                fade_into_second(
                    &self.pcm_transition[ch * F2_5..],
                    &mut pcm[ch * F2_5..],
                    F2_5,
                    ch,
                );
            } else {
                fade_into_second(&self.pcm_transition, pcm, F2_5, ch);
            }
        }
        self.range_final = if len <= 1 {
            0
        } else {
            ec.range() ^ redundant_rng
        };
        self.prev_mode = Some(mode);
        self.prev_redundancy = redundancy && !celt_to_silk;
        Ok(audiosize)
    }

    /// Decode one packet into `pcm` (interleaved, [`channels`](Self::channels) channels).
    ///
    /// `self_delimited` selects the framing of RFC 6716 Appendix B (all but the last stream of
    /// a multistream packet). An empty `data` conceals a lost packet of `plc_samples` samples.
    /// Returns (samples per channel, bytes consumed).
    pub fn decode(
        &mut self,
        data: &[u8],
        self_delimited: bool,
        pcm: &mut [f32],
        plc_samples: usize,
    ) -> Result<(usize, usize)> {
        if data.is_empty() {
            let n = plc_samples.min(MAX_PACKET_SAMPLES);
            let mut done = 0;
            while done < n {
                let want = (n - done).min(F20);
                let got = self.decode_frame(None, &mut pcm[done * self.channels..], want)?;
                if got == 0 {
                    break;
                }
                done += got;
            }
            pcm[done * self.channels..n * self.channels].fill(0.0);
            return Ok((n, 0));
        }
        let frames = if self_delimited {
            packet::parse_self_delimited(data)?
        } else {
            packet::parse_packet(data)?
        };
        let toc = frames.toc;
        self.mode = toc.mode;
        self.bandwidth = toc.bandwidth;
        self.frame_size = toc.frame_samples;
        self.stream_channels = if toc.stereo { 2 } else { 1 };
        let total = frames.frames.len() * toc.frame_samples;
        if total > MAX_PACKET_SAMPLES || pcm.len() < total * self.channels {
            return Err(mkff_audio_core::Error::invalid("opus: packet too long"));
        }
        let mut done = 0;
        for f in &frames.frames {
            let n = self.decode_frame(Some(f), &mut pcm[done * self.channels..], total - done)?;
            done += n;
        }
        Ok((done, frames.consumed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plc_before_any_packet_is_silence() {
        let mut d = StreamDecoder::new(2);
        let mut pcm = vec![1.0f32; 960 * 2];
        let (n, _) = d.decode(&[], false, &mut pcm, 960).unwrap();
        assert_eq!(n, 960);
        assert!(pcm.iter().all(|&v| v == 0.0));
    }
}
