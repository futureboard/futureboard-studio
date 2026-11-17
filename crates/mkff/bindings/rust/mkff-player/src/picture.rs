//! Decoded pictures in the layouts the renderer uploads.

use std::sync::atomic::{AtomicU64, Ordering};

use mkff_codecs::{Frame, Matrix, Planes};

static SERIAL: AtomicU64 = AtomicU64::new(1);

/// Pixel data of a [`Picture`].
#[derive(Clone, Debug)]
pub enum PictureData {
    /// Planar 8-bit Y'CbCr, tight rows. Chroma planes are `chroma_width × chroma_height`
    /// (4:2:0, 4:2:2 or 4:4:4 — the shader samples them in normalised coordinates, so any
    /// subsampling renders correctly).
    Yuv {
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
        chroma_width: u32,
        chroma_height: u32,
    },
    /// Straight-alpha sRGB RGBA8, `width × height × 4` bytes.
    Rgba(Vec<u8>),
}

/// One decoded video frame, ready for upload.
#[derive(Clone, Debug)]
pub struct Picture {
    /// Process-unique id: the renderer re-uploads only when it changes.
    pub serial: u64,
    /// Frame index at the stream's frame rate.
    pub index: u64,
    /// Presentation time in seconds.
    pub pts: f64,
    pub width: u32,
    pub height: u32,
    /// Pixel aspect ratio (1:1 for square pixels).
    pub pixel_aspect: (u32, u32),
    pub matrix: Matrix,
    pub full_range: bool,
    /// Bit depth of the decoded stream (pictures are stored at 8 bits).
    pub source_bit_depth: u32,
    pub data: PictureData,
}

impl Picture {
    /// Convert a decoded frame. Deeper-than-8-bit Y'CbCr is rounded to 8 bits; alpha is
    /// dropped (video is shown over black); float RGBA goes through the frame's colour
    /// management to sRGB.
    pub fn from_frame(frame: &Frame, index: u64, pts: f64) -> Picture {
        let (width, height) = (frame.width(), frame.height());
        let color = frame.color();
        let (cw, ch) = (frame.chroma_width(), frame.chroma_height());
        let (data, depth) = match (frame.planes(), frame.layout()) {
            (Planes::U8 { y, u, v, .. }, _) => (
                PictureData::Yuv {
                    y: y.to_vec(),
                    u: u.to_vec(),
                    v: v.to_vec(),
                    chroma_width: cw,
                    chroma_height: ch,
                },
                8,
            ),
            (Planes::U16 { y, u, v, .. }, mkff_codecs::PixelLayout::Yuv { bit_depth, .. }) => (
                PictureData::Yuv {
                    y: to_8bit(y, bit_depth),
                    u: to_8bit(u, bit_depth),
                    v: to_8bit(v, bit_depth),
                    chroma_width: cw,
                    chroma_height: ch,
                },
                bit_depth,
            ),
            (Planes::Rgba8(px), _) => (PictureData::Rgba(px.to_vec()), 8),
            _ => (PictureData::Rgba(frame.to_rgba8()), 32),
        };
        let (par_n, par_d) = frame.pixel_aspect();
        Picture {
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            index,
            pts,
            width,
            height,
            pixel_aspect: if par_n == 0 || par_d == 0 {
                (1, 1)
            } else {
                (par_n, par_d)
            },
            matrix: color.matrix,
            full_range: color.full_range,
            source_bit_depth: depth,
            data,
        }
    }

    /// Display aspect ratio (width / height after pixel aspect).
    pub fn display_aspect(&self) -> f32 {
        if self.height == 0 {
            return 16.0 / 9.0;
        }
        (self.width as f32 * self.pixel_aspect.0 as f32)
            / (self.height as f32 * self.pixel_aspect.1 as f32)
    }

    /// Straight-alpha sRGB RGBA8 conversion on the CPU (snapshots, CPU renderers).
    pub fn to_rgba8(&self) -> Vec<u8> {
        match &self.data {
            PictureData::Rgba(px) => px.clone(),
            PictureData::Yuv {
                y,
                u,
                v,
                chroma_width,
                chroma_height,
            } => {
                let m = crate::color::yuv_to_rgb_matrix(self.matrix, self.full_range);
                let (w, h) = (self.width as usize, self.height as usize);
                let (cw, chh) = (*chroma_width as usize, *chroma_height as usize);
                let (sx, sy) = (w.div_ceil(cw.max(1)).max(1), h.div_ceil(chh.max(1)).max(1));
                let mut out = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    let crow = (row / sy).min(chh.saturating_sub(1));
                    for col in 0..w {
                        let ccol = (col / sx).min(cw.saturating_sub(1));
                        let p = [
                            y[row * w + col] as f32 / 255.0,
                            u[crow * cw + ccol] as f32 / 255.0,
                            v[crow * cw + ccol] as f32 / 255.0,
                            1.0,
                        ];
                        for r in &m {
                            let c = r[0] * p[0] + r[1] * p[1] + r[2] * p[2] + r[3];
                            out.push((c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
                        }
                        out.push(255);
                    }
                }
                out
            }
        }
    }
}

/// Round LSB-aligned `bits`-deep samples to 8 bits.
fn to_8bit(src: &[u16], bits: u32) -> Vec<u8> {
    if bits <= 8 {
        return src.iter().map(|&s| s.min(255) as u8).collect();
    }
    let shift = bits - 8;
    let half = 1u32 << (shift - 1);
    src.iter()
        .map(|&s| ((u32::from(s) + half) >> shift).min(255) as u8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_bit_rounding() {
        assert_eq!(
            to_8bit(&[0, 1, 2, 3, 1023, 64, 940], 10),
            vec![0, 0, 1, 1, 255, 16, 235]
        );
        assert_eq!(to_8bit(&[4095], 12), vec![255]);
        assert_eq!(to_8bit(&[300], 8), vec![255]);
    }
}
