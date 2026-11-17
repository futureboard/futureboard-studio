//! The packet decoder: frame splitting, layer dispatch and output assembly.

use crate::header::{find_sync, next_free_format_header, parse_header, FrameHeader};
use crate::layer12::{decode_layer1, decode_layer2};
use crate::layer3::Layer3;
use crate::synth::Synth;
use crate::xing::is_info_frame;
use mkff_audio_core::{AudioBuffer, Decoder, Error, Result};

/// MPEG-1/2/2.5 layer I/II/III decoder.
///
/// A packet is one or more MPEG audio frames, headers included. A partial frame at the end of a
/// packet is kept and completed by the next packet, and bytes that are not frames (ID3 tags,
/// junk after a seek) are skipped. The Layer III bit reservoir is carried across packets, so
/// packets must be fed in stream order; call [`Decoder::reset`] after seeking.
///
/// The output is planar `f32` at the stream's sampling rate, one buffer per packet. Frames
/// carrying a Xing / Info / VBRI tag hold no audio and decode to nothing. Frames whose
/// reservoir data precedes the first packet after a reset decode to silence (keeping the
/// timing), as do frames whose data is damaged.
pub struct Mp3Decoder {
    pending: Vec<u8>,
    layer3: Box<Layer3>,
    synth: Box<[Synth; 2]>,
    pcm: [Vec<f32>; 2],
    sample_rate: u32,
    channels: usize,
    crc_check: bool,
    /// Free-format frame length without padding, once measured.
    free_len: Option<usize>,
    /// Header of the last decoded frame (the stream we are synchronised to).
    last: Option<FrameHeader>,
    frames: u64,
    skipped_frames: u64,
}

impl Default for Mp3Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Frames beyond this many buffered bytes are dropped instead of waiting for completion.
const MAX_PENDING: usize = 1 << 16;

impl Mp3Decoder {
    /// A decoder that accepts any version and layer.
    pub fn new() -> Self {
        Self {
            pending: Vec::with_capacity(4096),
            layer3: Box::new(Layer3::new()),
            synth: Box::default(),
            pcm: [vec![0.0; 1152], vec![0.0; 1152]],
            sample_rate: 0,
            channels: 0,
            crc_check: false,
            free_len: None,
            last: None,
            frames: 0,
            skipped_frames: 0,
        }
    }

    /// Verify frame CRCs when present (off by default, like most players). A frame that fails
    /// the check decodes to silence.
    pub fn set_crc_check(&mut self, on: bool) {
        self.crc_check = on;
    }

    /// Frames decoded since creation.
    pub fn frames_decoded(&self) -> u64 {
        self.frames
    }

    /// Frames that decoded to silence because their data was missing (reservoir after a reset)
    /// or damaged.
    pub fn frames_concealed(&self) -> u64 {
        self.skipped_frames
    }

    /// Bytes of an incomplete frame waiting for the next packet.
    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    /// Decode every complete frame in `pending` and append to `out`.
    fn drain(&mut self, out: &mut AudioBuffer, junk: &mut bool) -> Result<()> {
        let mut pos = 0usize;
        let buf = std::mem::take(&mut self.pending);
        while pos + 4 <= buf.len() {
            let h = match parse_header(&buf[pos..]) {
                Some(h) if self.last.is_some_and(|l| l.same_stream(&h)) => h,
                _ => {
                    // (re)synchronise: a header confirmed by the following one when the data
                    // reaches it
                    self.last = None;
                    match find_sync(&buf[pos..]) {
                        Some(0) => match parse_header(&buf[pos..]) {
                            Some(h) => h,
                            None => break,
                        },
                        Some(off) => {
                            pos += off;
                            *junk = true;
                            continue;
                        }
                        None => {
                            // keep a possible header prefix at the very end
                            let keep = buf.len().saturating_sub(3).max(pos);
                            *junk |= keep > pos;
                            pos = keep;
                            break;
                        }
                    }
                }
            };
            let len = if h.frame_len_bytes != 0 {
                h.frame_len_bytes
            } else {
                match self.free_format_len(&buf, pos, &h) {
                    Some(l) => l,
                    None => break,
                }
            };
            if pos + len > buf.len() {
                break;
            }
            let frame = &buf[pos..pos + len];
            self.decode_frame(&h, frame, out);
            self.last = Some(h);
            pos += len;
        }
        self.pending = buf;
        self.pending.drain(..pos);
        if self.pending.len() > MAX_PENDING {
            self.pending.clear();
        }
        Ok(())
    }

    fn free_format_len(&mut self, buf: &[u8], pos: usize, h: &FrameHeader) -> Option<usize> {
        let unit = if h.layer == 1 { 4 } else { 1 };
        if let Some(base) = self.free_len {
            return Some(base + usize::from(h.padding) * unit);
        }
        let next = next_free_format_header(buf, pos, h)?;
        let len = next - pos;
        self.free_len = Some(len - usize::from(h.padding) * unit);
        Some(len)
    }

    fn decode_frame(&mut self, h: &FrameHeader, frame: &[u8], out: &mut AudioBuffer) {
        if is_info_frame(frame) {
            self.frames += 1;
            return;
        }
        if out.channels.is_empty() {
            out.channels = vec![Vec::with_capacity(h.samples_per_frame * 2); h.channels];
            out.sample_rate = h.sample_rate;
        }
        if self.sample_rate != h.sample_rate || self.channels != h.channels {
            if self.channels != 0 {
                // a new stream: forget the old filter state
                self.synth.iter_mut().for_each(Synth::reset);
                self.layer3.reset();
            }
            self.sample_rate = h.sample_rate;
            self.channels = h.channels;
        }
        if out.sample_rate != h.sample_rate {
            // a rate change inside one packet: such frames cannot share the output buffer
            self.skipped_frames += 1;
            return;
        }
        let n = h.samples_per_frame;
        for p in &mut self.pcm {
            p.resize(n.max(1152), 0.0);
        }
        let res = match h.layer {
            1 => decode_layer1(h, frame, self.crc_check, &mut self.synth, &mut self.pcm),
            2 => decode_layer2(h, frame, self.crc_check, &mut self.synth, &mut self.pcm),
            _ => self
                .layer3
                .decode_frame(h, frame, self.crc_check, &mut self.synth, &mut self.pcm)
                .map(|have| {
                    if !have {
                        self.skipped_frames += 1;
                    }
                }),
        };
        if res.is_err() {
            self.skipped_frames += 1;
            for p in &mut self.pcm {
                p[..n].fill(0.0);
            }
        }
        self.frames += 1;
        // a channel-count change inside one packet: mono is duplicated, stereo keeps the left
        let nin = h.channels;
        for (c, dst) in out.channels.iter_mut().enumerate() {
            dst.extend_from_slice(&self.pcm[c.min(nin - 1)][..n]);
        }
    }
}

impl Decoder for Mp3Decoder {
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        self.pending.extend_from_slice(packet);
        let mut out = AudioBuffer {
            sample_rate: self.sample_rate,
            channels: Vec::new(),
        };
        let mut junk = false;
        self.drain(&mut out, &mut junk)?;
        if out.channels.is_empty() {
            out.channels = vec![Vec::new(); self.channels];
            if junk && self.frames == 0 && self.pending.len() <= 3 {
                return Err(Error::invalid("no MPEG audio frame found"));
            }
        }
        Ok(out)
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.last = None;
        self.layer3.reset();
        self.synth.iter_mut().for_each(Synth::reset);
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> usize {
        self.channels
    }
}
