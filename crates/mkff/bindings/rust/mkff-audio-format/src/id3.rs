//! Locating (and skipping) ID3v2, ID3v1 and APEv2 tags around raw streams.

use mkff_audio_core::Result;

use crate::io::{le32, Source};

/// Size of one ID3v2 tag at the start of `d` (header, body and footer), 0 if there is none.
fn one_id3v2(d: &[u8]) -> u64 {
    if d.len() < 10 || &d[..3] != b"ID3" || d[3] == 0xFF || d[4] == 0xFF {
        return 0;
    }
    if d[6..10].iter().any(|&b| b & 0x80 != 0) {
        return 0;
    }
    let size = d[6..10].iter().fold(0u64, |a, &b| (a << 7) | u64::from(b));
    let footer = if d[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

/// Total length of the ID3v2 tags at the start of `head` (possibly larger than `head`).
pub(crate) fn id3v2_len(head: &[u8]) -> u64 {
    let mut off = 0u64;
    loop {
        let rest = match head.get(off as usize..) {
            Some(r) if (off as usize) < head.len() => r,
            _ => return off,
        };
        let n = one_id3v2(rest);
        if n == 0 {
            return off;
        }
        off += n;
    }
}

/// Offset of the first byte after the ID3v2 tags at the start of the stream (0 if none).
pub(crate) fn skip_id3v2(src: &mut Source) -> Result<u64> {
    let mut off = 0u64;
    loop {
        let mut h = [0u8; 10];
        if src.read_at(off, &mut h)? < 10 {
            return Ok(off);
        }
        let n = one_id3v2(&h);
        if n == 0 || off + n > src.len() {
            return Ok(if n == 0 { off } else { src.len().min(off + n) });
        }
        off += n;
    }
}

/// End offset of the audio data: the stream length minus trailing ID3v1, APEv2, Lyrics3 and
/// appended ID3v2 tags. Never returns less than `start`.
pub(crate) fn audio_end(src: &mut Source, start: u64) -> Result<u64> {
    let mut end = src.len();
    for _ in 0..4 {
        let before = end;
        // ID3v1 (and the "TAG+" enhanced block before it)
        if end >= start + 128 {
            let mut t = [0u8; 3];
            src.read_at(end - 128, &mut t)?;
            if &t == b"TAG" {
                end -= 128;
                if end >= start + 227 {
                    let mut e = [0u8; 4];
                    src.read_at(end - 227, &mut e)?;
                    if &e == b"TAG+" {
                        end -= 227;
                    }
                }
            }
        }
        // APEv2 footer
        if end >= start + 32 {
            let mut f = [0u8; 32];
            src.read_at(end - 32, &mut f)?;
            if &f[..8] == b"APETAGEX" {
                let size = u64::from(le32(&f, 12));
                let flags = le32(&f, 20);
                let total = size + if flags & 0x8000_0000 != 0 { 32 } else { 0 };
                if total >= 32 && end >= start + total {
                    end -= total;
                }
            }
        }
        // Lyrics3 v2
        if end >= start + 15 {
            let mut f = [0u8; 15];
            src.read_at(end - 15, &mut f)?;
            if &f[6..] == b"LYRICS200" {
                let size: u64 = std::str::from_utf8(&f[..6])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(u64::MAX);
                if size < end && end - 15 - size >= start {
                    end -= 15 + size;
                }
            }
        }
        // appended ID3v2 with footer ("3DI")
        if end >= start + 10 {
            let mut f = [0u8; 10];
            src.read_at(end - 10, &mut f)?;
            if &f[..3] == b"3DI" && f[6..10].iter().all(|&b| b & 0x80 == 0) {
                let size = f[6..10].iter().fold(0u64, |a, &b| (a << 7) | u64::from(b)) + 20;
                if end >= start + size {
                    end -= size;
                }
            }
        }
        if end == before {
            break;
        }
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id3v2_sizes() {
        let mut t = b"ID3\x04\x00\x00\x00\x00\x01\x00".to_vec();
        assert_eq!(id3v2_len(&t), 10 + 128);
        t[5] = 0x10;
        assert_eq!(id3v2_len(&t), 10 + 128 + 10);
        t[8] = 0x80;
        assert_eq!(id3v2_len(&t), 0);
    }
}
