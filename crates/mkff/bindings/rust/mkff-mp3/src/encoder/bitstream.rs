//! Layer III frame formatting: side information, main data (scale factors and Huffman codes)
//! and the bit reservoir (main data placed in the unused bytes of earlier frames).

use super::quant::Quantized;
use crate::huffman::{quad_code, tables, TABLE_INFO};
use crate::tables::{SfBands, NR_OF_SFB};
use mkff_audio_core::bits::BitWriter;
use std::collections::VecDeque;

/// Write the side information.
pub(crate) fn write_side_info(
    w: &mut BitWriter,
    lsf: bool,
    nch: usize,
    main_data_begin: usize,
    scfsi: &[[bool; 4]; 2],
    gr: &[Vec<Quantized>],
) {
    if lsf {
        w.write(main_data_begin as u32, 8);
        w.write(0, if nch == 1 { 1 } else { 2 });
    } else {
        w.write(main_data_begin as u32, 9);
        w.write(0, if nch == 1 { 5 } else { 3 });
        for s in scfsi.iter().take(nch) {
            for &b in s {
                w.write_bit(b);
            }
        }
    }
    for g in gr {
        for q in g.iter().take(nch) {
            let i = &q.info;
            w.write(i.part2_3_length, 12);
            w.write(i.big_values, 9);
            w.write(i.global_gain, 8);
            w.write(i.scalefac_compress, if lsf { 9 } else { 4 });
            if i.block_type != 0 {
                w.write_bit(true);
                w.write(u32::from(i.block_type), 2);
                w.write_bit(i.mixed);
                for t in 0..2 {
                    w.write(u32::from(i.table_select[t]), 5);
                }
                for s in 0..3 {
                    w.write(u32::from(i.subblock_gain[s]), 3);
                }
            } else {
                w.write_bit(false);
                for t in 0..3 {
                    w.write(u32::from(i.table_select[t]), 5);
                }
                w.write(i.region0_count, 4);
                w.write(i.region1_count, 3);
            }
            if !lsf {
                w.write_bit(i.preflag);
            }
            w.write_bit(i.scalefac_scale);
            w.write_bit(i.count1_table_b);
        }
    }
}

/// Write the scale factors (part 2) of one granule / channel.
pub(crate) fn write_scalefactors(
    w: &mut BitWriter,
    q: &Quantized,
    lsf: bool,
    gr: usize,
    scfsi: &[bool; 4],
) {
    let short = q.info.block_type == 2;
    if lsf {
        let nr = NR_OF_SFB[0][usize::from(short)];
        let mut idx = 0usize;
        for k in 0..4 {
            let n = u32::from(q.slen[k]);
            for _ in 0..nr[k] {
                let v = if short {
                    q.sf_s[idx / 3][idx % 3]
                } else {
                    q.sf_l[idx]
                };
                w.write(u32::from(v), n);
                idx += 1;
            }
        }
        return;
    }
    let (s1, s2) = (u32::from(q.slen[0]), u32::from(q.slen[1]));
    if short {
        for sfb in 0..12 {
            let n = if sfb < 6 { s1 } else { s2 };
            for win in 0..3 {
                w.write(u32::from(q.sf_s[sfb][win]), n);
            }
        }
    } else {
        const GROUPS: [(usize, usize); 4] = [(0, 6), (6, 11), (11, 16), (16, 21)];
        for (k, &(a, b)) in GROUPS.iter().enumerate() {
            if gr == 1 && scfsi[k] {
                continue;
            }
            let n = if k < 2 { s1 } else { s2 };
            for sfb in a..b {
                w.write(u32::from(q.sf_l[sfb]), n);
            }
        }
    }
}

/// Write the Huffman coded values (part 3).
pub(crate) fn write_huffman(w: &mut BitWriter, q: &Quantized, bands: &SfBands) {
    let t = tables();
    let ix = &q.ix;
    let bv_end = q.info.big_values as usize * 2;
    let (r1, r2) = crate::layer3::region_bounds(&q.info, bands);
    let r1 = r1.min(bv_end);
    let r2 = r2.min(bv_end).max(r1);
    let mut i = 0;
    for (end, &ts) in [r1, r2, bv_end].iter().zip(&q.info.table_select) {
        let (tidx, lin) = TABLE_INFO[ts as usize];
        if tidx == 0 {
            i = (*end).max(i);
            continue;
        }
        let base = tidx as usize - 1;
        let lin = u32::from(lin);
        while i < *end {
            let (x, y) = (ix[i], ix[i + 1]);
            let (ax, ay) = (x.unsigned_abs(), y.unsigned_abs());
            let k = ax.min(15) as usize * 16 + ay.min(15) as usize;
            w.write(u32::from(t.code16[base][k]), u32::from(t.len16[base][k]));
            if ax != 0 {
                if lin > 0 && ax >= 15 {
                    w.write(ax - 15, lin);
                }
                w.write_bit(x < 0);
            }
            if ay != 0 {
                if lin > 0 && ay >= 15 {
                    w.write(ay - 15, lin);
                }
                w.write_bit(y < 0);
            }
            i += 2;
        }
    }
    let mut i = bv_end;
    while i < q.count1_end {
        let v = &ix[i..i + 4];
        let idx = v.iter().fold(0usize, |a, &x| a * 2 + usize::from(x != 0));
        let (code, len) = quad_code(q.info.count1_table_b, idx);
        w.write(code, len);
        for &x in v {
            if x != 0 {
                w.write_bit(x < 0);
            }
        }
        i += 4;
    }
}

struct Pending {
    data: Vec<u8>,
    /// Offset of the main data slots in `data`.
    slot_start: usize,
    /// Slot bytes filled so far.
    filled: usize,
    index: u64,
}

impl Pending {
    fn free(&self) -> usize {
        self.data.len() - self.slot_start - self.filled
    }
}

/// Places main data into frame slots, starting in the free bytes of earlier frames.
#[derive(Default)]
pub(crate) struct Reservoir {
    pending: VecDeque<Pending>,
    next_index: u64,
}

impl Reservoir {
    /// Free bytes before the next frame (the next frame's `main_data_begin`).
    pub(crate) fn available(&self) -> usize {
        self.pending.iter().map(Pending::free).sum()
    }

    /// Zero-fill free bytes so that at most `max` remain available.
    pub(crate) fn limit(&mut self, max: usize, out: &mut Vec<(u64, Vec<u8>)>) {
        let mut excess = self.available().saturating_sub(max);
        for p in &mut self.pending {
            if excess == 0 {
                break;
            }
            let n = p.free().min(excess);
            p.filled += n;
            excess -= n;
        }
        self.emit_full(out);
    }

    /// Append a frame (`head` = header + side information, `frame_len` total bytes) whose main
    /// data `main` starts `available()` bytes back. `main.len()` must not exceed
    /// `available() + frame_len - head.len()`.
    pub(crate) fn push(
        &mut self,
        head: &[u8],
        frame_len: usize,
        main: &[u8],
        out: &mut Vec<(u64, Vec<u8>)>,
    ) {
        let mut data = vec![0u8; frame_len];
        data[..head.len()].copy_from_slice(head);
        self.pending.push_back(Pending {
            data,
            slot_start: head.len(),
            filled: 0,
            index: self.next_index,
        });
        self.next_index += 1;
        let mut src = main;
        for p in &mut self.pending {
            if src.is_empty() {
                break;
            }
            let n = p.free().min(src.len());
            let at = p.slot_start + p.filled;
            p.data[at..at + n].copy_from_slice(&src[..n]);
            p.filled += n;
            src = &src[n..];
        }
        debug_assert!(src.is_empty(), "main data overflowed the reservoir");
        self.emit_full(out);
    }

    fn emit_full(&mut self, out: &mut Vec<(u64, Vec<u8>)>) {
        while let Some(p) = self.pending.front() {
            if p.free() > 0 {
                break;
            }
            let p = self.pending.pop_front().expect("front");
            out.push((p.index, p.data));
        }
    }

    /// Emit everything (remaining free bytes stay zero).
    pub(crate) fn flush(&mut self, out: &mut Vec<(u64, Vec<u8>)>) {
        while let Some(p) = self.pending.pop_front() {
            out.push((p.index, p.data));
        }
    }
}
