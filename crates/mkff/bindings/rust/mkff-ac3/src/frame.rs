//! Decoding of one syncframe of one (sub)stream: bit stream information, audio frame (E-AC-3)
//! and audio blocks, through to PCM.

use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{Error, Result};

use crate::bitalloc::{allocate, decode_exponents, BaParams, ChannelBa, Dba};
use crate::header::{nfchans, FrameInfo, StreamType};
use crate::imdct::Imdct;
use crate::tables::*;
use crate::vq::*;

/// Index of the coupling channel in per-channel allocation arrays.
pub(crate) const CPL: usize = 5;
/// Index of the LFE channel in per-channel allocation arrays.
pub(crate) const LFE: usize = 6;

/// 2^-e for exponents 0..=24.
const EXP_SCALE: [f32; 25] = {
    let mut t = [0f32; 25];
    let mut i = 0;
    let mut v = 1.0f32;
    while i < 25 {
        t[i] = v;
        v *= 0.5;
        i += 1;
    }
    t
};

/// Mantissa bits of the asymmetric quantizers for bap 6..=15 (Table 6.17).
const ASYM_BITS: [u32; 10] = [5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

/// Per-channel exponent / allocation state.
#[derive(Clone)]
pub(crate) struct AllocChan {
    pub exp: [u8; 256],
    pub bap: [u8; 256],
    pub end: usize,
    pub fsnroffst: u8,
    pub fgaincod: u8,
    pub dba: Dba,
    /// Exponents have been decoded at least once in this frame (reuse is legal).
    pub have_exp: bool,
    /// Exponents changed since the last bit allocation.
    pub exp_new: bool,
    /// Inputs of the last bit allocation: it is only rerun when they or the exponents change.
    ba_key: Option<BaKey>,
}

/// Everything besides the exponents that the bit allocation of a channel depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
struct BaKey {
    codes: [u8; 5],
    hth: usize,
    start: usize,
    end: usize,
    snroffset: i32,
    fgain: i32,
    leak: Option<(i32, i32)>,
    dba: Dba,
    hebap: bool,
}

impl Default for AllocChan {
    fn default() -> Self {
        Self {
            exp: [0; 256],
            bap: [0; 256],
            end: 0,
            fsnroffst: 0,
            fgaincod: 4,
            dba: Dba::default(),
            have_exp: false,
            exp_new: false,
            ba_key: None,
        }
    }
}

/// Group state of the grouped (3, 5 and 11 level) quantizers inside one audio block.
#[derive(Default)]
struct Groups {
    b1: [f32; 3],
    b1n: usize,
    b2: [f32; 3],
    b2n: usize,
    b4: [f32; 2],
    b4n: usize,
}

impl Groups {
    #[inline]
    fn mantissa(&mut self, r: &mut BitReader, bap: u8) -> Result<f32> {
        Ok(match bap {
            1 => {
                if self.b1n == 0 {
                    let g = r.read(5)?;
                    if g >= 27 {
                        return Err(Error::invalid("invalid 3-level mantissa group"));
                    }
                    let q = |c: u32| (c as f32 - 1.0) * (2.0 / 3.0);
                    self.b1 = [q(g / 9), q((g % 9) / 3), q(g % 3)];
                    self.b1n = 3;
                }
                self.b1n -= 1;
                self.b1[2 - self.b1n]
            }
            2 => {
                if self.b2n == 0 {
                    let g = r.read(7)?;
                    if g >= 125 {
                        return Err(Error::invalid("invalid 5-level mantissa group"));
                    }
                    let q = |c: u32| (c as f32 - 2.0) * (2.0 / 5.0);
                    self.b2 = [q(g / 25), q((g % 25) / 5), q(g % 5)];
                    self.b2n = 3;
                }
                self.b2n -= 1;
                self.b2[2 - self.b2n]
            }
            3 => {
                let c = r.read(3)?;
                if c == 7 {
                    return Err(Error::invalid("invalid 7-level mantissa"));
                }
                (c as f32 - 3.0) * (2.0 / 7.0)
            }
            4 => {
                if self.b4n == 0 {
                    let g = r.read(7)?;
                    if g >= 121 {
                        return Err(Error::invalid("invalid 11-level mantissa group"));
                    }
                    let q = |c: u32| (c as f32 - 5.0) * (2.0 / 11.0);
                    self.b4 = [q(g / 11), q(g % 11)];
                    self.b4n = 2;
                }
                self.b4n -= 1;
                self.b4[1 - self.b4n]
            }
            5 => {
                let c = r.read(4)?;
                if c == 15 {
                    return Err(Error::invalid("invalid 15-level mantissa"));
                }
                (c as f32 - 7.0) * (2.0 / 15.0)
            }
            6..=15 => {
                let bits = ASYM_BITS[usize::from(bap) - 6];
                r.read_signed(bits)? as f32 / (1u32 << (bits - 1)) as f32
            }
            _ => 0.0,
        })
    }
}

/// Small deterministic generator for dither and spectral extension noise.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rng(u32);

impl Rng {
    pub(crate) const SEED: u32 = 0x1234_5678;

    #[inline]
    fn next(&mut self) -> u32 {
        // xorshift32
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform in [-1, 1).
    #[inline]
    fn uniform(&mut self) -> f32 {
        (self.next() as i32) as f32 * (1.0 / 2_147_483_648.0)
    }
}

/// Decoder options a frame needs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Options {
    pub drc_scale: f32,
    pub dither: bool,
}

/// State of one substream (the AC-3 stream, an E-AC-3 independent substream, or one dependent
/// substream). Persists across frames for the overlap-add.
pub(crate) struct Substream {
    // frame header
    pub eac3: bool,
    strmtyp: StreamType,
    pub acmod: u8,
    pub nfchans: usize,
    pub lfeon: bool,
    hth: &'static [u16; 50],
    pub blocks: usize,
    pub clev: f32,
    pub slev: f32,
    pub info: Option<FrameInfo>,

    // E-AC-3 audio frame
    expstr: [[u8; 7]; 6],
    cplstre_f: [bool; 6],
    cplinu_f: [bool; 6],
    snroffststr: u8,
    pub(crate) blkswe: bool,
    dithflage: bool,
    bamode: bool,
    frmfgaincode: bool,
    dbaflde: bool,
    skipflde: bool,
    aht: [bool; 7],
    aht_read: [bool; 7],
    gaqmod: [u8; 7],
    frmcsnroffst: u8,
    frmfsnroffst: u8,
    chinspxatten: [bool; 5],
    spxattencod: [u8; 5],
    firstspxcos: [bool; 5],
    firstcplcos: [bool; 5],
    firstcplleak: bool,

    // audio block
    blksw: [bool; 5],
    dithflag: [bool; 5],
    dynrng: [f32; 2],
    cplinu: bool,
    chincpl: [bool; 5],
    phsflginu: bool,
    cplbegf: i32,
    cplendf: i32,
    cplbndstrc: [bool; 18],
    cpl_start: usize,
    cpl_end: usize,
    cplco: [[f32; 18]; 5],
    phsflg: [bool; 18],
    rematflg: [bool; 4],
    nrematbd: usize,
    chbwcod: [u8; 5],
    spxinu: bool,
    chinspx: [bool; 5],
    spxstrtf: usize,
    spxbegf: i32,
    spx_begin: usize,
    spx_end: usize,
    spxbndstrc: [bool; 17],
    spxco: [[f32; 17]; 5],
    spx_nblend: [[f32; 17]; 5],
    spx_sblend: [[f32; 17]; 5],
    sdcycod: u8,
    fdcycod: u8,
    sgaincod: u8,
    dbpbcod: u8,
    floorcod: u8,
    csnroffst: u8,
    cplfleak: u8,
    cplsleak: u8,
    alloc: [AllocChan; 7],
    /// AHT mantissas of the frame after the inverse DCT: `[ch][bin][block]`.
    aht_mant: Vec<[[f32; 6]; 256]>,

    coef: [[f32; 256]; 7],
    delay: [[f32; 256]; 6],
    /// PCM of the last frame: fbw channels then the LFE, `blocks * 256` samples each.
    pub pcm: Vec<Vec<f32>>,
    rng: Rng,
    /// Bit position of each audio block in the last frame, and of the end of the audio data.
    pub(crate) block_bits: [usize; 7],
    /// Bit position of the E-AC-3 audio frame element.
    pub(crate) audfrm_bit: usize,
    /// Coding tool usage counters (blocks), for tests.
    pub(crate) stats: Stats,
    /// Positions of conversion-related E-AC-3 fields of the last frame, for tests.
    pub(crate) marks: Marks,
}

/// Bit positions of E-AC-3 fields that differ between independent and dependent substreams.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Marks {
    /// Where `chanmape` is (or would be).
    pub chanmap_at: usize,
    /// The `convexpstre` / `convexpstr` fields.
    pub convexp: (usize, usize),
    /// The `convsnroffste` / `convsnroffst` fields of each block.
    pub convsnr: [(usize, usize); 6],
}

/// How often coding tools were used, counted in audio blocks.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stats {
    pub blocks: usize,
    pub blksw: usize,
    pub coupling: usize,
    pub phase_flags: usize,
    pub rematrix: usize,
    pub dither: usize,
    pub delta_ba: usize,
    pub spx: usize,
    pub aht: usize,
}

impl Substream {
    pub(crate) fn new() -> Self {
        Self {
            eac3: false,
            strmtyp: StreamType::Ac3,
            acmod: 2,
            nfchans: 2,
            lfeon: false,
            hth: &HTH[0],
            blocks: 6,
            clev: CMIXLEV[0],
            slev: SURMIXLEV[0],
            info: None,
            expstr: [[0; 7]; 6],
            cplstre_f: [false; 6],
            cplinu_f: [false; 6],
            snroffststr: 0,
            blkswe: false,
            dithflage: false,
            bamode: false,
            frmfgaincode: false,
            dbaflde: false,
            skipflde: false,
            aht: [false; 7],
            aht_read: [false; 7],
            gaqmod: [0; 7],
            frmcsnroffst: 0,
            frmfsnroffst: 0,
            chinspxatten: [false; 5],
            spxattencod: [0; 5],
            firstspxcos: [true; 5],
            firstcplcos: [true; 5],
            firstcplleak: true,
            blksw: [false; 5],
            dithflag: [false; 5],
            dynrng: [1.0; 2],
            cplinu: false,
            chincpl: [false; 5],
            phsflginu: false,
            cplbegf: 0,
            cplendf: 0,
            cplbndstrc: [false; 18],
            cpl_start: 0,
            cpl_end: 0,
            cplco: [[0.0; 18]; 5],
            phsflg: [false; 18],
            rematflg: [false; 4],
            nrematbd: 0,
            chbwcod: [0; 5],
            spxinu: false,
            chinspx: [false; 5],
            spxstrtf: 0,
            spxbegf: 0,
            spx_begin: 0,
            spx_end: 0,
            spxbndstrc: DEFAULT_SPX_BNDSTRC,
            spxco: [[0.0; 17]; 5],
            spx_nblend: [[0.0; 17]; 5],
            spx_sblend: [[0.0; 17]; 5],
            sdcycod: 0,
            fdcycod: 0,
            sgaincod: 0,
            dbpbcod: 0,
            floorcod: 0,
            csnroffst: 0,
            cplfleak: 0,
            cplsleak: 0,
            alloc: Default::default(),
            aht_mant: Vec::new(),
            coef: [[0.0; 256]; 7],
            delay: [[0.0; 256]; 6],
            pcm: vec![vec![0.0; 1536]; 6],
            rng: Rng(Rng::SEED),
            block_bits: [0; 7],
            audfrm_bit: 0,
            stats: Stats::default(),
            marks: Marks::default(),
        }
    }

    /// Forget the overlap (after a seek).
    pub(crate) fn reset(&mut self) {
        self.delay = [[0.0; 256]; 6];
        self.rng = Rng(Rng::SEED);
    }

    /// Number of coded channels (fbw + LFE).
    pub(crate) fn channels(&self) -> usize {
        self.nfchans + usize::from(self.lfeon)
    }

    /// Decode one complete syncframe into `self.pcm`.
    pub(crate) fn decode_frame(
        &mut self,
        frame: &[u8],
        info: &FrameInfo,
        imdct: &mut Imdct,
        opt: &Options,
    ) -> Result<()> {
        let mut r = BitReader::new(frame);
        let layout_changed = self.info.as_ref().is_none_or(|old| {
            old.acmod != info.acmod || old.lfe != info.lfe || old.chanmap != info.chanmap
        });
        if layout_changed {
            self.delay = [[0.0; 256]; 6];
        }
        self.eac3 = info.is_eac3();
        self.strmtyp = info.stream_type;
        self.acmod = info.acmod;
        self.nfchans = nfchans(info.acmod);
        self.lfeon = info.lfe;
        self.blocks = info.blocks_per_frame;
        self.info = Some(info.clone());
        if self.eac3 {
            self.parse_eac3_bsi(&mut r)?;
            self.parse_audfrm(&mut r, info)?;
        } else {
            self.parse_ac3_bsi(&mut r)?;
            self.spxinu = false;
            self.chinspx = [false; 5];
            self.aht = [false; 7];
        }
        for a in self.alloc.iter_mut() {
            a.have_exp = false;
            a.ba_key = None;
        }
        for blk in 0..self.blocks {
            self.decode_block(&mut r, blk, imdct, opt)?;
        }
        self.block_bits[self.blocks] = r.position();
        Ok(())
    }

    fn parse_ac3_bsi(&mut self, r: &mut BitReader) -> Result<()> {
        r.skip(32)?;
        let fscod = r.read(2)? as usize;
        r.skip(6)?;
        let bsid = r.read(5)?;
        r.skip(3)?;
        let acmod = r.read(3)?;
        self.hth = &HTH[fscod.min(2)];
        self.clev = CMIXLEV[0];
        self.slev = SURMIXLEV[0];
        if acmod & 1 != 0 && acmod != 1 {
            self.clev = CMIXLEV[r.read(2)? as usize];
        }
        if acmod & 4 != 0 {
            self.slev = SURMIXLEV[r.read(2)? as usize];
        }
        if acmod == 2 {
            r.skip(2)?;
        }
        r.skip(1 + 5)?; // lfeon, dialnorm
        if r.read_bit()? {
            r.skip(8)?;
        }
        if r.read_bit()? {
            r.skip(8)?;
        }
        if r.read_bit()? {
            r.skip(7)?;
        }
        if acmod == 0 {
            r.skip(5)?;
            if r.read_bit()? {
                r.skip(8)?;
            }
            if r.read_bit()? {
                r.skip(8)?;
            }
            if r.read_bit()? {
                r.skip(7)?;
            }
        }
        r.skip(2)?; // copyrightb, origbs
        if bsid == 6 {
            // Annex D alternate syntax
            if r.read_bit()? {
                r.skip(2 + 3 + 3)?;
                let c = r.read(3)? as usize;
                let s = r.read(3)? as usize;
                if acmod & 1 != 0 && acmod != 1 {
                    self.clev = LORO_CMIXLEV[c];
                }
                if acmod & 4 != 0 {
                    self.slev = LORO_SURMIXLEV[s];
                }
            }
            if r.read_bit()? {
                r.skip(14)?;
            }
        } else {
            if r.read_bit()? {
                r.skip(14)?;
            }
            if r.read_bit()? {
                r.skip(14)?;
            }
        }
        if r.read_bit()? {
            let l = r.read(6)? as usize;
            r.skip((l + 1) * 8)?;
        }
        Ok(())
    }

    fn parse_eac3_bsi(&mut self, r: &mut BitReader) -> Result<()> {
        r.skip(16)?;
        let strmtyp = r.read(2)?;
        r.skip(3 + 11)?;
        let fscod = r.read(2)? as usize;
        let numblkscod = if fscod == 3 {
            let fscod2 = r.read(2)? as usize;
            self.hth = &HTH[fscod2.min(2)];
            3
        } else {
            self.hth = &HTH[fscod];
            r.read(2)?
        };
        let acmod = r.read(3)?;
        let lfeon = r.read_bit()?;
        r.skip(5 + 5)?; // bsid, dialnorm
        if r.read_bit()? {
            r.skip(8)?;
        }
        if acmod == 0 {
            r.skip(5)?;
            if r.read_bit()? {
                r.skip(8)?;
            }
        }
        self.marks.chanmap_at = r.position();
        if strmtyp == 1 && r.read_bit()? {
            r.skip(16)?;
        }
        // without mixing metadata: the intermediate levels AC-3 uses for its reserved codes
        self.clev = CMIXLEV[1];
        self.slev = SURMIXLEV[1];
        if r.read_bit()? {
            // mixmdate
            if acmod > 2 {
                r.skip(2)?;
            }
            if acmod & 1 != 0 && acmod > 2 {
                r.skip(3)?;
                self.clev = LORO_CMIXLEV[r.read(3)? as usize];
            }
            if acmod & 4 != 0 {
                r.skip(3)?;
                self.slev = LORO_SURMIXLEV[r.read(3)? as usize];
            }
            if lfeon && r.read_bit()? {
                r.skip(5)?;
            }
            if strmtyp == 0 {
                if r.read_bit()? {
                    r.skip(6)?;
                }
                if acmod == 0 && r.read_bit()? {
                    r.skip(6)?;
                }
                if r.read_bit()? {
                    r.skip(6)?;
                }
                match r.read(2)? {
                    1 => r.skip(5)?,
                    2 => r.skip(12)?,
                    3 => {
                        let len = r.read(5)? as usize;
                        r.skip(8 * (len + 2))?;
                    }
                    _ => {}
                }
                if acmod < 2 {
                    if r.read_bit()? {
                        r.skip(14)?;
                    }
                    if acmod == 0 && r.read_bit()? {
                        r.skip(14)?;
                    }
                }
                if r.read_bit()? {
                    if numblkscod == 0 {
                        r.skip(5)?;
                    } else {
                        for _ in 0..self.blocks {
                            if r.read_bit()? {
                                r.skip(5)?;
                            }
                        }
                    }
                }
            }
        }
        if r.read_bit()? {
            // infomdate
            r.skip(3 + 1 + 1)?;
            if acmod == 2 {
                r.skip(4)?;
            }
            if acmod >= 6 {
                r.skip(2)?;
            }
            if r.read_bit()? {
                r.skip(8)?;
            }
            if acmod == 0 && r.read_bit()? {
                r.skip(8)?;
            }
            if fscod < 3 {
                r.skip(1)?;
            }
        }
        if strmtyp == 0 && numblkscod != 3 {
            r.skip(1)?; // convsync
        }
        if strmtyp == 2 {
            let blkid = numblkscod == 3 || r.read_bit()?;
            if blkid {
                r.skip(6)?;
            }
        }
        if r.read_bit()? {
            let l = r.read(6)? as usize;
            r.skip((l + 1) * 8)?;
        }
        Ok(())
    }

    fn parse_audfrm(&mut self, r: &mut BitReader, info: &FrameInfo) -> Result<()> {
        let nblk = self.blocks;
        let nf = self.nfchans;
        self.audfrm_bit = r.position();
        let (expstre, ahte) = if nblk == 6 {
            (r.read_bit()?, r.read_bit()?)
        } else {
            (true, false)
        };
        self.snroffststr = r.read(2)? as u8;
        let transproce = r.read_bit()?;
        self.blkswe = r.read_bit()?;
        self.dithflage = r.read_bit()?;
        self.bamode = r.read_bit()?;
        self.frmfgaincode = r.read_bit()?;
        self.dbaflde = r.read_bit()?;
        self.skipflde = r.read_bit()?;
        let spxattene = r.read_bit()?;
        if self.snroffststr == 3 {
            return Err(Error::invalid("reserved SNR offset strategy"));
        }
        // coupling in use per block
        self.cplstre_f = [false; 6];
        self.cplinu_f = [false; 6];
        if self.acmod > 1 {
            self.cplstre_f[0] = true;
            self.cplinu_f[0] = r.read_bit()?;
            for blk in 1..nblk {
                self.cplstre_f[blk] = r.read_bit()?;
                self.cplinu_f[blk] = if self.cplstre_f[blk] {
                    r.read_bit()?
                } else {
                    self.cplinu_f[blk - 1]
                };
            }
        }
        // exponent strategies
        self.expstr = [[EXP_REUSE; 7]; 6];
        if expstre {
            for blk in 0..nblk {
                if self.cplinu_f[blk] {
                    self.expstr[blk][CPL] = r.read(2)? as u8;
                }
                for ch in 0..nf {
                    self.expstr[blk][ch] = r.read(2)? as u8;
                }
            }
        } else {
            let ncplblks = self.cplinu_f.iter().filter(|&&c| c).count();
            if self.acmod > 1 && ncplblks > 0 {
                let s = r.read(5)? as usize;
                for (e, &v) in self.expstr.iter_mut().zip(&FRM_EXPSTR[s]) {
                    e[CPL] = v;
                }
            }
            for ch in 0..nf {
                let s = r.read(5)? as usize;
                for (e, &v) in self.expstr.iter_mut().zip(&FRM_EXPSTR[s]) {
                    e[ch] = v;
                }
            }
        }
        if self.lfeon {
            for blk in 0..nblk {
                self.expstr[blk][LFE] = r.read(1)? as u8;
            }
        }
        let at = r.position();
        if info.stream_type == StreamType::Independent {
            let convexpstre = nblk == 6 || r.read_bit()?;
            if convexpstre {
                r.skip(5 * nf)?;
            }
        }
        self.marks.convexp = (at, r.position());
        // adaptive hybrid transform
        self.aht = [false; 7];
        self.aht_read = [false; 7];
        if ahte {
            let ncplblks = self.cplinu_f.iter().filter(|&&c| c).count();
            let ncplregs = (0..6)
                .filter(|&b| self.cplinu_f[b] && (self.cplstre_f[b] || self.expstr[b][CPL] != 0))
                .count();
            if ncplblks == 6 && ncplregs == 1 {
                self.aht[CPL] = r.read_bit()?;
            }
            for ch in 0..nf {
                if (0..6).filter(|&b| self.expstr[b][ch] != EXP_REUSE).count() == 1 {
                    self.aht[ch] = r.read_bit()?;
                }
            }
            if self.lfeon && (0..6).filter(|&b| self.expstr[b][LFE] != EXP_REUSE).count() == 1 {
                self.aht[LFE] = r.read_bit()?;
            }
        }
        if self.snroffststr == 0 {
            self.frmcsnroffst = r.read(6)? as u8;
            self.frmfsnroffst = r.read(4)? as u8;
        }
        if transproce {
            // transient pre-noise processing data: parsed, not applied
            for _ in 0..nf {
                if r.read_bit()? {
                    r.skip(10 + 8)?;
                }
            }
        }
        self.chinspxatten = [false; 5];
        if spxattene {
            for ch in 0..nf {
                self.chinspxatten[ch] = r.read_bit()?;
                if self.chinspxatten[ch] {
                    self.spxattencod[ch] = r.read(5)? as u8;
                }
            }
        }
        if nblk != 1 && r.read_bit()? {
            let words = info.frame_size / 2;
            let log2 = usize::BITS - (words - 1).leading_zeros(); // ceil(log2(words))
            r.skip((nblk - 1) * (4 + log2 as usize))?;
        }
        self.firstspxcos = [true; 5];
        self.firstcplcos = [true; 5];
        self.firstcplleak = true;
        self.spxbndstrc = DEFAULT_SPX_BNDSTRC;
        self.cplbndstrc = DEFAULT_CPL_BNDSTRC;
        Ok(())
    }

    #[allow(clippy::needless_range_loop)]
    fn decode_block(
        &mut self,
        r: &mut BitReader,
        blk: usize,
        imdct: &mut Imdct,
        opt: &Options,
    ) -> Result<()> {
        let nf = self.nfchans;
        let eac3 = self.eac3;
        let acmod = self.acmod;
        self.block_bits[blk] = r.position();

        // block switch and dither flags
        for ch in 0..nf {
            self.blksw[ch] = if !eac3 || self.blkswe {
                r.read_bit()?
            } else {
                false
            };
        }
        for ch in 0..nf {
            self.dithflag[ch] = if !eac3 || self.dithflage {
                r.read_bit()?
            } else {
                true
            };
        }
        // dynamic range control
        for i in 0..if acmod == 0 { 2 } else { 1 } {
            if r.read_bit()? {
                let v = r.read(8)?;
                self.dynrng[i] = drc_gain(v, opt.drc_scale);
            } else if blk == 0 {
                self.dynrng[i] = 1.0;
            }
        }

        // spectral extension strategy
        if eac3 {
            let spxstre = blk == 0 || r.read_bit()?;
            if spxstre {
                self.spxinu = r.read_bit()?;
                if self.spxinu {
                    if acmod == 1 {
                        self.chinspx[0] = true;
                    } else {
                        for ch in 0..nf {
                            self.chinspx[ch] = r.read_bit()?;
                        }
                    }
                    self.spxstrtf = r.read(2)? as usize;
                    let begf = r.read(3)? as usize;
                    let endf = r.read(3)? as usize;
                    self.spxbegf = begf as i32;
                    self.spx_begin = if begf < 6 { begf + 2 } else { begf * 2 - 3 };
                    self.spx_end = if endf < 3 { endf + 5 } else { endf * 2 + 3 };
                    if self.spx_begin >= self.spx_end || self.spxstrtf >= self.spx_begin {
                        return Err(Error::invalid("invalid spectral extension range"));
                    }
                    if r.read_bit()? {
                        for bnd in self.spx_begin + 1..self.spx_end {
                            self.spxbndstrc[bnd] = r.read_bit()?;
                        }
                    }
                } else {
                    self.chinspx = [false; 5];
                    self.firstspxcos = [true; 5];
                }
            }
            if self.spxinu {
                let (nbnds, sizes) = self.spx_bands();
                for ch in 0..nf {
                    if !self.chinspx[ch] {
                        self.firstspxcos[ch] = true;
                        continue;
                    }
                    let spxcoe = if self.firstspxcos[ch] {
                        self.firstspxcos[ch] = false;
                        true
                    } else {
                        r.read_bit()?
                    };
                    if spxcoe {
                        let blend = r.read(5)? as f32 / 32.0;
                        let mstr = r.read(2)? as i32;
                        let mut mant_start = spx_band_start(self.spx_begin);
                        let end_mant = spx_band_start(self.spx_end) as f32;
                        for bnd in 0..nbnds {
                            let e = r.read(4)? as i32;
                            let m = r.read(2)? as f32;
                            let temp = if e == 15 { m / 4.0 } else { (m + 4.0) / 8.0 };
                            self.spxco[ch][bnd] = temp * pow2(-(e + 3 * mstr));
                            let size = sizes[bnd];
                            let nratio = ((mant_start as f32 + 0.5 * size as f32) / end_mant
                                - blend)
                                .clamp(0.0, 1.0);
                            self.spx_nblend[ch][bnd] = nratio.sqrt();
                            self.spx_sblend[ch][bnd] = (1.0 - nratio).sqrt();
                            mant_start += size;
                        }
                    }
                }
            }
        }

        // coupling strategy
        // (E-AC-3 modes without coupling still "send" the strategy in block 0: coupling off)
        let cplstre = if eac3 {
            self.cplstre_f[blk] || blk == 0
        } else {
            r.read_bit()?
        };
        if blk == 0 && !cplstre && !eac3 {
            return Err(Error::invalid("coupling strategy missing in block 0"));
        }
        let prev_cplinu = self.cplinu;
        let prev_cpl_range = (self.cplbegf, self.cplendf);
        if cplstre {
            self.cplinu = if eac3 {
                self.cplinu_f[blk]
            } else {
                r.read_bit()?
            };
            if self.cplinu {
                if eac3 && r.read_bit()? {
                    return Err(Error::unsupported("E-AC-3 enhanced coupling"));
                }
                if eac3 && acmod == 2 {
                    self.chincpl[0] = true;
                    self.chincpl[1] = true;
                } else {
                    for ch in 0..nf {
                        self.chincpl[ch] = r.read_bit()?;
                    }
                }
                self.phsflginu = acmod == 2 && r.read_bit()?;
                self.cplbegf = r.read(4)? as i32;
                self.cplendf = if eac3 && self.spxinu {
                    let b = self.spxbegf;
                    if b < 6 {
                        b - 2
                    } else {
                        b * 2 - 7
                    }
                } else {
                    r.read(4)? as i32
                };
                let nsub = 3 + self.cplendf - self.cplbegf;
                if nsub < 1 {
                    return Err(Error::invalid("coupling end below coupling begin"));
                }
                let read_struct = !eac3 || r.read_bit()?;
                if read_struct {
                    for bnd in 1..nsub as usize {
                        self.cplbndstrc[self.cplbegf as usize + bnd] = r.read_bit()?;
                    }
                }
                self.cpl_start = 37 + 12 * self.cplbegf as usize;
                self.cpl_end = (37 + 12 * (self.cplendf + 3)) as usize;
                if self.chincpl[..nf].iter().filter(|&&c| c).count() < 2 {
                    return Err(Error::invalid("coupling with fewer than two channels"));
                }
            } else {
                for ch in 0..nf {
                    self.chincpl[ch] = false;
                    self.firstcplcos[ch] = true;
                }
                self.firstcplleak = true;
                self.phsflginu = false;
            }
        }
        if !self.cplinu {
            self.chincpl = [false; 5];
        }

        // coupling coordinates and phase flags
        if self.cplinu {
            let nsub = (3 + self.cplendf - self.cplbegf) as usize;
            let begf = self.cplbegf as usize;
            let mut any_cplcoe = false;
            for ch in 0..nf {
                if !self.chincpl[ch] {
                    self.firstcplcos[ch] = true;
                    continue;
                }
                let cplcoe = if eac3 && self.firstcplcos[ch] {
                    self.firstcplcos[ch] = false;
                    true
                } else {
                    r.read_bit()?
                };
                if !eac3 && !cplcoe && (blk == 0 || !prev_cplinu) {
                    return Err(Error::invalid("missing coupling coordinates"));
                }
                if cplcoe {
                    any_cplcoe = true;
                    let mstr = 3 * r.read(2)? as i32;
                    let mut sb = 0;
                    while sb < nsub {
                        let e = r.read(4)? as i32;
                        let m = r.read(4)? as f32;
                        let temp = if e == 15 { m / 16.0 } else { (m + 16.0) / 32.0 };
                        let co = temp * pow2(-(e + mstr));
                        self.cplco[ch][begf + sb] = co;
                        sb += 1;
                        while sb < nsub && self.cplbndstrc[begf + sb] {
                            self.cplco[ch][begf + sb] = co;
                            sb += 1;
                        }
                    }
                }
            }
            if acmod == 2 && self.phsflginu && any_cplcoe {
                let mut sb = 0;
                while sb < nsub {
                    let f = r.read_bit()?;
                    self.phsflg[begf + sb] = f;
                    sb += 1;
                    while sb < nsub && self.cplbndstrc[begf + sb] {
                        self.phsflg[begf + sb] = f;
                        sb += 1;
                    }
                }
            }
        }

        // rematrixing
        if acmod == 2 {
            let rematstr = (eac3 && blk == 0) || r.read_bit()?;
            if blk == 0 && !rematstr {
                return Err(Error::invalid("rematrixing strategy missing in block 0"));
            }
            if rematstr {
                self.nrematbd = if self.cplinu {
                    match self.cplbegf {
                        0 => 2,
                        1 | 2 => 3,
                        _ => 4,
                    }
                } else if eac3 && self.spxinu {
                    if self.spx_begin < 4 {
                        3
                    } else {
                        4
                    }
                } else {
                    4
                };
                for b in 0..self.nrematbd {
                    self.rematflg[b] = r.read_bit()?;
                }
            }
        }

        // exponent strategies
        let mut expstr = [EXP_REUSE; 7];
        if eac3 {
            expstr = self.expstr[blk];
            if !self.cplinu {
                expstr[CPL] = EXP_REUSE;
            }
        } else {
            if self.cplinu {
                expstr[CPL] = r.read(2)? as u8;
            }
            for ch in 0..nf {
                expstr[ch] = r.read(2)? as u8;
            }
            if self.lfeon {
                expstr[LFE] = r.read(1)? as u8;
            }
        }
        if self.cplinu
            && expstr[CPL] == EXP_REUSE
            && (!prev_cplinu
                || !self.alloc[CPL].have_exp
                || prev_cpl_range != (self.cplbegf, self.cplendf))
        {
            return Err(Error::invalid(
                "coupling exponents reused without a previous set",
            ));
        }
        for ch in 0..nf {
            if expstr[ch] != EXP_REUSE && !self.chincpl[ch] && !(eac3 && self.chinspx[ch]) {
                self.chbwcod[ch] = r.read(6)? as u8;
                if self.chbwcod[ch] > 60 {
                    return Err(Error::invalid("channel bandwidth code above 60"));
                }
            }
        }

        // exponents
        if self.cplinu && expstr[CPL] != EXP_REUSE {
            let absexp = (r.read(4)? as i32) << 1;
            let grp = 3usize << (expstr[CPL] - 1);
            let ngrps = (self.cpl_end - self.cpl_start) / grp;
            let start = self.cpl_start;
            let a = &mut self.alloc[CPL];
            decode_exponents(r, absexp, ngrps, grp / 3, &mut a.exp, start, false)?;
            a.end = self.cpl_end;
            a.have_exp = true;
            a.exp_new = true;
        }
        for ch in 0..nf {
            if expstr[ch] == EXP_REUSE {
                if !self.alloc[ch].have_exp {
                    return Err(Error::invalid("exponents reused without a previous set"));
                }
                continue;
            }
            let end = if self.chincpl[ch] {
                self.cpl_start
            } else if eac3 && self.spxinu && self.chinspx[ch] {
                spx_band_start(self.spx_begin)
            } else {
                37 + 3 * (usize::from(self.chbwcod[ch]) + 12)
            };
            let grp = 3usize << (expstr[ch] - 1);
            let ngrps = match expstr[ch] {
                EXP_D15 => (end - 1) / 3,
                EXP_D25 => (end - 1 + 3) / 6,
                _ => (end - 1 + 9) / 12,
            };
            let absexp = r.read(4)? as i32;
            let a = &mut self.alloc[ch];
            decode_exponents(r, absexp, ngrps, grp / 3, &mut a.exp, 0, true)?;
            a.end = end;
            a.have_exp = true;
            a.exp_new = true;
            r.skip(2)?; // gainrng
        }
        if self.lfeon {
            if expstr[LFE] != EXP_REUSE {
                let absexp = r.read(4)? as i32;
                let a = &mut self.alloc[LFE];
                decode_exponents(r, absexp, 2, 1, &mut a.exp, 0, true)?;
                a.end = 7;
                a.have_exp = true;
                a.exp_new = true;
            } else if !self.alloc[LFE].have_exp {
                return Err(Error::invalid(
                    "LFE exponents reused without a previous set",
                ));
            }
        }

        // bit allocation parameters
        if !eac3 || self.bamode {
            if r.read_bit()? {
                self.sdcycod = r.read(2)? as u8;
                self.fdcycod = r.read(2)? as u8;
                self.sgaincod = r.read(2)? as u8;
                self.dbpbcod = r.read(2)? as u8;
                self.floorcod = r.read(3)? as u8;
            } else if blk == 0 && !eac3 {
                return Err(Error::invalid(
                    "bit allocation parameters missing in block 0",
                ));
            }
        } else {
            self.sdcycod = 2;
            self.fdcycod = 1;
            self.sgaincod = 1;
            self.dbpbcod = 2;
            self.floorcod = 7;
        }
        if eac3 {
            if self.snroffststr == 0 {
                self.csnroffst = self.frmcsnroffst;
                for a in self.alloc.iter_mut() {
                    a.fsnroffst = self.frmfsnroffst;
                }
            } else if blk == 0 || r.read_bit()? {
                self.csnroffst = r.read(6)? as u8;
                if self.snroffststr == 1 {
                    let f = r.read(4)? as u8;
                    for a in self.alloc.iter_mut() {
                        a.fsnroffst = f;
                    }
                } else {
                    if self.cplinu {
                        self.alloc[CPL].fsnroffst = r.read(4)? as u8;
                    }
                    for ch in 0..nf {
                        self.alloc[ch].fsnroffst = r.read(4)? as u8;
                    }
                    if self.lfeon {
                        self.alloc[LFE].fsnroffst = r.read(4)? as u8;
                    }
                }
            }
            let fgaincode = self.frmfgaincode && r.read_bit()?;
            if fgaincode {
                if self.cplinu {
                    self.alloc[CPL].fgaincod = r.read(3)? as u8;
                }
                for ch in 0..nf {
                    self.alloc[ch].fgaincod = r.read(3)? as u8;
                }
                if self.lfeon {
                    self.alloc[LFE].fgaincod = r.read(3)? as u8;
                }
            } else {
                for a in self.alloc.iter_mut() {
                    a.fgaincod = 4;
                }
            }
            let at = r.position();
            if self.strmtyp == StreamType::Independent && r.read_bit()? {
                r.skip(10)?; // convsnroffst
            }
            self.marks.convsnr[blk] = (at, r.position());
        } else if r.read_bit()? {
            self.csnroffst = r.read(6)? as u8;
            if self.cplinu {
                self.alloc[CPL].fsnroffst = r.read(4)? as u8;
                self.alloc[CPL].fgaincod = r.read(3)? as u8;
            }
            for ch in 0..nf {
                self.alloc[ch].fsnroffst = r.read(4)? as u8;
                self.alloc[ch].fgaincod = r.read(3)? as u8;
            }
            if self.lfeon {
                self.alloc[LFE].fsnroffst = r.read(4)? as u8;
                self.alloc[LFE].fgaincod = r.read(3)? as u8;
            }
        } else if blk == 0 {
            return Err(Error::invalid("SNR offsets missing in block 0"));
        }
        if self.cplinu {
            let cplleake = if eac3 && self.firstcplleak {
                self.firstcplleak = false;
                true
            } else {
                r.read_bit()?
            };
            if cplleake {
                self.cplfleak = r.read(3)? as u8;
                self.cplsleak = r.read(3)? as u8;
            } else if !eac3 && (blk == 0 || !prev_cplinu) {
                return Err(Error::invalid("coupling leak values missing"));
            }
        }

        // delta bit allocation
        let deltbaie = (!eac3 || self.dbaflde) && r.read_bit()?;
        if deltbaie {
            let mut cpl_mode = 0;
            if self.cplinu {
                cpl_mode = r.read(2)?;
            }
            let mut modes = [0u32; 5];
            for m in modes.iter_mut().take(nf) {
                *m = r.read(2)?;
            }
            if self.cplinu {
                apply_dba_mode(&mut self.alloc[CPL].dba, cpl_mode, r)?;
            }
            for ch in 0..nf {
                apply_dba_mode(&mut self.alloc[ch].dba, modes[ch], r)?;
            }
        } else if blk == 0 {
            for a in self.alloc.iter_mut() {
                a.dba.active = false;
            }
        }

        // skip field
        if (!eac3 || self.skipflde) && r.read_bit()? {
            let l = r.read(9)? as usize;
            r.skip(l * 8)?;
        }

        // bit allocation
        self.run_bit_allocation(blk)?;
        {
            let st = &mut self.stats;
            st.blocks += 1;
            st.blksw += usize::from(self.blksw[..nf].contains(&true));
            st.coupling += usize::from(self.cplinu);
            st.phase_flags += usize::from(self.cplinu && self.phsflginu);
            st.rematrix +=
                usize::from(acmod == 2 && self.rematflg[..self.nrematbd].contains(&true));
            st.dither += usize::from(self.dithflag[..nf].contains(&true));
            st.delta_ba += usize::from(self.alloc.iter().any(|a| a.dba.active));
            st.spx += usize::from(eac3 && self.spxinu);
            st.aht += usize::from(self.aht.contains(&true));
        }

        // mantissas
        self.read_mantissas(r, blk, opt)?;

        // decoupling, spectral extension, rematrixing
        if self.cplinu {
            self.decouple(opt);
        }
        if eac3 && self.spxinu {
            self.spectral_extension();
        }
        if acmod == 2 {
            self.rematrix();
        }

        // dynamic range, inverse transform
        let out_base = blk * 256;
        for ch in 0..nf {
            let g = if acmod == 0 && ch == 1 {
                self.dynrng[1]
            } else {
                self.dynrng[0]
            };
            if g != 1.0 {
                for v in self.coef[ch].iter_mut() {
                    *v *= g;
                }
            }
            let out = &mut self.pcm[ch][out_base..out_base + 256];
            imdct.transform(&self.coef[ch], self.blksw[ch], &mut self.delay[ch], out);
        }
        if self.lfeon {
            let g = self.dynrng[0];
            if g != 1.0 {
                for v in self.coef[LFE].iter_mut() {
                    *v *= g;
                }
            }
            let out = &mut self.pcm[nf][out_base..out_base + 256];
            imdct.transform(&self.coef[LFE], false, &mut self.delay[5], out);
        }
        Ok(())
    }

    /// Spectral extension band count and sizes (E.2.6.2).
    fn spx_bands(&self) -> (usize, [usize; 17]) {
        let mut sizes = [0usize; 17];
        let mut n = 1;
        sizes[0] = 12;
        for bnd in self.spx_begin + 1..self.spx_end {
            if self.spxbndstrc[bnd] {
                sizes[n - 1] += 12;
            } else {
                sizes[n] = 12;
                n += 1;
            }
        }
        (n, sizes)
    }

    #[allow(clippy::needless_range_loop)]
    fn run_bit_allocation(&mut self, blk: usize) -> Result<()> {
        let params = BaParams {
            sdcycod: self.sdcycod,
            fdcycod: self.fdcycod,
            sgaincod: self.sgaincod,
            dbpbcod: self.dbpbcod,
            floorcod: self.floorcod,
            hth: self.hth,
        };
        let nf = self.nfchans;
        let mut chans = [false; 7];
        chans[..nf].fill(true);
        chans[CPL] = self.cplinu;
        chans[LFE] = self.lfeon;
        // all SNR offsets zero: no mantissa bits at all
        let all_zero = self.csnroffst == 0
            && (0..7)
                .filter(|&c| chans[c])
                .all(|c| self.alloc[c].fsnroffst == 0);
        for c in 0..7 {
            if !chans[c] {
                continue;
            }
            // AHT channels only allocate in the block that carries their mantissas
            if self.aht[c] && (blk != 0 || self.aht_read[c]) {
                continue;
            }
            let a = &mut self.alloc[c];
            if all_zero {
                a.bap = [0; 256];
                a.ba_key = None;
                continue;
            }
            let start = if c == CPL { self.cpl_start } else { 0 };
            let end = a.end;
            if c == CPL && (end != self.cpl_end || start >= end) {
                return Err(Error::invalid(
                    "coupling exponents do not match coupling range",
                ));
            }
            let snroffset = (((i32::from(self.csnroffst) - 15) << 4) + i32::from(a.fsnroffst)) << 2;
            let leak = if c == CPL {
                Some((
                    (i32::from(self.cplfleak) << 8) + 768,
                    (i32::from(self.cplsleak) << 8) + 768,
                ))
            } else {
                None
            };
            let dba = a.dba;
            let fgain = FASTGAIN[usize::from(a.fgaincod)];
            let key = BaKey {
                codes: [
                    self.sdcycod,
                    self.fdcycod,
                    self.sgaincod,
                    self.dbpbcod,
                    self.floorcod,
                ],
                hth: self.hth.as_ptr() as usize,
                start,
                end,
                snroffset,
                fgain,
                leak,
                dba,
                hebap: self.aht[c],
            };
            if !a.exp_new && a.ba_key == Some(key) {
                continue;
            }
            let cba = ChannelBa {
                start,
                end,
                fgain,
                snroffset,
                leak,
                dba: &dba,
                hebap: self.aht[c],
            };
            allocate(&a.exp, &params, &cba, &mut a.bap);
            a.exp_new = false;
            a.ba_key = Some(key);
        }
        Ok(())
    }

    fn read_mantissas(&mut self, r: &mut BitReader, blk: usize, opt: &Options) -> Result<()> {
        let nf = self.nfchans;
        let mut groups = Groups::default();
        let mut got_cpl = false;
        for ch in 0..nf {
            let end = self.alloc[ch].end;
            if self.aht[ch] {
                if !self.aht_read[ch] {
                    if blk != 0 {
                        return Err(Error::invalid("AHT mantissas not in block 0"));
                    }
                    self.read_aht(r, ch, 0, end)?;
                }
                self.aht_block(ch, blk, 0, end, self.dithflag[ch] && opt.dither);
            } else {
                let a = &self.alloc[ch];
                let coef = &mut self.coef[ch];
                let dith = self.dithflag[ch] && opt.dither;
                for bin in 0..end {
                    let b = a.bap[bin];
                    let m = if b == 0 {
                        if dith {
                            self.rng.uniform() * 0.707_106_77
                        } else {
                            0.0
                        }
                    } else {
                        groups.mantissa(r, b)?
                    };
                    coef[bin] = m * EXP_SCALE[usize::from(a.exp[bin])];
                }
                coef[end..].fill(0.0);
            }
            if self.cplinu && self.chincpl[ch] && !got_cpl {
                got_cpl = true;
                let (s, e) = (self.cpl_start, self.cpl_end);
                if self.aht[CPL] {
                    if !self.aht_read[CPL] {
                        if blk != 0 {
                            return Err(Error::invalid("AHT mantissas not in block 0"));
                        }
                        self.read_aht(r, CPL, s, e)?;
                    }
                    self.aht_block(CPL, blk, s, e, false);
                } else {
                    let a = &self.alloc[CPL];
                    for bin in s..e {
                        let b = a.bap[bin];
                        // zero-bit coupling mantissas are dithered per channel when decoupling
                        let m = if b == 0 { 0.0 } else { groups.mantissa(r, b)? };
                        self.coef[CPL][bin] = m * EXP_SCALE[usize::from(a.exp[bin])];
                    }
                }
            }
        }
        if self.lfeon {
            if self.aht[LFE] {
                if !self.aht_read[LFE] {
                    if blk != 0 {
                        return Err(Error::invalid("AHT mantissas not in block 0"));
                    }
                    self.read_aht(r, LFE, 0, 7)?;
                }
                self.aht_block(LFE, blk, 0, 7, false);
            } else {
                let a = &self.alloc[LFE];
                for bin in 0..7 {
                    let b = a.bap[bin];
                    let m = if b == 0 { 0.0 } else { groups.mantissa(r, b)? };
                    self.coef[LFE][bin] = m * EXP_SCALE[usize::from(a.exp[bin])];
                }
            }
            self.coef[LFE][7..].fill(0.0);
        }
        Ok(())
    }

    /// Read the AHT mantissas of all six blocks of channel `c` (E.1.2.4, E.2.4.4) and invert
    /// the DCT (E.2.4.5).
    #[allow(clippy::needless_range_loop)]
    fn read_aht(&mut self, r: &mut BitReader, c: usize, start: usize, end: usize) -> Result<()> {
        if self.aht_mant.len() < 7 {
            self.aht_mant = vec![[[0.0; 6]; 256]; 7];
        }
        self.aht_read[c] = true;
        let gaqmod = r.read(2)? as u8;
        self.gaqmod[c] = gaqmod;
        let hebap = &self.alloc[c].bap;
        let endbap = if gaqmod < 2 { 12 } else { 17 };
        let active = (start..end)
            .filter(|&b| hebap[b] > 7 && hebap[b] < endbap)
            .count();
        // gains, one per active bin
        let mut gains = [1u8; 256];
        match gaqmod {
            1 | 2 => {
                let g = if gaqmod == 1 { 2 } else { 4 };
                for v in gains.iter_mut().take(active) {
                    if r.read_bit()? {
                        *v = g;
                    }
                }
            }
            3 => {
                let sections = active.div_ceil(3);
                for s in 0..sections {
                    let grp = r.read(5)?;
                    if grp >= 27 {
                        return Err(Error::invalid("invalid GAQ gain group"));
                    }
                    for (i, m) in [grp / 9, (grp % 9) / 3, grp % 3].into_iter().enumerate() {
                        if s * 3 + i < 256 {
                            gains[s * 3 + i] = [1, 2, 4][m as usize];
                        }
                    }
                }
            }
            _ => {}
        }
        // inverse DCT basis (E.2.4.5): C(m) = sqrt(2) * sum_j R_j X_j cos(j (2m+1) pi / 12),
        // R_0 = 1/sqrt(2), R_j = 1 otherwise
        let mut basis = [[0f32; 6]; 6];
        for (m, row) in basis.iter_mut().enumerate() {
            for (j, b) in row.iter_mut().enumerate() {
                let w = if j == 0 {
                    1.0
                } else {
                    std::f64::consts::SQRT_2
                };
                *b = (w * ((j * (2 * m + 1)) as f64 * std::f64::consts::PI / 12.0).cos()) as f32;
            }
        }
        let mut gi = 0;
        let out = &mut self.aht_mant[c];
        for bin in start..end {
            let h = hebap[bin];
            let mut x = [0f32; 6];
            if h >= 8 {
                let gain = if h < endbap && gaqmod != 0 {
                    let g = gains[gi];
                    gi += 1;
                    g
                } else {
                    if h < endbap {
                        gi += 1;
                    }
                    1
                };
                for v in x.iter_mut() {
                    *v = gaq_mantissa(r, h, gain)?;
                }
            } else if h > 0 {
                let idx = r.read(VQ_BITS[usize::from(h)])? as usize;
                let vec: &[i16; 6] = match h {
                    1 => &VQ_HEBAP1[idx],
                    2 => &VQ_HEBAP2[idx],
                    3 => &VQ_HEBAP3[idx],
                    4 => &VQ_HEBAP4[idx],
                    5 => &VQ_HEBAP5[idx],
                    6 => &VQ_HEBAP6[idx],
                    _ => &VQ_HEBAP7[idx],
                };
                for (v, q) in x.iter_mut().zip(vec) {
                    *v = f32::from(*q) / 32768.0;
                }
            }
            for (o, row) in out[bin].iter_mut().zip(&basis) {
                *o = row.iter().zip(&x).map(|(b, v)| b * v).sum();
            }
        }
        Ok(())
    }

    /// Coefficients of block `blk` from the AHT mantissas.
    fn aht_block(&mut self, c: usize, blk: usize, start: usize, end: usize, dither: bool) {
        let a = &self.alloc[c];
        let m = &self.aht_mant[c];
        for bin in start..end {
            let v = if a.bap[bin] == 0 && dither {
                self.rng.uniform() * 0.707_106_77
            } else {
                m[bin][blk]
            };
            self.coef[c][bin] = v * EXP_SCALE[usize::from(a.exp[bin])];
        }
        if c != CPL {
            self.coef[c][end..].fill(0.0);
        }
    }

    fn decouple(&mut self, opt: &Options) {
        let (s, e) = (self.cpl_start, self.cpl_end);
        for ch in 0..self.nfchans {
            if !self.chincpl[ch] {
                continue;
            }
            let dith = self.dithflag[ch] && opt.dither && !self.aht[CPL];
            for bin in s..e {
                let sb = (bin - 37) / 12;
                let mut co = self.cplco[ch][sb] * 8.0;
                if self.acmod == 2 && ch == 1 && self.phsflginu && self.phsflg[sb] {
                    co = -co;
                }
                let a = &self.alloc[CPL];
                let v = if a.bap[bin] == 0 && !self.aht[CPL] {
                    if dith {
                        self.rng.uniform() * 0.707_106_77 * EXP_SCALE[usize::from(a.exp[bin])]
                    } else {
                        0.0
                    }
                } else {
                    self.coef[CPL][bin]
                };
                self.coef[ch][bin] = v * co;
            }
            self.coef[ch][e..].fill(0.0);
        }
    }

    fn rematrix(&mut self) {
        let end = self.alloc[0].end.min(self.alloc[1].end);
        for b in 0..self.nrematbd {
            if !self.rematflg[b] {
                continue;
            }
            let lo = REMAT_BANDS[b];
            let hi = REMAT_BANDS[b + 1].min(end);
            for bin in lo..hi {
                let l = self.coef[0][bin];
                let r = self.coef[1][bin];
                self.coef[0][bin] = l + r;
                self.coef[1][bin] = l - r;
            }
        }
    }

    /// High frequency synthesis by spectral extension (E.2.6.4).
    fn spectral_extension(&mut self) {
        let (nbnds, sizes) = self.spx_bands();
        let copystart = spx_band_start(self.spxstrtf);
        let copyend = spx_band_start(self.spx_begin);
        for ch in 0..self.nfchans {
            if !self.chinspx[ch] {
                continue;
            }
            let tc = &mut self.coef[ch];
            // translation
            let mut wrap = [false; 17];
            let mut copy = copystart;
            let mut insert = copyend;
            for bnd in 0..nbnds {
                let size = sizes[bnd];
                if copy + size > copyend {
                    copy = copystart;
                    wrap[bnd] = true;
                }
                for _ in 0..size {
                    if copy == copyend {
                        copy = copystart;
                    }
                    tc[insert] = tc[copy];
                    insert += 1;
                    copy += 1;
                }
            }
            // banded RMS energy of the translated coefficients
            let mut rms = [0f32; 17];
            let mut pos = copyend;
            for bnd in 0..nbnds {
                let size = sizes[bnd];
                let acc: f32 = tc[pos..pos + size].iter().map(|v| v * v).sum();
                rms[bnd] = (acc / size as f32).sqrt();
                pos += size;
            }
            // notch filter at the baseband border and at every wrap point
            if self.chinspxatten[ch] {
                let code = usize::from(self.spxattencod[ch]);
                let att = |i: usize| pow2f(-(((i + 1) * (code + 1)) as f32) / 15.0);
                let notch = |tc: &mut [f32; 256], at: usize| {
                    if at >= 2 && at + 2 < 256 {
                        for (k, i) in [0usize, 1, 2, 1, 0].into_iter().enumerate() {
                            tc[at - 2 + k] *= att(i);
                        }
                    }
                };
                notch(tc, copyend);
                let mut border = copyend + sizes[0];
                for bnd in 1..nbnds {
                    if wrap[bnd] {
                        notch(tc, border);
                    }
                    border += sizes[bnd];
                }
            }
            // noise blending and scaling by the coordinates
            let mut pos = copyend;
            for bnd in 0..nbnds {
                let size = sizes[bnd];
                let nscale = rms[bnd] * self.spx_nblend[ch][bnd];
                let sscale = self.spx_sblend[ch][bnd];
                let co = self.spxco[ch][bnd] * 32.0;
                for v in tc[pos..pos + size].iter_mut() {
                    // unit variance noise: uniform in [-sqrt(3), sqrt(3))
                    let noise = self.rng.uniform() * 1.732_050_8;
                    *v = (*v * sscale + noise * nscale) * co;
                }
                pos += size;
            }
            tc[pos..].fill(0.0);
        }
    }
}

fn apply_dba_mode(dba: &mut Dba, mode: u32, r: &mut BitReader) -> Result<()> {
    match mode {
        0 => Ok(()),
        1 => dba.read(r),
        2 => {
            dba.active = false;
            Ok(())
        }
        _ => Err(Error::invalid("reserved delta bit allocation mode")),
    }
}

/// One GAQ / scalar mantissa of an AHT channel (E.2.4.4.2).
fn gaq_mantissa(r: &mut BitReader, hebap: u8, gain: u8) -> Result<f32> {
    let h = usize::from(hebap - 8);
    let m = GAQ_BITS[h];
    let frac = |v: i32, bits: u32| v as f32 / (1u32 << (bits - 1)) as f32;
    let k = |v: i16| f32::from(v) / 32768.0;
    if gain == 1 {
        let x = frac(r.read_signed(m)?, m);
        let (a, _, _) = GAQ_REMAP[h][0];
        return Ok(x + k(a) * x);
    }
    let (small, large, gi) = if gain == 2 {
        (m - 1, m - 1, 1)
    } else {
        (m - 2, m, 2)
    };
    let s = r.read_signed(small)?;
    if s == -(1 << (small - 1)) {
        let x = frac(r.read_signed(large)?, large);
        let (a, bp, bn) = GAQ_REMAP[h][gi];
        Ok(x + k(a) * x + if x >= 0.0 { k(bp) } else { k(bn) })
    } else {
        Ok(frac(s, small) / f32::from(gain))
    }
}

/// Dynamic range gain of a `dynrng` word (clause 6.7.2.2), raised to the user scale.
fn drc_gain(v: u32, scale: f32) -> f32 {
    if scale == 0.0 {
        return 1.0;
    }
    let x = ((v as i32) << 24 >> 29) + 1; // signed 3-bit field + 1
    let y = (v & 0x1f) as f32;
    let g = pow2(x) * (32.0 + y) / 64.0;
    g.powf(scale)
}

#[inline]
fn pow2(e: i32) -> f32 {
    pow2f(e as f32)
}

#[inline]
fn pow2f(e: f32) -> f32 {
    e.exp2()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drc_words() {
        assert_eq!(drc_gain(0, 1.0), 1.0);
        assert!((drc_gain(0x80, 0.0) - 1.0).abs() < 1e-6);
        // 0b111_00000: X = -1 -> 0 dB, Y = 0 -> 1/2: -6 dB
        assert!((drc_gain(0xE0, 1.0) - 0.5).abs() < 1e-6);
        // 0b011_11111: +24 dB * 63/64
        assert!((drc_gain(0x7F, 1.0) - 16.0 * 63.0 / 64.0).abs() < 1e-4);
    }

    #[test]
    fn gaq_remap_gain1_is_symmetric() {
        // hebap 8 (m = 3), gain 1: codes -3..3 map to 2k/7
        for code in -3i32..=3 {
            let byte = ((code as u8) & 7) << 5;
            let data = [byte, 0];
            let mut r = BitReader::new(&data);
            let v = gaq_mantissa(&mut r, 8, 1).unwrap();
            assert!((v - 2.0 * code as f32 / 7.0).abs() < 1e-3, "{code}: {v}");
        }
    }
}
