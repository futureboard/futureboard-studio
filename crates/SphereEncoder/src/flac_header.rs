//! Read-time correction of the STREAMINFO that earlier FLAC exports wrote.
//!
//! Before #352 the FLAC encoder folded the last, shorter block into
//! STREAMINFO's minimum block size. Every frame of such a file is coded with
//! the fixed blocking strategy and the fixed block size (the maximum), but a
//! minimum below the maximum makes decoders (symphonia among them) take the
//! stream for a variable-block-size one and refuse every frame ("end of
//! stream"), and a last block under 16 samples makes the header invalid
//! outright ("minimum block length is 16 samples"). The encoder writes the
//! right header now; this recognises the old one so readers can present the
//! minimum as the maximum, as the format has it, without touching the file.
//!
//! Always compiled, independent of the `flac` (encoder) feature: readers need
//! it whether or not they can write FLAC.

use std::io::{Read, Seek, SeekFrom};

/// Byte offset of STREAMINFO's minimum block size (16-bit big-endian) in a
/// FLAC file: after `fLaC` and the 4-byte metadata block header.
pub const STREAMINFO_MIN_BLOCK_SIZE_OFFSET: u64 = 8;

/// Metadata blocks walked before giving up on finding the first frame.
const MAX_METADATA_BLOCKS: usize = 256;

/// The minimum block size to read in place of a pre-#352 export's, or `None`
/// when the file is not FLAC, its header is consistent, or it is anything but
/// exactly that broken pattern:
///
/// * the first metadata block is a 34-byte STREAMINFO whose minimum block
///   size is below its maximum, and
/// * the first frame uses the fixed blocking strategy (sync `FF F8`) and its
///   block size is that maximum.
///
/// A fixed-blocksize stream has one block size for every block but the last,
/// so its minimum must equal its maximum; a genuinely variable-blocksize
/// stream (sync `FF F9`) is left alone. Read failures are `None` too: the
/// decoder then reports the file's own problem. The reader is left at an
/// unspecified position.
pub fn legacy_min_block_size_fix<R: Read + Seek>(reader: &mut R) -> Option<u16> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut head = [0u8; 8 + 34];
    reader.read_exact(&mut head).ok()?;
    let is_streaminfo = head[4] & 0x7f == 0 && head[5..8] == [0, 0, 34];
    if &head[..4] != b"fLaC" || !is_streaminfo {
        return None;
    }
    let min_block = u16::from_be_bytes([head[8], head[9]]);
    let max_block = u16::from_be_bytes([head[10], head[11]]);
    if min_block >= max_block {
        return None;
    }

    // Past the metadata blocks to the first frame.
    let mut at = 4u64;
    let mut block_header = [0u8; 4];
    let mut last = false;
    for _ in 0..MAX_METADATA_BLOCKS {
        reader.seek(SeekFrom::Start(at)).ok()?;
        reader.read_exact(&mut block_header).ok()?;
        let length = u32::from_be_bytes([0, block_header[1], block_header[2], block_header[3]]);
        at += 4 + u64::from(length);
        if block_header[0] & 0x80 != 0 {
            last = true;
            break;
        }
    }
    if !last {
        return None;
    }
    reader.seek(SeekFrom::Start(at)).ok()?;
    let mut frame = [0u8; 4 + 7 + 2];
    let read = read_up_to(reader, &mut frame)?;
    // Sync code with the fixed blocking strategy (and the reserved bit clear).
    if read < 5 || frame[..2] != [0xff, 0xf8] {
        return None;
    }
    // The coded frame number (UTF-8-like, 1 to 7 bytes) precedes any
    // explicit block size.
    let number_len = match frame[4] {
        byte if byte & 0x80 == 0 => 1,
        byte => {
            let ones = byte.leading_ones() as usize;
            if !(2..=7).contains(&ones) {
                return None;
            }
            ones
        }
    };
    let tail = 4 + number_len;
    let block_size = match frame[2] >> 4 {
        1 => 192,
        code @ 2..=5 => 576u32 << (code - 2),
        6 if read > tail => u32::from(frame[tail]) + 1,
        7 if read > tail + 1 => u32::from(u16::from_be_bytes([frame[tail], frame[tail + 1]])) + 1,
        code @ 8..=15 => 256u32 << (code - 8),
        _ => return None,
    };
    (block_size == u32::from(max_block)).then_some(max_block)
}

/// Fill as much of `buf` as the reader has; `None` on an I/O error.
fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> Option<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// `fLaC`, a STREAMINFO with the given block sizes, then a frame header
    /// starting with `frame`.
    fn file(min_block: u16, max_block: u16, frame: &[u8]) -> Vec<u8> {
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut info = [0u8; 34];
        info[..2].copy_from_slice(&min_block.to_be_bytes());
        info[2..4].copy_from_slice(&max_block.to_be_bytes());
        bytes.extend_from_slice(&info);
        bytes.extend_from_slice(frame);
        bytes
    }

    /// Fixed-blocksize frame 0 with block-size code 12 (4096 samples).
    const FIXED_4096: [u8; 6] = [0xff, 0xf8, 0xc9, 0x18, 0x00, 0x00];

    #[test]
    fn corrects_only_the_pre_352_pattern() {
        let fix = |bytes: Vec<u8>| legacy_min_block_size_fix(&mut Cursor::new(bytes));
        // The broken pattern: min = last block, fixed frames of `max`.
        assert_eq!(fix(file(2_545, 4_096, &FIXED_4096)), Some(4_096));
        assert_eq!(fix(file(1, 4_096, &FIXED_4096)), Some(4_096));
        // A consistent header is left alone.
        assert_eq!(fix(file(4_096, 4_096, &FIXED_4096)), None);
        // A variable-blocksize stream may have min < max.
        let mut variable = FIXED_4096;
        variable[1] = 0xf9;
        assert_eq!(fix(file(2_545, 4_096, &variable)), None);
        // A first frame that is not `max` long is not this encoder's output.
        let mut other_size = FIXED_4096;
        other_size[2] = 0xb9; // code 11: 2048
        assert_eq!(fix(file(1_000, 4_096, &other_size)), None);
        // An explicit 16-bit block size (code 7) after the frame number.
        let explicit = [0xff, 0xf8, 0x79, 0x18, 0x00, 0x0f, 0xff];
        assert_eq!(fix(file(100, 4_096, &explicit)), Some(4_096));
        // Not FLAC, or truncated before the first frame.
        assert_eq!(fix(b"RIFF....WAVE".to_vec()), None);
        assert_eq!(fix(file(2_545, 4_096, &[])), None);
    }
}
