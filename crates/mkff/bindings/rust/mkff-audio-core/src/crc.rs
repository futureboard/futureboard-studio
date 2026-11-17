//! Table-driven CRCs used by the audio formats (MSB-first, no reflection, no final XOR).
//!
//! | Use | Width | Polynomial | Init |
//! |-----|-------|------------|------|
//! | FLAC frame header | 8 | 0x07 | 0 |
//! | FLAC frame, MPEG audio header, AC-3 | 16 | 0x8005 | 0 (FLAC, AC-3) / 0xFFFF (MPEG audio) |
//! | Ogg page | 32 | 0x04C11DB7 | 0 |

/// A CRC-8 / CRC-16 / CRC-32 with a 256-entry table.
#[derive(Clone, Debug)]
pub struct Crc {
    width: u32,
    table: [u32; 256],
    mask: u32,
}

impl Crc {
    pub fn new(width: u32, poly: u32) -> Self {
        assert!((8..=32).contains(&width));
        let mask = if width == 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        let top = 1u32 << (width - 1);
        let mut table = [0u32; 256];
        for (i, t) in table.iter_mut().enumerate() {
            let mut c = (i as u32) << (width - 8);
            for _ in 0..8 {
                c = if c & top != 0 {
                    (c << 1) ^ poly
                } else {
                    c << 1
                };
            }
            *t = c & mask;
        }
        Self { width, table, mask }
    }

    /// Continue a CRC over `data`.
    pub fn update(&self, mut crc: u32, data: &[u8]) -> u32 {
        let sh = self.width - 8;
        for &b in data {
            let idx = ((crc >> sh) ^ u32::from(b)) & 0xFF;
            crc = ((crc << 8) ^ self.table[idx as usize]) & self.mask;
        }
        crc
    }

    pub fn checksum(&self, init: u32, data: &[u8]) -> u32 {
        self.update(init, data)
    }
}

/// CRC-8, polynomial 0x07, init 0 (FLAC frame headers).
pub fn crc8(data: &[u8]) -> u8 {
    static T: std::sync::OnceLock<Crc> = std::sync::OnceLock::new();
    T.get_or_init(|| Crc::new(8, 0x07)).checksum(0, data) as u8
}

/// CRC-16, polynomial 0x8005, from `init` (0 for FLAC / AC-3, 0xFFFF for MPEG audio).
pub fn crc16(init: u16, data: &[u8]) -> u16 {
    static T: std::sync::OnceLock<Crc> = std::sync::OnceLock::new();
    T.get_or_init(|| Crc::new(16, 0x8005))
        .checksum(u32::from(init), data) as u16
}

/// CRC-32, polynomial 0x04C11DB7, init 0 (Ogg pages).
pub fn crc32_ogg(data: &[u8]) -> u32 {
    static T: std::sync::OnceLock<Crc> = std::sync::OnceLock::new();
    T.get_or_init(|| Crc::new(32, 0x04C1_1DB7))
        .checksum(0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // CRC-8/SMBUS and CRC-16/UMTS ("BUYPASS") check values for "123456789"
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc16(0, b"123456789"), 0xFEE8);
        // CRC-32/MPEG-2 with init 0 (Ogg): CRC-32/MPEG-2 check is 0x0376E6E7 with init 0xFFFFFFFF
        let c = Crc::new(32, 0x04C1_1DB7);
        assert_eq!(c.checksum(0xFFFF_FFFF, b"123456789"), 0x0376_E6E7);
    }
}
