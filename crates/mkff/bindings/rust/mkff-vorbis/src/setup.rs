//! The setup header (spec §4.2.4): codebooks, floors, residues, mappings and modes.

use crate::bitread::{ilog, BitReader};
use crate::codebook::Codebook;
use crate::header::{is_header, IdentHeader, PACKET_SETUP};
use mkff_audio_core::{Error, Result};

/// Total codebook entries + VQ values one setup header may allocate.
const SETUP_BUDGET: usize = 1 << 22;

#[derive(Clone, Debug)]
pub(crate) struct Floor0 {
    pub order: usize,
    pub bark_map_size: u32,
    pub amplitude_bits: u32,
    pub amplitude_offset: u32,
    pub books: Vec<u8>,
    /// Bark map for the short and the long block (`n/2` entries each).
    pub map: [Vec<u16>; 2],
}

#[derive(Clone, Debug)]
pub(crate) struct Floor1 {
    pub partition_class: Vec<u8>,
    pub class_dimensions: [u8; 16],
    pub class_subclasses: [u8; 16],
    pub class_masterbook: [u8; 16],
    /// Subclass books per class; -1 = none.
    pub subclass_books: [[i16; 8]; 16],
    pub multiplier: u32,
    /// X positions in header order (`[0]` = 0, `[1]` = `1 << rangebits`).
    pub xs: Vec<u16>,
    /// Indices of `xs` in ascending X order.
    pub sorted: Vec<u8>,
    /// Low / high neighbours (spec §9.2.4 / §9.2.5) of each index ≥ 2.
    pub low: Vec<u8>,
    pub high: Vec<u8>,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // a handful per stream
pub(crate) enum Floor {
    Zero(Floor0),
    One(Floor1),
}

#[derive(Clone, Debug)]
pub(crate) struct Residue {
    pub kind: u16,
    pub begin: usize,
    pub end: usize,
    pub partition_size: usize,
    pub classifications: usize,
    pub classbook: u8,
    /// Per classification, the book of each of the eight passes; -1 = none.
    pub books: Vec<[i16; 8]>,
    /// Whether any classification has a book in each pass.
    pub pass_used: [bool; 8],
}

#[derive(Clone, Debug)]
pub(crate) struct Mapping {
    /// (magnitude, angle) channel pairs.
    pub coupling: Vec<(u8, u8)>,
    /// Submap of each channel.
    pub mux: Vec<u8>,
    /// (floor, residue) of each submap.
    pub submaps: Vec<(u8, u8)>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Mode {
    pub blockflag: bool,
    pub mapping: u8,
}

#[derive(Clone, Debug)]
pub(crate) struct Setup {
    pub codebooks: Vec<Codebook>,
    pub floors: Vec<Floor>,
    pub residues: Vec<Residue>,
    pub mappings: Vec<Mapping>,
    pub modes: Vec<Mode>,
    pub mode_bits: u32,
}

fn invalid(msg: &str) -> Error {
    Error::invalid(format!("vorbis: {msg}"))
}

fn bark(x: f64) -> f64 {
    13.1 * (0.00074 * x).atan() + 2.24 * (0.000_000_018_5 * x * x).atan() + 0.0001 * x
}

impl Setup {
    pub fn parse(packet: &[u8], ident: &IdentHeader) -> Result<Self> {
        if !is_header(packet, PACKET_SETUP) {
            return Err(invalid("not a setup header"));
        }
        let mut r = BitReader::new(&packet[7..]);
        Self::parse_body(&mut r, ident).map_err(|e| match e {
            Error::EndOfData => invalid("setup header truncated"),
            e => e,
        })
    }

    fn parse_body(r: &mut BitReader, ident: &IdentHeader) -> Result<Self> {
        let channels = usize::from(ident.channels);
        let mut budget = SETUP_BUDGET;

        let count = r.read(8)? as usize + 1;
        let mut codebooks = Vec::with_capacity(count);
        for _ in 0..count {
            codebooks.push(Codebook::parse(r, &mut budget)?);
        }
        let book_count = codebooks.len();
        let check_book = |b: u32| -> Result<u8> {
            if (b as usize) < book_count {
                Ok(b as u8)
            } else {
                Err(invalid("codebook number out of range"))
            }
        };
        let check_vq_book = |b: u32| -> Result<u8> {
            let b = check_book(b)?;
            if codebooks[usize::from(b)].vq.is_none() {
                return Err(invalid("VQ use of a codebook without a lookup table"));
            }
            Ok(b)
        };

        // time domain transforms: placeholders, must be zero
        let times = r.read(6)? + 1;
        for _ in 0..times {
            if r.read(16)? != 0 {
                return Err(invalid("nonzero time domain transform type"));
            }
        }

        let count = r.read(6)? as usize + 1;
        let mut floors = Vec::with_capacity(count);
        for _ in 0..count {
            let floor = match r.read(16)? {
                0 => {
                    let order = r.read(8)? as usize;
                    let rate = r.read(16)?;
                    let bark_map_size = r.read(16)?;
                    let amplitude_bits = r.read(6)?;
                    let amplitude_offset = r.read(8)?;
                    let nbooks = r.read(4)? as usize + 1;
                    let mut books = Vec::with_capacity(nbooks);
                    for _ in 0..nbooks {
                        books.push(check_vq_book(r.read(8)?)?);
                    }
                    if order == 0 || rate == 0 || bark_map_size == 0 {
                        return Err(invalid("invalid floor 0 parameters"));
                    }
                    let make_map = |n: usize| -> Vec<u16> {
                        let scale = f64::from(bark_map_size) / bark(0.5 * f64::from(rate));
                        (0..n)
                            .map(|i| {
                                let f = bark(f64::from(rate) * i as f64 / (2.0 * n as f64)) * scale;
                                (f.floor().max(0.0) as u32).min(bark_map_size - 1) as u16
                            })
                            .collect()
                    };
                    Floor::Zero(Floor0 {
                        order,
                        bark_map_size,
                        amplitude_bits,
                        amplitude_offset,
                        books,
                        map: [
                            make_map(ident.blocksize_0 / 2),
                            make_map(ident.blocksize_1 / 2),
                        ],
                    })
                }
                1 => Floor::One(Self::parse_floor1(r, &check_book)?),
                _ => return Err(invalid("unknown floor type")),
            };
            floors.push(floor);
        }

        let count = r.read(6)? as usize + 1;
        let mut residues = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = r.read(16)? as u16;
            if kind > 2 {
                return Err(invalid("unknown residue type"));
            }
            let begin = r.read(24)? as usize;
            let end = r.read(24)? as usize;
            let partition_size = r.read(24)? as usize + 1;
            let classifications = r.read(6)? as usize + 1;
            let classbook = check_book(r.read(8)?)?;
            if codebooks[usize::from(classbook)].dimensions == 0 {
                return Err(invalid("residue classbook with zero dimensions"));
            }
            let mut cascade = Vec::with_capacity(classifications);
            for _ in 0..classifications {
                let low = r.read(3)?;
                let high = if r.read_bit()? { r.read(5)? } else { 0 };
                cascade.push((high << 3) | low);
            }
            let mut books = Vec::with_capacity(classifications);
            let mut pass_used = [false; 8];
            for c in cascade {
                let mut b = [-1i16; 8];
                for (j, slot) in b.iter_mut().enumerate() {
                    if c & (1 << j) != 0 {
                        *slot = i16::from(check_vq_book(r.read(8)?)?);
                        pass_used[j] = true;
                    }
                }
                books.push(b);
            }
            residues.push(Residue {
                kind,
                begin,
                end,
                partition_size,
                classifications,
                classbook,
                books,
                pass_used,
            });
        }

        let count = r.read(6)? as usize + 1;
        let mut mappings = Vec::with_capacity(count);
        for _ in 0..count {
            if r.read(16)? != 0 {
                return Err(invalid("unknown mapping type"));
            }
            let submap_count = if r.read_bit()? {
                r.read(4)? as usize + 1
            } else {
                1
            };
            let mut coupling = Vec::new();
            if r.read_bit()? {
                let steps = r.read(8)? as usize + 1;
                let bits = ilog(channels as u32 - 1);
                for _ in 0..steps {
                    let m = r.read(bits)? as usize;
                    let a = r.read(bits)? as usize;
                    if m == a || m >= channels || a >= channels {
                        return Err(invalid("invalid channel coupling"));
                    }
                    coupling.push((m as u8, a as u8));
                }
            }
            if r.read(2)? != 0 {
                return Err(invalid("mapping reserved field nonzero"));
            }
            let mut mux = vec![0u8; channels];
            if submap_count > 1 {
                for m in &mut mux {
                    let v = r.read(4)? as usize;
                    if v >= submap_count {
                        return Err(invalid("mapping mux out of range"));
                    }
                    *m = v as u8;
                }
            }
            let mut submaps = Vec::with_capacity(submap_count);
            for _ in 0..submap_count {
                r.read(8)?; // unused time configuration
                let floor = r.read(8)? as usize;
                let residue = r.read(8)? as usize;
                if floor >= floors.len() || residue >= residues.len() {
                    return Err(invalid("mapping floor / residue out of range"));
                }
                submaps.push((floor as u8, residue as u8));
            }
            mappings.push(Mapping {
                coupling,
                mux,
                submaps,
            });
        }

        let count = r.read(6)? as usize + 1;
        let mut modes = Vec::with_capacity(count);
        for _ in 0..count {
            let blockflag = r.read_bit()?;
            let window = r.read(16)?;
            let transform = r.read(16)?;
            let mapping = r.read(8)? as usize;
            if window != 0 || transform != 0 {
                return Err(invalid("unknown window or transform type"));
            }
            if mapping >= mappings.len() {
                return Err(invalid("mode mapping out of range"));
            }
            modes.push(Mode {
                blockflag,
                mapping: mapping as u8,
            });
        }
        if !r.read_bit()? {
            return Err(invalid("setup header framing bit unset"));
        }
        let mode_bits = ilog(modes.len() as u32 - 1);
        Ok(Self {
            codebooks,
            floors,
            residues,
            mappings,
            modes,
            mode_bits,
        })
    }

    fn parse_floor1(r: &mut BitReader, check_book: &dyn Fn(u32) -> Result<u8>) -> Result<Floor1> {
        let partitions = r.read(5)? as usize;
        let mut partition_class = Vec::with_capacity(partitions);
        let mut max_class: i32 = -1;
        for _ in 0..partitions {
            let c = r.read(4)? as u8;
            max_class = max_class.max(i32::from(c));
            partition_class.push(c);
        }
        let mut class_dimensions = [0u8; 16];
        let mut class_subclasses = [0u8; 16];
        let mut class_masterbook = [0u8; 16];
        let mut subclass_books = [[-1i16; 8]; 16];
        for c in 0..=max_class {
            let c = c as usize;
            class_dimensions[c] = r.read(3)? as u8 + 1;
            class_subclasses[c] = r.read(2)? as u8;
            if class_subclasses[c] != 0 {
                class_masterbook[c] = check_book(r.read(8)?)?;
            }
            for slot in subclass_books[c].iter_mut().take(1 << class_subclasses[c]) {
                let b = r.read(8)?;
                *slot = if b == 0 {
                    -1
                } else {
                    i16::from(check_book(b - 1)?)
                };
            }
        }
        let multiplier = r.read(2)? + 1;
        let rangebits = r.read(4)?;
        let mut xs: Vec<u16> = vec![0, 1 << rangebits];
        for &c in &partition_class {
            for _ in 0..class_dimensions[usize::from(c)] {
                xs.push(r.read(rangebits)? as u16);
            }
        }
        let mut sorted: Vec<u8> = (0..xs.len() as u8).collect();
        sorted.sort_by_key(|&i| xs[usize::from(i)]);
        if sorted
            .windows(2)
            .any(|w| xs[usize::from(w[0])] == xs[usize::from(w[1])])
        {
            return Err(invalid("floor 1 X list has duplicates"));
        }
        let mut low = vec![0u8; xs.len()];
        let mut high = vec![0u8; xs.len()];
        for i in 2..xs.len() {
            let x = xs[i];
            let (mut lo, mut hi) = (0usize, 1usize);
            for (j, &xj) in xs.iter().enumerate().take(i) {
                if xj < x && xj > xs[lo] {
                    lo = j;
                }
                if xj > x && xj < xs[hi] {
                    hi = j;
                }
            }
            low[i] = lo as u8;
            high[i] = hi as u8;
        }
        Ok(Floor1 {
            partition_class,
            class_dimensions,
            class_subclasses,
            class_masterbook,
            subclass_books,
            multiplier,
            xs,
            sorted,
            low,
            high,
        })
    }
}
