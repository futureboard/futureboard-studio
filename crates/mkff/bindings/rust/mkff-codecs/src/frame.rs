use std::sync::Arc;

use filmcraft_frame::{Chroma, PixelData, VideoFrame};

/// Chroma subsampling of a planar Y'CbCr frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChromaFormat {
    Yuv420,
    Yuv422,
    Yuv444,
}

impl ChromaFormat {
    /// (horizontal, vertical) subsampling shifts.
    pub fn shifts(self) -> (u32, u32) {
        match self {
            ChromaFormat::Yuv420 => (1, 1),
            ChromaFormat::Yuv422 => (1, 0),
            ChromaFormat::Yuv444 => (0, 0),
        }
    }

    fn from_fc(c: &Chroma) -> Self {
        match c {
            Chroma::C420 => ChromaFormat::Yuv420,
            Chroma::C422 => ChromaFormat::Yuv422,
            Chroma::C444 => ChromaFormat::Yuv444,
        }
    }
}

/// How a [`Frame`] stores its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelLayout {
    /// Planar Y'CbCr. `bit_depth` 8 is stored in `u8` planes, anything above in `u16` planes
    /// (LSB-aligned: `bit_depth` significant bits).
    Yuv {
        chroma: ChromaFormat,
        bit_depth: u32,
        alpha: bool,
    },
    /// Straight-alpha, sRGB-encoded RGBA, 8 bits per channel (Motion JPEG, stills).
    Rgba8,
    /// Premultiplied linear-light RGBA f32.
    RgbaF32,
}

/// Borrowed pixel planes of a [`Frame`]. Planes are tight: a luma row is `width` samples, a
/// chroma row [`Frame::chroma_width`] samples.
#[derive(Clone, Copy, Debug)]
pub enum Planes<'a> {
    U8 {
        y: &'a [u8],
        u: &'a [u8],
        v: &'a [u8],
        alpha: Option<&'a [u8]>,
    },
    U16 {
        y: &'a [u16],
        u: &'a [u16],
        v: &'a [u16],
        alpha: Option<&'a [u16]>,
    },
    Rgba8(&'a [u8]),
    RgbaF32(&'a [f32]),
}

/// Y'CbCr → R'G'B' matrix coefficients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020Ncl,
}

/// Colour description of a frame (from the bitstream VUI or the container).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ColorDesc {
    pub matrix: Matrix,
    /// Full-range (0-255) instead of video-range (16-235) samples.
    pub full_range: bool,
}

/// A decoded picture.
#[derive(Clone, Debug)]
pub struct Frame {
    inner: VideoFrame,
    pts: i64,
}

impl Frame {
    pub(crate) fn new(inner: VideoFrame, pts: i64) -> Self {
        Self { inner, pts }
    }

    pub fn width(&self) -> u32 {
        self.inner.width
    }

    pub fn height(&self) -> u32 {
        self.inner.height
    }

    /// Presentation timestamp: the `pts` passed with the packet to [`crate::VideoDecoder`], or the
    /// frame's media time in nanoseconds for frames from [`crate::MediaFile`].
    pub fn pts(&self) -> i64 {
        self.pts
    }

    /// Pixel aspect ratio (1:1 for square pixels).
    pub fn pixel_aspect(&self) -> (u32, u32) {
        self.inner.par
    }

    pub fn layout(&self) -> PixelLayout {
        match &self.inner.data {
            PixelData::Yuv8 { chroma, alpha, .. } => PixelLayout::Yuv {
                chroma: ChromaFormat::from_fc(chroma),
                bit_depth: 8,
                alpha: alpha.is_some(),
            },
            PixelData::Yuv16 {
                chroma,
                bits,
                alpha,
                ..
            } => PixelLayout::Yuv {
                chroma: ChromaFormat::from_fc(chroma),
                bit_depth: *bits,
                alpha: alpha.is_some(),
            },
            PixelData::Rgba8(_) => PixelLayout::Rgba8,
            PixelData::RgbaF32(_) => PixelLayout::RgbaF32,
        }
    }

    pub fn color(&self) -> ColorDesc {
        use filmcraft_color::{Matrix as M, Range};
        let c = self.inner.color;
        ColorDesc {
            matrix: match c.matrix {
                M::Bt601 => Matrix::Bt601,
                M::Bt709 => Matrix::Bt709,
                M::Bt2020Ncl => Matrix::Bt2020Ncl,
            },
            full_range: c.range == Range::Full,
        }
    }

    /// Width of the chroma planes (luma width for RGBA layouts).
    pub fn chroma_width(&self) -> u32 {
        match self.layout() {
            PixelLayout::Yuv { chroma, .. } => self.width().div_ceil(1 << chroma.shifts().0),
            _ => self.width(),
        }
    }

    /// Height of the chroma planes (luma height for RGBA layouts).
    pub fn chroma_height(&self) -> u32 {
        match self.layout() {
            PixelLayout::Yuv { chroma, .. } => self.height().div_ceil(1 << chroma.shifts().1),
            _ => self.height(),
        }
    }

    pub fn planes(&self) -> Planes<'_> {
        match &self.inner.data {
            PixelData::Yuv8 { planes, alpha, .. } => Planes::U8 {
                y: &planes[0],
                u: &planes[1],
                v: &planes[2],
                alpha: alpha.as_deref().map(Vec::as_slice),
            },
            PixelData::Yuv16 { planes, alpha, .. } => Planes::U16 {
                y: &planes[0],
                u: &planes[1],
                v: &planes[2],
                alpha: alpha.as_deref().map(Vec::as_slice),
            },
            PixelData::Rgba8(d) => Planes::Rgba8(d),
            PixelData::RgbaF32(d) => Planes::RgbaF32(d),
        }
    }

    /// Straight-alpha sRGB RGBA8, `width * height * 4` bytes (colour-managed: honours the matrix,
    /// range and transfer of the frame).
    pub fn to_rgba8(&self) -> Vec<u8> {
        self.inner.to_rgba8()
    }

    /// 8-bit NV12: `width * height` luma bytes, then `chroma_width_420 * chroma_height_420` CbCr
    /// pairs, where the 4:2:0 chroma size is `ceil(width / 2) x ceil(height / 2)`.
    ///
    /// Deeper frames are rounded to 8 bits; 4:2:2 / 4:4:4 chroma is box-filtered down to 4:2:0;
    /// RGBA frames are converted with the BT.709 video-range matrix. Alpha is dropped.
    pub fn to_nv12(&self) -> Vec<u8> {
        let (w, h) = (self.width() as usize, self.height() as usize);
        match (&self.inner.data, self.layout()) {
            (PixelData::Yuv8 { planes, .. }, PixelLayout::Yuv { chroma, .. }) => semi_planar(
                [&planes[0][..], &planes[1][..], &planes[2][..]],
                w,
                h,
                chroma,
                |v| v as u8,
            ),
            (
                PixelData::Yuv16 { planes, .. },
                PixelLayout::Yuv {
                    chroma, bit_depth, ..
                },
            ) => {
                let shift = bit_depth.saturating_sub(8);
                let half = if shift > 0 { 1u32 << (shift - 1) } else { 0 };
                semi_planar(
                    [&planes[0][..], &planes[1][..], &planes[2][..]],
                    w,
                    h,
                    chroma,
                    |v| ((v + half) >> shift).min(255) as u8,
                )
            }
            _ => {
                let planes = rgba_to_yuv444(&self.to_rgba8(), w, h);
                semi_planar(
                    [&planes[0][..], &planes[1][..], &planes[2][..]],
                    w,
                    h,
                    ChromaFormat::Yuv444,
                    |v| v as u8,
                )
            }
        }
    }

    /// 10-bit P010 (MSB-aligned in `u16`, the layout `MKFF_PIXEL_FORMAT_P010` uses): `width *
    /// height` luma samples, then `ceil(width / 2) * ceil(height / 2)` CbCr pairs. Samples of any
    /// depth are scaled to the 16-bit container; chroma is box-filtered to 4:2:0 as in
    /// [`Self::to_nv12`].
    pub fn to_p010(&self) -> Vec<u16> {
        let (w, h) = (self.width() as usize, self.height() as usize);
        match (&self.inner.data, self.layout()) {
            (PixelData::Yuv8 { planes, .. }, PixelLayout::Yuv { chroma, .. }) => semi_planar(
                [&planes[0][..], &planes[1][..], &planes[2][..]],
                w,
                h,
                chroma,
                |v| (v as u16) << 8,
            ),
            (
                PixelData::Yuv16 { planes, .. },
                PixelLayout::Yuv {
                    chroma, bit_depth, ..
                },
            ) => {
                let shift = 16u32.saturating_sub(bit_depth.min(16));
                // keep 10 significant bits, as P010 does
                semi_planar(
                    [&planes[0][..], &planes[1][..], &planes[2][..]],
                    w,
                    h,
                    chroma,
                    |v| ((v << shift).min(0xFFFF) as u16) & 0xFFC0,
                )
            }
            _ => {
                let planes = rgba_to_yuv444(&self.to_rgba8(), w, h);
                semi_planar(
                    [&planes[0][..], &planes[1][..], &planes[2][..]],
                    w,
                    h,
                    ChromaFormat::Yuv444,
                    |v| (v as u16) << 8,
                )
            }
        }
    }

    /// The underlying FilmCraft frame (colour metadata, compositor conversions).
    pub fn as_filmcraft(&self) -> &VideoFrame {
        &self.inner
    }

    pub fn into_filmcraft(self) -> VideoFrame {
        self.inner
    }

    pub(crate) fn from_shared(f: Arc<VideoFrame>) -> Self {
        let pts = (f.pts.seconds() * 1e9).round() as i64;
        let inner = Arc::try_unwrap(f).unwrap_or_else(|a| (*a).clone());
        Self { inner, pts }
    }
}

/// Planar Y'CbCr (any subsampling) → semi-planar 4:2:0 (Y plane, then interleaved CbCr), each
/// output sample produced by `conv` from the (box-averaged) source sample.
fn semi_planar<S: Copy + Into<u32>, D: Copy + Default>(
    planes: [&[S]; 3],
    w: usize,
    h: usize,
    chroma: ChromaFormat,
    conv: impl Fn(u32) -> D,
) -> Vec<D> {
    let (sx, sy) = chroma.shifts();
    let (scw, sch) = (w.div_ceil(1 << sx), h.div_ceil(1 << sy));
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = vec![D::default(); w * h + cw * ch * 2];
    let (luma, uv) = out.split_at_mut(w * h);
    for (o, &s) in luma.iter_mut().zip(planes[0]) {
        *o = conv(s.into());
    }
    // source chroma samples covering one 4:2:0 position: (2 >> sx) x (2 >> sy)
    let (bx, by) = (2usize >> sx, 2usize >> sy);
    let at = |p: &[S], x: usize, y: usize| -> u32 {
        p.get(y.min(sch.saturating_sub(1)) * scw + x.min(scw.saturating_sub(1)))
            .map_or(0, |&s| s.into())
    };
    for y in 0..ch {
        for x in 0..cw {
            let (mut su, mut sv) = (0u32, 0u32);
            for dy in 0..by {
                for dx in 0..bx {
                    su += at(planes[1], x * bx + dx, y * by + dy);
                    sv += at(planes[2], x * bx + dx, y * by + dy);
                }
            }
            let n = (bx * by) as u32;
            let i = (y * cw + x) * 2;
            uv[i] = conv((su + n / 2) / n);
            uv[i + 1] = conv((sv + n / 2) / n);
        }
    }
    out
}

/// sRGB RGBA8 → BT.709 video-range Y'CbCr 4:4:4 planes.
fn rgba_to_yuv444(rgba: &[u8], w: usize, h: usize) -> [Vec<u8>; 3] {
    let n = w * h;
    let (mut y, mut u, mut v) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    for p in rgba.chunks_exact(4).take(n) {
        let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
        let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        y.push((16.0 + l * 219.0 / 255.0).round().clamp(0.0, 255.0) as u8);
        u.push(
            (128.0 + (b - l) / 1.8556 * 224.0 / 255.0)
                .round()
                .clamp(0.0, 255.0) as u8,
        );
        v.push(
            (128.0 + (r - l) / 1.5748 * 224.0 / 255.0)
                .round()
                .clamp(0.0, 255.0) as u8,
        );
    }
    [y, u, v]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yuv8(w: u32, h: u32, chroma: Chroma, y: Vec<u8>, u: Vec<u8>, v: Vec<u8>) -> Frame {
        let data = PixelData::Yuv8 {
            planes: [Arc::new(y), Arc::new(u), Arc::new(v)],
            chroma,
            alpha: None,
        };
        Frame::new(
            VideoFrame {
                width: w,
                height: h,
                data,
                color: filmcraft_color::ColorInfo::REC709,
                par: (1, 1),
                pts: filmcraft_time::Tick::ZERO,
            },
            0,
        )
    }

    #[test]
    fn nv12_from_420_is_a_straight_interleave() {
        let f = yuv8(
            4,
            2,
            Chroma::C420,
            (0..8).collect(),
            vec![10, 20],
            vec![30, 40],
        );
        assert_eq!(f.to_nv12(), vec![0, 1, 2, 3, 4, 5, 6, 7, 10, 30, 20, 40]);
    }

    #[test]
    fn nv12_from_444_box_filters_chroma() {
        let f = yuv8(
            2,
            2,
            Chroma::C444,
            vec![1; 4],
            vec![10, 20, 30, 40],
            vec![0, 0, 4, 4],
        );
        assert_eq!(f.to_nv12(), vec![1, 1, 1, 1, 25, 2]);
    }

    #[test]
    fn odd_sizes_round_the_chroma_plane_up() {
        let f = yuv8(3, 3, Chroma::C422, vec![0; 9], vec![100; 6], vec![50; 6]);
        let nv12 = f.to_nv12();
        assert_eq!(nv12.len(), 9 + 2 * 2 * 2);
        assert!(nv12[9..].chunks(2).all(|c| c == [100, 50]));
    }

    #[test]
    fn p010_is_msb_aligned() {
        let data = PixelData::Yuv16 {
            planes: [
                Arc::new(vec![1023; 4]),
                Arc::new(vec![512]),
                Arc::new(vec![64]),
            ],
            chroma: Chroma::C420,
            bits: 10,
            alpha: None,
        };
        let f = Frame::new(
            VideoFrame {
                width: 2,
                height: 2,
                data,
                color: filmcraft_color::ColorInfo::REC709,
                par: (1, 1),
                pts: filmcraft_time::Tick::ZERO,
            },
            0,
        );
        assert_eq!(
            f.to_p010(),
            vec![0xFFC0, 0xFFC0, 0xFFC0, 0xFFC0, 512 << 6, 64 << 6]
        );
        assert_eq!(f.to_nv12(), vec![255, 255, 255, 255, 128, 16]);
    }
}
