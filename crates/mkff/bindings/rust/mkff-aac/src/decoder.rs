//! The AAC-LC decoder: `raw_data_block()` parsing (14496-3 4.4.2.1), channel mapping, ADTS /
//! LOAS framing and the HE-AAC fallback.

use crate::adts::parse_adts;
use crate::asc::{object_type, AudioSpecificConfig, ProgramConfig, SbrSignalling};
use crate::filterbank::{ChannelState, Synthesis};
use crate::huffman::books;
use crate::ics::{apply_stereo, band_idx, ChannelData, IcsInfo};
use crate::latm::{loas_frame_length, LatmParser};
use crate::tables::ZERO_HCB;
use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{AudioBuffer, Decoder, Error, Result};

const ID_SCE: u32 = 0;
const ID_CPE: u32 = 1;
const ID_CCE: u32 = 2;
const ID_LFE: u32 = 3;
const ID_DSE: u32 = 4;
const ID_PCE: u32 = 5;
const ID_FIL: u32 = 6;
const ID_END: u32 = 7;

const EXT_SBR_DATA: u32 = 13;
const EXT_SBR_DATA_CRC: u32 = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Sce,
    Cpe,
    Lfe,
}

/// Where the channels of the n-th element of a kind go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    kind: Kind,
    out: [usize; 2],
}

const fn slot(kind: Kind, a: usize, b: usize) -> Slot {
    Slot { kind, out: [a, b] }
}

/// Element-to-channel mapping of the channel configurations, in WAV order
/// (FL FR FC LFE BL BR FLC FRC …).
fn config_layout(cfg: u8) -> Vec<Slot> {
    use Kind::*;
    match cfg {
        1 => vec![slot(Sce, 0, 0)],
        2 => vec![slot(Cpe, 0, 1)],
        3 => vec![slot(Sce, 2, 0), slot(Cpe, 0, 1)],
        4 => vec![slot(Sce, 2, 0), slot(Cpe, 0, 1), slot(Sce, 3, 0)],
        5 => vec![slot(Sce, 2, 0), slot(Cpe, 0, 1), slot(Cpe, 3, 4)],
        6 => vec![
            slot(Sce, 2, 0),
            slot(Cpe, 0, 1),
            slot(Cpe, 4, 5),
            slot(Lfe, 3, 0),
        ],
        // SCE, CPE, CPE, CPE, LFE as 7.1 (FL FR FC LFE BL BR SL SR): front pair, side pair,
        // back pair — how FFmpeg reads and writes configuration 7 (the specification names the
        // pairs centre-front, outside-front and surround)
        7 => vec![
            slot(Sce, 2, 0),
            slot(Cpe, 0, 1),
            slot(Cpe, 6, 7),
            slot(Cpe, 4, 5),
            slot(Lfe, 3, 0),
        ],
        _ => Vec::new(),
    }
}

/// Speaker labels in WAV order; labels from `EXTRA` on are unnamed extra channels.
const FL: usize = 0;
const FC: usize = 2;
const LFE: usize = 3;
const BL: usize = 4;
const FLC: usize = 6;
const BC: usize = 8;
const SL: usize = 9;
const EXTRA: usize = 11;

/// Channel mapping of a program config element.
fn pce_layout(p: &ProgramConfig) -> (Vec<Slot>, usize) {
    // (kind, [label, label]) in bitstream order
    let mut elems: Vec<(Kind, [usize; 2])> = Vec::new();
    let mut extra = EXTRA;
    let mut next_extra = || {
        extra += 1;
        extra - 1
    };
    let front_cpes = p.front.iter().filter(|e| e.0).count();
    let (mut fc_used, mut cpe_seen) = (false, 0);
    for &(cpe, _) in &p.front {
        if cpe {
            cpe_seen += 1;
            let l = if cpe_seen == front_cpes {
                FL
            } else if cpe_seen == front_cpes - 1 {
                FLC
            } else {
                usize::MAX
            };
            let pair = if l == usize::MAX {
                [next_extra(), next_extra()]
            } else {
                [l, l + 1]
            };
            elems.push((Kind::Cpe, pair));
        } else if !fc_used {
            fc_used = true;
            elems.push((Kind::Sce, [FC, 0]));
        } else {
            elems.push((Kind::Sce, [next_extra(), 0]));
        }
    }
    let (mut sl_used, mut lfe_slot_free) = (false, p.lfe.is_empty());
    for &(cpe, _) in &p.side {
        if cpe && !sl_used {
            sl_used = true;
            elems.push((Kind::Cpe, [SL, SL + 1]));
        } else if cpe {
            elems.push((Kind::Cpe, [next_extra(), next_extra()]));
        } else if lfe_slot_free {
            // a lone side mono channel without an LFE takes the 4th position (as in
            // 5.1 programs that code the LFE as a side SCE)
            lfe_slot_free = false;
            elems.push((Kind::Sce, [LFE, 0]));
        } else {
            elems.push((Kind::Sce, [next_extra(), 0]));
        }
    }
    let (mut bl_used, mut bc_used) = (false, false);
    for &(cpe, _) in &p.back {
        if cpe && !bl_used {
            bl_used = true;
            elems.push((Kind::Cpe, [BL, BL + 1]));
        } else if cpe {
            elems.push((Kind::Cpe, [next_extra(), next_extra()]));
        } else if !bc_used {
            bc_used = true;
            elems.push((Kind::Sce, [BC, 0]));
        } else {
            elems.push((Kind::Sce, [next_extra(), 0]));
        }
    }
    for (i, _) in p.lfe.iter().enumerate() {
        elems.push((Kind::Lfe, [if i == 0 { LFE } else { next_extra() }, 0]));
    }
    // labels in use → output indices in label order
    let mut labels: Vec<usize> = Vec::new();
    for (k, l) in &elems {
        labels.push(l[0]);
        if *k == Kind::Cpe {
            labels.push(l[1]);
        }
    }
    labels.sort_unstable();
    let index = |lab: usize| labels.iter().position(|&x| x == lab).unwrap_or(0);
    let slots = elems
        .iter()
        .map(|(k, l)| Slot {
            kind: *k,
            out: [index(l[0]), index(l[1])],
        })
        .collect();
    (slots, labels.len())
}

/// A 2× interpolator (Kaiser-windowed sinc, 32 taps) used to bring an HE-AAC core to the SBR
/// output rate when the SBR layer is not decoded.
#[derive(Clone, Debug)]
struct Upsampler2x {
    hist: Vec<f32>,
}

const UP_HALF: usize = 16;

fn up_kernel() -> &'static [f32; 2 * UP_HALF] {
    static K: std::sync::OnceLock<[f32; 2 * UP_HALF]> = std::sync::OnceLock::new();
    K.get_or_init(|| {
        let mut k = [0f32; 2 * UP_HALF];
        let i0 = |x: f64| {
            let (mut s, mut t) = (1.0f64, 1.0f64);
            for j in 1..40 {
                t *= x * x / 4.0 / (j * j) as f64;
                s += t;
            }
            s
        };
        let beta = 7.0;
        for (j, v) in k.iter_mut().enumerate() {
            // tap j multiplies x[n - j]; the output instant is n - UP_HALF + 0.5
            let d = UP_HALF as f64 - 0.5 - j as f64;
            let r = d / UP_HALF as f64;
            let w = i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0(beta);
            let a = std::f64::consts::PI * d;
            *v = (a.sin() / a * w) as f32;
        }
        k
    })
}

impl Upsampler2x {
    fn new() -> Self {
        Self {
            hist: vec![0.0; 2 * UP_HALF],
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let k = up_kernel();
        let n = 2 * UP_HALF;
        self.hist.extend_from_slice(input);
        for i in 0..input.len() {
            // newest sample is hist[i + n]
            let win = &self.hist[i + 1..i + n + 1];
            let mut s = 0.0;
            for j in 0..n {
                s += win[n - 1 - j] * k[j];
            }
            out.push(self.hist[i + n - UP_HALF]);
            out.push(s);
        }
        self.hist.drain(..input.len());
    }
}

/// Counts of the coding tools met so far (diagnostics; channels of all elements are counted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecoderStats {
    /// Raw data blocks decoded.
    pub blocks: u64,
    /// Channel frames per window sequence: only long, long start, eight short, long stop.
    pub window_sequences: [u64; 4],
    /// Channel frames using the KBD window shape.
    pub kbd_frames: u64,
    /// Channel frames with TNS data.
    pub tns_frames: u64,
    /// Channel frames with pulse data.
    pub pulse_frames: u64,
    /// Bands (× groups) coded with M/S.
    pub ms_bands: u64,
    /// Bands (× groups) coded with intensity stereo.
    pub intensity_bands: u64,
    /// Bands (× groups) coded with PNS.
    pub pns_bands: u64,
    /// Coupling channel elements met (parsed and skipped).
    pub cce: u64,
    /// Data stream elements met.
    pub dse: u64,
    /// Program config elements met in access units.
    pub pce: u64,
    /// Fill elements met.
    pub fil: u64,
    /// Fill elements carrying an SBR payload.
    pub sbr_payloads: u64,
}

impl DecoderStats {
    fn channel(&mut self, cd: &ChannelData) {
        self.window_sequences[usize::from(cd.ics.seq & 3)] += 1;
        self.kbd_frames += u64::from(cd.ics.shape);
        self.tns_frames += u64::from(cd.tns.present);
        self.pulse_frames += u64::from(cd.pulse_present);
        for g in 0..cd.ics.groups {
            for b in 0..cd.ics.max_sfb {
                match cd.band_type[band_idx(g, b)] {
                    crate::tables::NOISE_HCB => self.pns_bands += 1,
                    crate::tables::INTENSITY_HCB | crate::tables::INTENSITY_HCB2 => {
                        self.intensity_bands += 1
                    }
                    _ => {}
                }
            }
        }
    }
}

/// AAC-LC decoder (`mkff_audio_core::Decoder`).
///
/// - With an `AudioSpecificConfig` ([`AacDecoder::new`] with a non-empty slice) every packet is
///   one raw access unit (`raw_data_block`).
/// - With an empty configuration every packet is one ADTS frame (or one LOAS
///   `AudioSyncStream` frame); the configuration comes from each header.
///
/// HE-AAC (SBR, PS): the SBR payload is not decoded. The AAC-LC core is decoded and
/// interpolated 2× so that the output rate is the SBR rate (32 output samples of extra
/// delay); with parametric stereo the mono core is output on both channels. SBR is recognised
/// from explicit signalling in the configuration, or implicitly from an SBR extension
/// payload in the first frame of a stream whose core rate is 24 kHz or less.
///
/// Output is planar f32 at ±1.0 full scale in WAV channel order. Channel configuration 7 is
/// output as 7.1 (FL FR FC LFE BL BR SL SR, its three pairs taken as front, side and back, as
/// FFmpeg does); program config elements are mapped onto WAV positions (front centre, front
/// pair, inner front pair, side pair, back pair, back centre, LFE), extra channels last.
pub struct AacDecoder {
    adts_mode: bool,
    asc: Option<AudioSpecificConfig>,
    sf_index: u8,
    slots: Vec<Slot>,
    used: Vec<bool>,
    nch: usize,
    states: Vec<ChannelState>,
    syn: Synthesis,
    cd: [ChannelData; 2],
    scratch: Box<ChannelData>,
    ms: [bool; 128],
    rng: u32,
    out: Vec<Vec<f32>>,
    latm: LatmParser,
    /// Interpolate the core 2× (SBR not decoded).
    upsample: bool,
    upsamplers: Vec<Upsampler2x>,
    /// Duplicate a mono core to stereo (parametric stereo not decoded).
    ps_dup: bool,
    sbr_seen: bool,
    frames: u64,
    stats: DecoderStats,
}

impl AacDecoder {
    /// Create a decoder from an `AudioSpecificConfig` (raw access units), or from an empty
    /// slice for ADTS / LOAS packets.
    ///
    /// Errors: [`Error::Unsupported`] for AAC Main / SSR / LTP, ER object types and 960-sample
    /// frames; [`Error::InvalidData`] for a malformed configuration.
    pub fn new(asc: &[u8]) -> Result<Self> {
        let mut d = Self {
            adts_mode: asc.is_empty(),
            asc: None,
            sf_index: 4,
            slots: Vec::new(),
            used: Vec::new(),
            nch: 0,
            states: Vec::new(),
            syn: Synthesis::new(),
            cd: [ChannelData::default(), ChannelData::default()],
            scratch: Box::default(),
            ms: [false; 128],
            rng: 0x1234_5678,
            out: Vec::new(),
            latm: LatmParser::new(),
            upsample: false,
            upsamplers: Vec::new(),
            ps_dup: false,
            sbr_seen: false,
            frames: 0,
            stats: DecoderStats::default(),
        };
        let _ = books();
        if !asc.is_empty() {
            let c = AudioSpecificConfig::parse(asc)?;
            d.configure(c)?;
        }
        Ok(d)
    }

    /// Create a decoder from a parsed configuration.
    pub fn from_config(asc: AudioSpecificConfig) -> Result<Self> {
        let mut d = Self::new(&[])?;
        d.adts_mode = false;
        d.configure(asc)?;
        Ok(d)
    }

    /// The active configuration (`None` in ADTS mode before the first frame).
    pub fn config(&self) -> Option<&AudioSpecificConfig> {
        self.asc.as_ref()
    }

    /// Counts of the coding tools met so far.
    pub fn stats(&self) -> &DecoderStats {
        &self.stats
    }

    /// Whether the output is the core interpolated to the SBR rate (HE-AAC detected).
    pub fn sbr_upsampling(&self) -> bool {
        self.upsample
    }

    fn configure(&mut self, c: AudioSpecificConfig) -> Result<()> {
        match c.object_type {
            object_type::AAC_LC => {}
            object_type::AAC_MAIN => return Err(Error::unsupported("AAC Main object type")),
            object_type::AAC_SSR => return Err(Error::unsupported("AAC SSR object type")),
            object_type::AAC_LTP => return Err(Error::unsupported("AAC LTP object type")),
            t => return Err(Error::unsupported(format!("audio object type {t}"))),
        }
        if c.frame_length_flag {
            return Err(Error::unsupported("960-sample frames (frameLengthFlag)"));
        }
        if c.depends_on_core_coder {
            return Err(Error::unsupported("scalable core coder"));
        }
        let (slots, nch) = if c.channel_config == 0 {
            match &c.pce {
                Some(p) => pce_layout(p),
                None => (Vec::new(), 0),
            }
        } else {
            let s = config_layout(c.channel_config);
            let n = match c.channel_config {
                7 => 8,
                n => usize::from(n),
            };
            (s, n)
        };
        self.sf_index = c.band_index();
        self.ps_dup = c.ps_present && nch == 1;
        self.upsample = c.sbr_present && c.extension_sample_rate == 2 * c.sample_rate;
        if c.sbr_present && !self.upsample && c.extension_sample_rate != c.sample_rate {
            return Err(Error::unsupported("SBR with a rate ratio other than 2"));
        }
        self.set_layout(slots, nch);
        self.asc = Some(c);
        self.frames = 0;
        self.sbr_seen = false;
        Ok(())
    }

    fn set_layout(&mut self, slots: Vec<Slot>, nch: usize) {
        self.used = vec![false; slots.len()];
        self.slots = slots;
        self.nch = nch;
        self.states = vec![ChannelState::default(); nch];
        self.out = vec![vec![0.0; 1024]; nch];
        let outn = if self.ps_dup { 2 } else { nch };
        self.upsamplers = vec![Upsampler2x::new(); outn];
    }

    fn out_channels(&self) -> usize {
        if self.ps_dup {
            2
        } else {
            self.nch
        }
    }

    fn take_slot(&mut self, kind: Kind) -> Option<Slot> {
        for (i, s) in self.slots.iter().enumerate() {
            if s.kind == kind && !self.used[i] {
                self.used[i] = true;
                return Some(*s);
            }
        }
        None
    }

    /// Decode one `raw_data_block`; returns the bytes consumed.
    fn decode_block(&mut self, data: &[u8]) -> Result<usize> {
        let mut br = BitReader::new(data);
        let sf = self.sf_index;
        for o in &mut self.out {
            o.fill(0.0);
        }
        self.used.fill(false);
        let mut elements = 0usize;
        loop {
            let id = br.read(3)?;
            elements += 1;
            if elements > 256 {
                return Err(Error::invalid("too many syntactic elements"));
            }
            match id {
                ID_SCE | ID_LFE => {
                    br.skip(4)?;
                    self.cd[0].parse(&mut br, false, sf)?;
                    let kind = if id == ID_SCE { Kind::Sce } else { Kind::Lfe };
                    self.stats.channel(&self.cd[0]);
                    let cd = &mut self.cd[0];
                    cd.apply_pns(&mut self.rng, None);
                    cd.apply_tns(sf);
                    if let Some(s) = self.take_slot(kind) {
                        self.synth(0, s.out[0]);
                    }
                }
                ID_CPE => {
                    br.skip(4)?;
                    let common = br.read_bit()?;
                    let mut msp = 0;
                    self.ms = [false; 128];
                    if common {
                        let info = IcsInfo::parse(&mut br, sf)?;
                        self.cd[0].ics = info;
                        self.cd[1].ics = info;
                        msp = br.read(2)?;
                        match msp {
                            1 => {
                                for g in 0..info.groups {
                                    for b in 0..info.max_sfb {
                                        self.ms[band_idx(g, b)] = br.read_bit()?;
                                    }
                                }
                            }
                            2 => {
                                for g in 0..info.groups {
                                    for b in 0..info.max_sfb {
                                        self.ms[band_idx(g, b)] = true;
                                    }
                                }
                            }
                            3 => return Err(Error::invalid("reserved ms_mask_present")),
                            _ => {}
                        }
                    }
                    self.cd[0].parse(&mut br, common, sf)?;
                    self.cd[1].parse(&mut br, common, sf)?;
                    self.stats.channel(&self.cd[0]);
                    self.stats.channel(&self.cd[1]);
                    self.stats.ms_bands += self.ms.iter().filter(|&&m| m).count() as u64;
                    let (a, b) = self.cd.split_at_mut(1);
                    a[0].apply_pns(&mut self.rng, None);
                    b[0].apply_pns(&mut self.rng, Some((&a[0], &self.ms)));
                    if common {
                        apply_stereo(&mut a[0], &mut b[0], &self.ms, msp);
                    }
                    a[0].apply_tns(sf);
                    b[0].apply_tns(sf);
                    if let Some(s) = self.take_slot(Kind::Cpe) {
                        self.synth(0, s.out[0]);
                        self.synth(1, s.out[1]);
                    }
                }
                ID_CCE => {
                    self.stats.cce += 1;
                    self.skip_cce(&mut br)?;
                }
                ID_DSE => {
                    self.stats.dse += 1;
                    br.skip(4)?;
                    let align = br.read_bit()?;
                    let mut cnt = br.read(8)? as usize;
                    if cnt == 255 {
                        cnt += br.read(8)? as usize;
                    }
                    if align {
                        br.byte_align();
                    }
                    br.skip(cnt * 8)?;
                }
                ID_PCE => {
                    self.stats.pce += 1;
                    let p = ProgramConfig::parse(&mut br, 0)?;
                    let cfg0 = self.asc.as_ref().is_some_and(|a| a.channel_config == 0);
                    if cfg0 && self.asc.as_ref().and_then(|a| a.pce.as_ref()) != Some(&p) {
                        let (slots, n) = pce_layout(&p);
                        if let Some(a) = self.asc.as_mut() {
                            a.pce = Some(p);
                        }
                        self.set_layout(slots, n);
                    }
                }
                ID_FIL => {
                    self.stats.fil += 1;
                    let mut cnt = br.read(4)? as usize;
                    if cnt == 15 {
                        cnt += br.read(8)? as usize;
                        cnt -= 1;
                    }
                    if cnt > 0 {
                        let t = br.peek(4);
                        if t == EXT_SBR_DATA || t == EXT_SBR_DATA_CRC {
                            self.sbr_seen = true;
                            self.stats.sbr_payloads += 1;
                        }
                    }
                    br.skip(cnt * 8)?;
                }
                _ => {
                    debug_assert_eq!(id, ID_END);
                    break;
                }
            }
        }
        br.byte_align();
        self.stats.blocks += 1;
        Ok(br.position() / 8)
    }

    fn synth(&mut self, which: usize, ch: usize) {
        if ch >= self.nch {
            return;
        }
        let cd = &self.cd[which];
        self.syn.run(
            &mut self.states[ch],
            &cd.coef[..],
            cd.ics.seq,
            cd.ics.shape,
            &mut self.out[ch],
        );
    }

    /// Parse a coupling channel element and drop it (its contribution is not applied).
    fn skip_cce(&mut self, br: &mut BitReader) -> Result<()> {
        br.skip(4)?;
        let ind_sw = br.read_bit()?;
        let num_coupled = br.read(3)?;
        let mut lists = 0;
        for _ in 0..=num_coupled {
            lists += 1;
            let is_cpe = br.read_bit()?;
            br.skip(4)?;
            if is_cpe {
                let l = br.read_bit()?;
                let r = br.read_bit()?;
                if l && r {
                    lists += 1;
                }
            }
        }
        br.skip(4)?; // cc_domain, gain_element_sign, gain_element_scale
        let sc = &mut self.scratch;
        sc.parse(br, false, self.sf_index)?;
        let sfb = &books().sf;
        for _ in 1..lists {
            let cge = if ind_sw { true } else { br.read_bit()? };
            if cge {
                sfb.decode(br)?;
            } else {
                for g in 0..sc.ics.groups {
                    for b in 0..sc.ics.max_sfb {
                        if sc.band_type[band_idx(g, b)] != ZERO_HCB {
                            sfb.decode(br)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// After a block: deliver the 1024 core samples per channel (upsampled / duplicated).
    fn emit(&mut self, dst: &mut AudioBuffer) {
        let n_out = self.out_channels();
        if dst.channels.len() != n_out {
            dst.channels = vec![Vec::new(); n_out];
        }
        for c in 0..n_out {
            let src = &self.out[c.min(self.nch.saturating_sub(1))];
            if self.upsample {
                self.upsamplers[c].process(src, &mut dst.channels[c]);
            } else {
                dst.channels[c].extend_from_slice(src);
            }
        }
    }

    fn after_first_frame(&mut self) {
        if self.frames == 0 && self.sbr_seen && !self.upsample {
            if let Some(a) = &self.asc {
                if a.sbr_signalling == SbrSignalling::None && a.sample_rate <= 24000 {
                    self.upsample = true;
                }
            }
        }
        self.frames += 1;
    }

    fn decode_raw(&mut self, data: &[u8], dst: &mut AudioBuffer) -> Result<usize> {
        if self.nch == 0 {
            // channel configuration 0 without a PCE yet: parse anyway (a PCE may set it)
        }
        let used = self.decode_block(data)?;
        self.after_first_frame();
        if self.nch > 0 {
            self.emit(dst);
        }
        Ok(used)
    }

    fn decode_adts(&mut self, packet: &[u8], dst: &mut AudioBuffer) -> Result<()> {
        let h = parse_adts(packet).ok_or_else(|| Error::invalid("no ADTS header"))?;
        if packet.len() < h.frame_length {
            return Err(Error::EndOfData);
        }
        let asc = h.to_asc();
        let same = self.asc.as_ref().is_some_and(|a| {
            a.object_type == asc.object_type
                && a.sampling_index == asc.sampling_index
                && a.channel_config == asc.channel_config
        });
        if !same {
            let keep_upsample = self.upsample;
            let had = self.asc.is_some();
            self.configure(asc)?;
            if had {
                self.upsample =
                    keep_upsample && self.asc.as_ref().is_some_and(|a| a.sample_rate <= 24000);
                self.frames = 1;
            }
        }
        let body = &packet[h.header_length..h.frame_length];
        let mut pos = 0;
        for _ in 0..h.raw_blocks {
            if pos >= body.len() {
                return Err(Error::EndOfData);
            }
            pos += self.decode_raw(&body[pos..], dst)?;
            if h.has_crc && h.raw_blocks > 1 {
                pos += 2;
            }
        }
        Ok(())
    }

    fn decode_loas(&mut self, packet: &[u8], dst: &mut AudioBuffer) -> Result<()> {
        let aus = self.latm.parse_loas_frame(packet)?;
        if self.latm.take_config_changed() || self.asc.is_none() {
            let c = self
                .latm
                .config()
                .cloned()
                .ok_or_else(|| Error::invalid("LOAS without configuration"))?;
            self.configure(c)?;
        }
        for au in aus {
            self.decode_raw(&au, dst)?;
        }
        Ok(())
    }
}

impl Decoder for AacDecoder {
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        let mut dst = AudioBuffer {
            sample_rate: 0,
            channels: Vec::new(),
        };
        if packet.is_empty() {
            dst.sample_rate = self.sample_rate();
            dst.channels = vec![Vec::new(); self.channels()];
            return Ok(dst);
        }
        let is_adts = parse_adts(packet).is_some_and(|h| h.frame_length <= packet.len());
        if self.adts_mode {
            if is_adts {
                self.decode_adts(packet, &mut dst)?;
            } else if loas_frame_length(packet).is_some() {
                self.decode_loas(packet, &mut dst)?;
            } else {
                return Err(Error::invalid("packet is neither ADTS nor LOAS"));
            }
        } else if is_adts && parse_adts(packet).is_some_and(|h| h.frame_length == packet.len()) {
            self.decode_adts(packet, &mut dst)?;
        } else {
            self.decode_raw(packet, &mut dst)?;
        }
        dst.sample_rate = self.sample_rate();
        if dst.channels.is_empty() {
            dst.channels = vec![Vec::new(); self.channels()];
        }
        Ok(dst)
    }

    fn reset(&mut self) {
        for s in &mut self.states {
            *s = ChannelState::default();
        }
        for u in &mut self.upsamplers {
            *u = Upsampler2x::new();
        }
        self.rng = 0x1234_5678;
    }

    fn sample_rate(&self) -> u32 {
        match &self.asc {
            Some(a) if self.upsample => a.sample_rate * 2,
            Some(a) => a.sample_rate,
            None => 0,
        }
    }

    fn channels(&self) -> usize {
        self.out_channels()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pce_5_1_maps_to_wav_order() {
        let p = ProgramConfig {
            front: vec![(false, 0), (true, 0)],
            back: vec![(true, 1)],
            lfe: vec![0],
            ..Default::default()
        };
        let (slots, n) = pce_layout(&p);
        assert_eq!(n, 6);
        assert_eq!(slots[0].out[0], 2);
        assert_eq!(slots[1].out, [0, 1]);
        assert_eq!(slots[2].out, [4, 5]);
        assert_eq!(slots[3].out[0], 3);
    }

    #[test]
    fn upsampler_passes_low_tone() {
        let mut u = Upsampler2x::new();
        let x: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.05).sin()).collect();
        let mut y = Vec::new();
        u.process(&x, &mut y);
        // y[2n] = x[n-16]; y[2n+1] ≈ x(n - 15.5)
        for n in 100..4000 {
            let want = ((n as f32 - 15.5) * 0.05).sin();
            assert!((y[2 * n + 1] - want).abs() < 2e-3, "{n}");
        }
    }
}
