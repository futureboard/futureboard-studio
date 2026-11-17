//! MD5 message digest (RFC 1321), used for the FLAC `STREAMINFO` audio signature.

/// Per-step additive constants: `floor(abs(sin(i + 1)) * 2^32)` (RFC 1321 section 3.4).
const T: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// Left-rotation amounts, four per round.
const S: [[u32; 4]; 4] = [
    [7, 12, 17, 22],
    [5, 9, 14, 20],
    [4, 11, 16, 23],
    [6, 10, 15, 21],
];

/// Streaming MD5 hasher.
///
/// ```
/// let mut h = mkff_flac::Md5::new();
/// h.update(b"ab");
/// h.update(b"c");
/// assert_eq!(mkff_flac::Md5::hex(&h.finish()), "900150983cd24fb0d6963f7d28e17f72");
/// ```
#[derive(Clone, Debug)]
pub struct Md5 {
    state: [u32; 4],
    block: [u8; 64],
    block_len: usize,
    total: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

impl Md5 {
    /// A hasher with the RFC 1321 initial state.
    pub fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            block: [0; 64],
            block_len: 0,
            total: 0,
        }
    }

    /// MD5 of `data` in one call.
    pub fn digest(data: &[u8]) -> [u8; 16] {
        let mut h = Self::new();
        h.update(data);
        h.finish()
    }

    /// Lower-case hexadecimal form of a digest.
    pub fn hex(digest: &[u8; 16]) -> String {
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Feed more message bytes.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.block_len > 0 {
            let take = (64 - self.block_len).min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len < 64 {
                return;
            }
            let block = self.block;
            self.compress(&block);
            self.block_len = 0;
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for b in blocks {
            self.compress(b);
        }
        self.block[..rest.len()].copy_from_slice(rest);
        self.block_len = rest.len();
    }

    /// Pad, finish and return the 16-byte digest.
    pub fn finish(mut self) -> [u8; 16] {
        let bit_len = self.total.wrapping_mul(8);
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        let pad_len = if self.block_len < 56 {
            56 - self.block_len
        } else {
            120 - self.block_len
        };
        let total = self.total;
        self.update(&pad[..pad_len]);
        self.update(&bit_len.to_le_bytes());
        self.total = total;
        debug_assert_eq!(self.block_len, 0);
        let mut out = [0u8; 16];
        for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            o.copy_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut x = [0u32; 16];
        for (w, b) in x.iter_mut().zip(block.as_chunks::<4>().0) {
            *w = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        let [mut a, mut b, mut c, mut d] = self.state;
        for i in 0..64 {
            let round = i / 16;
            let (f, g) = match round {
                0 => ((b & c) | (!b & d), i),
                1 => ((b & d) | (c & !d), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let sum = a.wrapping_add(f).wrapping_add(T[i]).wrapping_add(x[g]);
            let next = b.wrapping_add(sum.rotate_left(S[round][i % 4]));
            a = d;
            d = c;
            c = b;
            b = next;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

#[cfg(test)]
mod tests {
    use super::Md5;

    #[test]
    fn rfc1321_suite() {
        let cases: [(&str, &str); 7] = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (msg, hex) in cases {
            assert_eq!(Md5::hex(&Md5::digest(msg.as_bytes())), hex, "{msg:?}");
            // byte-at-a-time feeding gives the same digest
            let mut h = Md5::new();
            for b in msg.as_bytes() {
                h.update(std::slice::from_ref(b));
            }
            assert_eq!(Md5::hex(&h.finish()), hex);
        }
    }
}
