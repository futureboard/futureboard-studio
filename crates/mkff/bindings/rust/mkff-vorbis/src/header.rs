//! Identification and comment headers (spec §4.2.2, §5) and codec-configuration splitting.

use mkff_audio_core::{Error, Result};

/// Header packet types (the first byte of a header packet).
pub(crate) const PACKET_IDENT: u8 = 1;
pub(crate) const PACKET_COMMENT: u8 = 3;
pub(crate) const PACKET_SETUP: u8 = 5;

/// Whether `packet` starts like a Vorbis header of the given type (`type` byte + `"vorbis"`).
pub(crate) fn is_header(packet: &[u8], kind: u8) -> bool {
    packet.len() >= 7 && packet[0] == kind && &packet[1..7] == b"vorbis"
}

/// The identification header (the first of the three Vorbis headers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentHeader {
    /// Number of audio channels (1..=255).
    pub channels: u8,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Maximum bitrate in bits per second (0 or negative when unset).
    pub bitrate_max: i32,
    /// Nominal bitrate in bits per second (0 or negative when unset).
    pub bitrate_nominal: i32,
    /// Minimum bitrate in bits per second (0 or negative when unset).
    pub bitrate_min: i32,
    /// Short block size in samples (a power of two in 64..=8192).
    pub blocksize_0: usize,
    /// Long block size in samples (a power of two in 64..=8192, `>= blocksize_0`).
    pub blocksize_1: usize,
}

impl IdentHeader {
    /// Parse an identification header packet (30 bytes, starting `\x01vorbis`).
    pub fn parse(packet: &[u8]) -> Result<Self> {
        if !is_header(packet, PACKET_IDENT) {
            return Err(Error::invalid("vorbis: not an identification header"));
        }
        if packet.len() < 30 {
            return Err(Error::invalid("vorbis: identification header truncated"));
        }
        let u32_at =
            |o: usize| u32::from_le_bytes([packet[o], packet[o + 1], packet[o + 2], packet[o + 3]]);
        if u32_at(7) != 0 {
            return Err(Error::unsupported("vorbis: unknown vorbis_version"));
        }
        let channels = packet[11];
        let sample_rate = u32_at(12);
        if channels == 0 || sample_rate == 0 {
            return Err(Error::invalid("vorbis: zero channels or sample rate"));
        }
        let bs0 = u32::from(packet[28] & 0x0f);
        let bs1 = u32::from(packet[28] >> 4);
        if !(6..=13).contains(&bs0) || !(6..=13).contains(&bs1) || bs0 > bs1 {
            return Err(Error::invalid("vorbis: invalid block sizes"));
        }
        if packet[29] & 1 == 0 {
            return Err(Error::invalid(
                "vorbis: identification header framing bit unset",
            ));
        }
        Ok(Self {
            channels,
            sample_rate,
            bitrate_max: u32_at(16) as i32,
            bitrate_nominal: u32_at(20) as i32,
            bitrate_min: u32_at(24) as i32,
            blocksize_0: 1 << bs0,
            blocksize_1: 1 << bs1,
        })
    }
}

/// The comment header: vendor string and `KEY=value` user comments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Comments {
    /// The encoder's vendor string.
    pub vendor: String,
    /// User comments split at the first `=` (`(key, value)`, key as written; a comment without
    /// `=` has an empty value). Invalid UTF-8 is replaced with U+FFFD.
    pub comments: Vec<(String, String)>,
}

impl Comments {
    /// Parse a comment header packet (starting `\x03vorbis`). The final framing bit is not
    /// required.
    pub fn parse(packet: &[u8]) -> Result<Self> {
        Self::parse_len(packet).map(|(c, _)| c)
    }

    /// Parse and also return the header's length in bytes (including the framing byte when
    /// present).
    pub(crate) fn parse_len(packet: &[u8]) -> Result<(Self, usize)> {
        if !is_header(packet, PACKET_COMMENT) {
            return Err(Error::invalid("vorbis: not a comment header"));
        }
        let mut pos = 7usize;
        let truncated = || Error::invalid("vorbis: comment header truncated");
        let read_u32 = |pos: &mut usize| -> Result<usize> {
            let b = packet.get(*pos..*pos + 4).ok_or_else(truncated)?;
            *pos += 4;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        };
        let vlen = read_u32(&mut pos)?;
        let vendor = packet
            .get(pos..pos.saturating_add(vlen))
            .ok_or_else(truncated)?;
        pos += vlen;
        let count = read_u32(&mut pos)?;
        // each comment needs at least its 4-byte length
        if count > (packet.len() - pos) / 4 {
            return Err(truncated());
        }
        let mut comments = Vec::with_capacity(count);
        for _ in 0..count {
            let len = read_u32(&mut pos)?;
            let s = packet
                .get(pos..pos.saturating_add(len))
                .ok_or_else(truncated)?;
            pos += len;
            let s = String::from_utf8_lossy(s);
            let (k, v) = s.split_once('=').unwrap_or((&s, ""));
            comments.push((k.to_string(), v.to_string()));
        }
        if packet.get(pos).is_some_and(|b| b & 1 != 0) {
            pos += 1;
        }
        Ok((
            Self {
                vendor: String::from_utf8_lossy(vendor).into_owned(),
                comments,
            },
            pos,
        ))
    }

    /// The first value of `key` (compared ASCII case-insensitively, as Vorbis keys are).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.comments
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// Split a codec configuration blob into its identification, comment and setup headers.
///
/// Recognised layouts:
/// - Xiph lacing (Matroska / WebM `CodecPrivate`, Ogg-in-MP4 `dOps`-style blobs): a byte holding
///   the packet count minus one (2), the laced sizes of the first two packets, then the packets.
/// - Three headers each prefixed by a 16-bit big-endian length (`00 1E 01 'vorbis' …`).
/// - The three headers simply concatenated (`01 'vorbis' …`), found by parsing the comment header.
///
/// Returns `None` for an empty configuration.
pub fn split_config(config: &[u8]) -> Result<Option<[&[u8]; 3]>> {
    if config.is_empty() {
        return Ok(None);
    }
    let bad = || Error::invalid("vorbis: unrecognised codec configuration");
    if config[0] == 2 {
        // Xiph lacing
        let mut pos = 1usize;
        let mut sizes = [0usize; 2];
        for s in &mut sizes {
            loop {
                let b = *config.get(pos).ok_or_else(bad)?;
                pos += 1;
                *s += usize::from(b);
                if b != 255 {
                    break;
                }
            }
        }
        let id_end = pos.checked_add(sizes[0]).ok_or_else(bad)?;
        let com_end = id_end.checked_add(sizes[1]).ok_or_else(bad)?;
        if com_end > config.len() {
            return Err(bad());
        }
        return Ok(Some([
            &config[pos..id_end],
            &config[id_end..com_end],
            &config[com_end..],
        ]));
    }
    if config.len() >= 3 && config[0] == 0 && config[2] == PACKET_IDENT {
        // 16-bit big-endian length prefixes
        let mut out: [&[u8]; 3] = [&[]; 3];
        let mut pos = 0usize;
        for o in &mut out {
            let b = config.get(pos..pos + 2).ok_or_else(bad)?;
            let len = usize::from(u16::from_be_bytes([b[0], b[1]]));
            pos += 2;
            *o = config.get(pos..pos + len).ok_or_else(bad)?;
            pos += len;
        }
        return Ok(Some(out));
    }
    if is_header(config, PACKET_IDENT) && config.len() >= 30 {
        let rest = &config[30..];
        let com_len = match Comments::parse_len(rest) {
            Ok((_, len)) if is_header(&rest[len..], PACKET_SETUP) => len,
            // fall back to searching for the setup header
            _ => (7..rest.len().saturating_sub(6))
                .find(|&i| is_header(&rest[i..], PACKET_SETUP))
                .ok_or_else(bad)?,
        };
        return Ok(Some([&config[..30], &rest[..com_len], &rest[com_len..]]));
    }
    Err(bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn ident_bytes(channels: u8, rate: u32, bs: u8) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(b"vorbis");
        v.extend_from_slice(&0u32.to_le_bytes());
        v.push(channels);
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&0i32.to_le_bytes());
        v.extend_from_slice(&128_000i32.to_le_bytes());
        v.extend_from_slice(&0i32.to_le_bytes());
        v.push(bs);
        v.push(1);
        v
    }

    fn comment_bytes() -> Vec<u8> {
        let mut v = vec![3u8];
        v.extend_from_slice(b"vorbis");
        let vendor = b"test vendor";
        v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        v.extend_from_slice(vendor);
        let comments: [&[u8]; 3] = [b"TITLE=Hello", b"artist=A=B", b"bad\xff"];
        v.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            v.extend_from_slice(&(c.len() as u32).to_le_bytes());
            v.extend_from_slice(c);
        }
        v.push(1);
        v
    }

    #[test]
    fn ident() {
        let h = IdentHeader::parse(&ident_bytes(2, 44100, 0xb8)).unwrap();
        assert_eq!(h.channels, 2);
        assert_eq!(h.sample_rate, 44100);
        assert_eq!(h.bitrate_nominal, 128_000);
        assert_eq!((h.blocksize_0, h.blocksize_1), (256, 2048));
        assert!(IdentHeader::parse(&ident_bytes(2, 44100, 0x8b)).is_err()); // bs0 > bs1
        assert!(IdentHeader::parse(&ident_bytes(0, 44100, 0xb8)).is_err());
        assert!(IdentHeader::parse(&ident_bytes(2, 44100, 0xe8)).is_err()); // 16384
        let mut b = ident_bytes(2, 44100, 0xb8);
        b[29] = 0;
        assert!(IdentHeader::parse(&b).is_err());
        assert!(IdentHeader::parse(&b[..20]).is_err());
    }

    #[test]
    fn comments() {
        let c = Comments::parse(&comment_bytes()).unwrap();
        assert_eq!(c.vendor, "test vendor");
        assert_eq!(c.get("title"), Some("Hello"));
        assert_eq!(c.get("ARTIST"), Some("A=B"));
        assert_eq!(c.comments[2], ("bad\u{fffd}".to_string(), String::new()));
        let b = comment_bytes();
        for n in 0..b.len() - 1 {
            // truncations either parse (missing framing byte) or fail; never panic
            let _ = Comments::parse(&b[..n]);
        }
        assert!(Comments::parse(&b[..b.len() - 1]).is_ok());
    }

    #[test]
    fn config_layouts() {
        let id = ident_bytes(1, 8000, 0x86);
        let com = comment_bytes();
        let mut setup = vec![5u8];
        setup.extend_from_slice(b"vorbis");
        setup.extend_from_slice(&[0xaa; 300]);

        let mut laced = vec![2u8, id.len() as u8, com.len() as u8];
        laced.extend_from_slice(&id);
        laced.extend_from_slice(&com);
        laced.extend_from_slice(&setup);
        let mut be = Vec::new();
        for p in [&id, &com, &setup] {
            be.extend_from_slice(&(p.len() as u16).to_be_bytes());
            be.extend_from_slice(p);
        }
        let plain = [id.clone(), com.clone(), setup.clone()].concat();
        let plain_noframe = [id.clone(), com[..com.len() - 1].to_vec(), setup.clone()].concat();
        for (cfg, com_want) in [
            (&laced, &com[..]),
            (&be, &com),
            (&plain, &com),
            (&plain_noframe, &com[..com.len() - 1]),
        ] {
            let [a, b, c] = split_config(cfg).unwrap().unwrap();
            assert_eq!(a, &id[..]);
            assert_eq!(b, com_want);
            assert_eq!(c, &setup[..]);
        }
        assert!(split_config(&[]).unwrap().is_none());
        assert!(split_config(&[2, 255]).is_err());
        assert!(split_config(&[2, 30, 200, 1]).is_err());
        assert!(split_config(&[7, 7, 7]).is_err());
    }
}
