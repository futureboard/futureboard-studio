//! Unit-test helpers.

/// Writes bits LSB-first as Vorbis packs them; `code` writes a codeword MSB-first (tree order).
#[derive(Default)]
pub(crate) struct CodeWriter {
    pub bytes: Vec<u8>,
    n: usize,
}

impl CodeWriter {
    pub fn bit(&mut self, b: bool) {
        if self.n.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if b {
            *self.bytes.last_mut().unwrap() |= 1 << (self.n % 8);
        }
        self.n += 1;
    }

    pub fn code(&mut self, s: &str) -> &mut Self {
        for c in s.chars() {
            self.bit(c == '1');
        }
        self
    }
}
