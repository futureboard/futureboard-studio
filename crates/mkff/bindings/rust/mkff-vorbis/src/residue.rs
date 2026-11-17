//! Residue decode (spec §8).

use crate::bitread::BitReader;
use crate::codebook::Codebook;
use crate::setup::Residue;
use mkff_audio_core::Result;

/// Decode residue vectors with the format of `res.kind` (0 = interleaved partitions, 1 or 2 =
/// concatenated). For residue type 2 the caller passes the single interleaved vector.
///
/// `vecs` must be zeroed by the caller; decoded values are added. An end of packet (or an invalid
/// codeword) stops the decode and leaves the rest of the vectors as they are, which the
/// specification treats as a normal occurrence.
pub(crate) fn decode(
    res: &Residue,
    books: &[Codebook],
    r: &mut BitReader,
    vecs: &mut [&mut [f32]],
    skip: &[bool],
    classes: &mut Vec<u8>,
) {
    let _ = decode_inner(res, books, r, vecs, skip, classes);
}

fn decode_inner(
    res: &Residue,
    books: &[Codebook],
    r: &mut BitReader,
    vecs: &mut [&mut [f32]],
    skip: &[bool],
    classes: &mut Vec<u8>,
) -> Result<()> {
    let Some(size) = vecs.first().map(|v| v.len()) else {
        return Ok(());
    };
    let begin = res.begin.min(size);
    let end = res.end.min(size);
    if end <= begin {
        return Ok(());
    }
    let psize = res.partition_size;
    let parts = (end - begin) / psize;
    if parts == 0 {
        return Ok(());
    }
    let classbook = &books[usize::from(res.classbook)];
    let cwpc = classbook.dimensions;
    let nclass = res.classifications as u32;
    let stride = parts + cwpc;
    classes.clear();
    classes.resize(stride * vecs.len(), 0);

    for pass in 0..8 {
        if pass > 0 && !res.pass_used[pass] {
            continue;
        }
        let mut pc = 0;
        while pc < parts {
            if pass == 0 {
                for (j, _) in vecs.iter().enumerate() {
                    if skip[j] {
                        continue;
                    }
                    let mut temp = classbook.decode_scalar(r)?;
                    let row = &mut classes[j * stride + pc..j * stride + pc + cwpc];
                    for c in row.iter_mut().rev() {
                        *c = (temp % nclass) as u8;
                        temp /= nclass;
                    }
                }
            }
            for _ in 0..cwpc {
                if pc >= parts {
                    break;
                }
                for (j, v) in vecs.iter_mut().enumerate() {
                    if skip[j] {
                        continue;
                    }
                    let class = usize::from(classes[j * stride + pc]);
                    let book = res.books[class][pass];
                    if book < 0 {
                        continue;
                    }
                    let book = &books[book as usize];
                    let off = begin + pc * psize;
                    if res.kind == 0 {
                        partition_format0(book, r, v, off, psize)?;
                    } else {
                        partition_format1(book, r, v, off, psize)?;
                    }
                }
                pc += 1;
            }
        }
    }
    Ok(())
}

/// Format 0: each VQ vector is spread across the partition with stride `n / dimensions`.
fn partition_format0(
    book: &Codebook,
    r: &mut BitReader,
    v: &mut [f32],
    off: usize,
    n: usize,
) -> Result<()> {
    let dims = book.dimensions;
    let step = n / dims;
    for i in 0..step {
        let e = book.decode_vq(r)?;
        for (j, &x) in e.iter().enumerate() {
            if let Some(s) = v.get_mut(off + i + j * step) {
                *s += x;
            }
        }
    }
    Ok(())
}

/// Format 1: VQ vectors are concatenated through the partition.
#[inline]
fn partition_format1(
    book: &Codebook,
    r: &mut BitReader,
    v: &mut [f32],
    off: usize,
    n: usize,
) -> Result<()> {
    let dims = book.dimensions;
    let mut i = 0;
    while i < n {
        let e = book.decode_vq(r)?;
        let start = off + i;
        if let Some(dst) = v.get_mut(start..start + dims) {
            for (d, &x) in dst.iter_mut().zip(e) {
                *d += x;
            }
        } else if let Some(dst) = v.get_mut(start.min(v.len())..) {
            for (d, &x) in dst.iter_mut().zip(e) {
                *d += x;
            }
        }
        i += dims;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::CodeWriter;

    fn books() -> Vec<Codebook> {
        vec![
            // classbook: 2 entries, codes 0 / 1
            Codebook::from_parts(&[1, 1], 1, None),
            // VQ: 4 entries of 2 values, codes 00 01 10 11
            Codebook::from_parts(
                &[2, 2, 2, 2],
                2,
                Some(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
            ),
        ]
    }

    fn residue(kind: u16) -> Residue {
        let mut b1 = [-1i16; 8];
        b1[0] = 1;
        Residue {
            kind,
            begin: 2,
            end: 10,
            partition_size: 8,
            classifications: 2,
            classbook: 0,
            books: vec![[-1; 8], b1],
            pass_used: [true, false, false, false, false, false, false, false],
        }
    }

    fn run(kind: u16, bits: &CodeWriter, skip: &[bool]) -> Vec<Vec<f32>> {
        let mut v = vec![vec![0.0f32; 12]; skip.len()];
        let mut refs: Vec<&mut [f32]> = v.iter_mut().map(|x| &mut x[..]).collect();
        let mut r = BitReader::new(&bits.bytes);
        decode(
            &residue(kind),
            &books(),
            &mut r,
            &mut refs,
            skip,
            &mut Vec::new(),
        );
        v
    }

    #[test]
    fn formats_place_vectors() {
        let mut w = CodeWriter::default();
        w.code("1").code("00").code("01").code("10").code("11");
        // format 0: vector i goes to i, i + step (step = 8 / 2)
        let v = run(0, &w, &[false]);
        assert_eq!(
            v[0],
            [0.0, 0.0, 1.0, 3.0, 5.0, 7.0, 2.0, 4.0, 6.0, 8.0, 0.0, 0.0]
        );
        // format 1: concatenated
        let v = run(1, &w, &[false]);
        assert_eq!(
            v[0],
            [0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0]
        );
    }

    #[test]
    fn skipped_channels_and_class_without_book() {
        // channel 0 class 0 (no book), channel 1 skipped, channel 2 class 1
        let mut w = CodeWriter::default();
        w.code("0")
            .code("1")
            .code("11")
            .code("11")
            .code("00")
            .code("00");
        let v = run(1, &w, &[false, true, false]);
        assert!(v[0].iter().all(|&x| x == 0.0));
        assert!(v[1].iter().all(|&x| x == 0.0));
        assert_eq!(
            v[2],
            [0.0, 0.0, 7.0, 8.0, 7.0, 8.0, 1.0, 2.0, 1.0, 2.0, 0.0, 0.0]
        );
    }

    #[test]
    fn end_of_packet_keeps_partial_output() {
        let mut w = CodeWriter::default();
        w.code("1").code("11").code("10");
        let v = run(1, &w, &[false]);
        // two vectors decoded before the packet ran out (the trailing pad bits decode a third)
        assert_eq!(&v[0][2..6], &[7.0, 8.0, 5.0, 6.0]);
    }
}
