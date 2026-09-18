//! Crate-local planar image type used by the composition layer.
//!
//! [`HeifFrame`] is a tightly packed planar YCbCr / monochrome picture
//! with an optional alpha plane: one byte per sample at 8 bits, one
//! little-endian 16-bit word per sample above 8 bits (sample values in
//! the low bits). It mirrors what the codec crates emit and what the
//! framework's `VideoFrame` carries, without depending on either, so
//! the standalone build can compose grids / overlays / alpha over
//! frames decoded by any HEVC / AV1 implementation.

use crate::error::{HeifError, Result};

/// Chroma sampling structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Chroma {
    /// 4:0:0 — luma only.
    Mono,
    /// 4:2:0 — chroma halved horizontally and vertically.
    Yuv420,
    /// 4:2:2 — chroma halved horizontally.
    Yuv422,
    /// 4:4:4 — full-resolution chroma.
    Yuv444,
}

impl Chroma {
    /// From an HEVC / AV1-style `chroma_format_idc`.
    pub fn from_idc(idc: u8) -> Option<Self> {
        match idc {
            0 => Some(Chroma::Mono),
            1 => Some(Chroma::Yuv420),
            2 => Some(Chroma::Yuv422),
            3 => Some(Chroma::Yuv444),
            _ => None,
        }
    }

    /// The `chroma_format_idc` value.
    pub fn idc(&self) -> u8 {
        match self {
            Chroma::Mono => 0,
            Chroma::Yuv420 => 1,
            Chroma::Yuv422 => 2,
            Chroma::Yuv444 => 3,
        }
    }

    /// `(shift_x, shift_y)` of the chroma planes relative to luma.
    pub fn shift(&self) -> (u32, u32) {
        match self {
            Chroma::Mono => (0, 0),
            Chroma::Yuv420 => (1, 1),
            Chroma::Yuv422 => (1, 0),
            Chroma::Yuv444 => (0, 0),
        }
    }

    /// Number of colour planes (1 or 3).
    pub fn colour_planes(&self) -> usize {
        if *self == Chroma::Mono {
            1
        } else {
            3
        }
    }

    /// Chroma plane size for a `width × height` luma plane.
    pub fn chroma_dims(&self, width: u32, height: u32) -> (u32, u32) {
        let (sx, sy) = self.shift();
        (
            (width + (1 << sx) - 1) >> sx,
            (height + (1 << sy) - 1) >> sy,
        )
    }
}

/// Sample layout of a [`HeifFrame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HeifPixelFormat {
    /// Chroma structure.
    pub chroma: Chroma,
    /// Bits per sample (8..=16).
    pub bit_depth: u8,
    /// Whether a full-resolution alpha plane follows the colour planes.
    pub has_alpha: bool,
}

impl HeifPixelFormat {
    /// Build a format; `bit_depth` outside `8..=16` is rejected.
    pub fn new(chroma: Chroma, bit_depth: u8, has_alpha: bool) -> Result<Self> {
        if !(8..=16).contains(&bit_depth) {
            return Err(HeifError::unsupported(format!("bit depth {bit_depth}")));
        }
        Ok(Self {
            chroma,
            bit_depth,
            has_alpha,
        })
    }

    /// Bytes per sample (1 or 2).
    pub fn bytes_per_sample(&self) -> usize {
        if self.bit_depth > 8 {
            2
        } else {
            1
        }
    }

    /// Maximum sample value.
    pub fn max_value(&self) -> u16 {
        ((1u32 << self.bit_depth) - 1) as u16
    }

    /// Number of planes (colour planes + alpha).
    pub fn plane_count(&self) -> usize {
        self.chroma.colour_planes() + self.has_alpha as usize
    }

    /// Index of the alpha plane, when present.
    pub fn alpha_plane(&self) -> Option<usize> {
        self.has_alpha.then_some(self.chroma.colour_planes())
    }

    /// Size of plane `plane` for a `width × height` picture.
    pub fn plane_dims(&self, plane: usize, width: u32, height: u32) -> (u32, u32) {
        if plane == 0 || Some(plane) == self.alpha_plane() {
            (width, height)
        } else {
            self.chroma.chroma_dims(width, height)
        }
    }

    /// The same layout with an alpha plane.
    pub fn with_alpha(&self) -> Self {
        Self {
            has_alpha: true,
            ..*self
        }
    }

    /// The same layout without an alpha plane.
    pub fn without_alpha(&self) -> Self {
        Self {
            has_alpha: false,
            ..*self
        }
    }
}

/// One plane: `stride` bytes per row, rows packed top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeifPlane {
    /// Bytes per row.
    pub stride: usize,
    /// `stride × rows` bytes.
    pub data: Vec<u8>,
}

/// A planar picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeifFrame {
    /// Luma width in pixels.
    pub width: u32,
    /// Luma height in pixels.
    pub height: u32,
    /// Sample layout.
    pub format: HeifPixelFormat,
    /// The planes, `format.plane_count()` of them.
    pub planes: Vec<HeifPlane>,
}

/// Upper bound on the luma pixel count of a frame this crate allocates.
pub const MAX_FRAME_PIXELS: u64 = 1 << 30;

impl HeifFrame {
    /// Allocate a frame filled with `value` in every plane.
    pub fn filled(width: u32, height: u32, format: HeifPixelFormat, value: u16) -> Result<Self> {
        Self::check_dims(width, height)?;
        let bps = format.bytes_per_sample();
        let mut planes = Vec::with_capacity(format.plane_count());
        for p in 0..format.plane_count() {
            let (w, h) = format.plane_dims(p, width, height);
            let stride = w as usize * bps;
            let mut data = vec![0u8; stride * h as usize];
            if value != 0 {
                if bps == 1 {
                    data.fill(value as u8);
                } else {
                    for px in data.chunks_exact_mut(2) {
                        px.copy_from_slice(&value.to_le_bytes());
                    }
                }
            }
            planes.push(HeifPlane { stride, data });
        }
        Ok(Self {
            width,
            height,
            format,
            planes,
        })
    }

    /// Allocate a black (all-zero) frame.
    pub fn zeroed(width: u32, height: u32, format: HeifPixelFormat) -> Result<Self> {
        Self::filled(width, height, format, 0)
    }

    fn check_dims(width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(HeifError::invalid("zero-sized frame"));
        }
        if width as u64 * height as u64 > MAX_FRAME_PIXELS {
            return Err(HeifError::exhausted(format!(
                "frame {width}x{height} exceeds {MAX_FRAME_PIXELS} pixels"
            )));
        }
        Ok(())
    }

    /// Verify plane count / sizes match the declared layout.
    pub fn validate(&self) -> Result<()> {
        Self::check_dims(self.width, self.height)?;
        if self.planes.len() != self.format.plane_count() {
            return Err(HeifError::invalid(format!(
                "frame has {} planes, layout needs {}",
                self.planes.len(),
                self.format.plane_count()
            )));
        }
        let bps = self.format.bytes_per_sample();
        for (i, p) in self.planes.iter().enumerate() {
            let (w, h) = self.format.plane_dims(i, self.width, self.height);
            if p.stride < w as usize * bps {
                return Err(HeifError::invalid(format!(
                    "plane {i}: stride {} < {} bytes per row",
                    p.stride,
                    w as usize * bps
                )));
            }
            if p.stride == 0 || p.data.len() < p.stride * (h as usize - 1) + w as usize * bps {
                return Err(HeifError::invalid(format!(
                    "plane {i}: {} bytes, need {} rows of stride {}",
                    p.data.len(),
                    h,
                    p.stride
                )));
            }
        }
        Ok(())
    }

    /// Size of plane `plane`.
    pub fn plane_dims(&self, plane: usize) -> (u32, u32) {
        self.format.plane_dims(plane, self.width, self.height)
    }

    /// Read one sample (no bounds check beyond the slice's own).
    #[inline]
    pub fn sample(&self, plane: usize, x: u32, y: u32) -> u16 {
        let p = &self.planes[plane];
        let bps = self.format.bytes_per_sample();
        let i = y as usize * p.stride + x as usize * bps;
        if bps == 1 {
            p.data[i] as u16
        } else {
            u16::from_le_bytes([p.data[i], p.data[i + 1]])
        }
    }

    /// Write one sample.
    #[inline]
    pub fn set_sample(&mut self, plane: usize, x: u32, y: u32, v: u16) {
        let bps = self.format.bytes_per_sample();
        let p = &mut self.planes[plane];
        let i = y as usize * p.stride + x as usize * bps;
        if bps == 1 {
            p.data[i] = v as u8;
        } else {
            p.data[i..i + 2].copy_from_slice(&v.to_le_bytes());
        }
    }

    /// One row of plane `plane` as bytes (`width × bps` long).
    pub fn row(&self, plane: usize, y: u32) -> &[u8] {
        let p = &self.planes[plane];
        let (w, _) = self.plane_dims(plane);
        let start = y as usize * p.stride;
        &p.data[start..start + w as usize * self.format.bytes_per_sample()]
    }

    /// A tightly packed copy (stride == row bytes).
    pub fn tight(&self) -> Self {
        let bps = self.format.bytes_per_sample();
        let planes = (0..self.planes.len())
            .map(|i| {
                let (w, h) = self.plane_dims(i);
                let stride = w as usize * bps;
                let mut data = Vec::with_capacity(stride * h as usize);
                for y in 0..h {
                    data.extend_from_slice(self.row(i, y));
                }
                HeifPlane { stride, data }
            })
            .collect();
        Self {
            width: self.width,
            height: self.height,
            format: self.format,
            planes,
        }
    }

    /// The colour planes only (drops alpha).
    pub fn without_alpha(&self) -> Self {
        let n = self.format.chroma.colour_planes();
        Self {
            width: self.width,
            height: self.height,
            format: self.format.without_alpha(),
            planes: self.planes[..n].to_vec(),
        }
    }

    /// The alpha plane as a monochrome frame, when present.
    pub fn alpha_as_frame(&self) -> Option<Self> {
        let i = self.format.alpha_plane()?;
        Some(Self {
            width: self.width,
            height: self.height,
            format: HeifPixelFormat {
                chroma: Chroma::Mono,
                bit_depth: self.format.bit_depth,
                has_alpha: false,
            },
            planes: vec![self.planes[i].clone()],
        })
    }

    /// Attach `alpha` (a monochrome frame of the same size and depth)
    /// as the alpha plane.
    pub fn with_alpha_plane(&self, alpha: &HeifFrame) -> Result<Self> {
        if alpha.width != self.width || alpha.height != self.height {
            return Err(HeifError::invalid(format!(
                "alpha plane {}x{} does not match the {}x{} master",
                alpha.width, alpha.height, self.width, self.height
            )));
        }
        if alpha.format.bit_depth != self.format.bit_depth {
            return Err(HeifError::unsupported(format!(
                "alpha depth {} differs from master depth {}",
                alpha.format.bit_depth, self.format.bit_depth
            )));
        }
        let mut out = self.without_alpha();
        out.format = out.format.with_alpha();
        out.planes.push(alpha.planes[0].clone());
        out.validate()?;
        Ok(out)
    }
}

/// Bridges to the framework types (`registry` feature).
#[cfg(feature = "registry")]
pub mod core_bridge {
    use super::*;
    use oxideav_core::{PixelFormat, VideoFrame, VideoPlane};

    impl HeifPixelFormat {
        /// The framework pixel format for this layout, when one exists.
        ///
        /// Monochrome + alpha above 8 bits and 4:2:0 12-bit + alpha
        /// have no framework layout; callers promote them with
        /// [`HeifFrame::to_core`]'s neutral-chroma 4:4:4 fallback.
        pub fn to_core(&self) -> Option<PixelFormat> {
            use Chroma::*;
            Some(match (self.chroma, self.bit_depth, self.has_alpha) {
                (Mono, 8, false) => PixelFormat::Gray8,
                (Mono, 10, false) => PixelFormat::Gray10Le,
                (Mono, 12, false) => PixelFormat::Gray12Le,
                (Mono, 16, false) => PixelFormat::Gray16Le,
                (Mono, 8, true) => PixelFormat::Ya8,
                (Yuv420, 8, false) => PixelFormat::Yuv420P,
                (Yuv420, 10, false) => PixelFormat::Yuv420P10Le,
                (Yuv420, 12, false) => PixelFormat::Yuv420P12Le,
                (Yuv420, 16, false) => PixelFormat::Yuv420P16Le,
                (Yuv420, 8, true) => PixelFormat::Yuva420P,
                (Yuv420, 10, true) => PixelFormat::Yuva420P10Le,
                (Yuv422, 8, false) => PixelFormat::Yuv422P,
                (Yuv422, 10, false) => PixelFormat::Yuv422P10Le,
                (Yuv422, 12, false) => PixelFormat::Yuv422P12Le,
                (Yuv422, 16, false) => PixelFormat::Yuv422P16Le,
                (Yuv422, 8, true) => PixelFormat::Yuva422P,
                (Yuv422, 10, true) => PixelFormat::Yuva422P10Le,
                (Yuv422, 12, true) => PixelFormat::Yuva422P12Le,
                (Yuv422, 16, true) => PixelFormat::Yuva422P16Le,
                (Yuv444, 8, false) => PixelFormat::Yuv444P,
                (Yuv444, 10, false) => PixelFormat::Yuv444P10Le,
                (Yuv444, 12, false) => PixelFormat::Yuv444P12Le,
                (Yuv444, 16, false) => PixelFormat::Yuv444P16Le,
                (Yuv444, 8, true) => PixelFormat::Yuva444P,
                (Yuv444, 10, true) => PixelFormat::Yuva444P10Le,
                (Yuv444, 12, true) => PixelFormat::Yuva444P12Le,
                (Yuv444, 16, true) => PixelFormat::Yuva444P16Le,
                _ => return None,
            })
        }

        /// The layout of a framework pixel format, for the planar
        /// YCbCr / gray formats this crate consumes.
        pub fn from_core(f: PixelFormat) -> Option<Self> {
            use Chroma::*;
            let (chroma, bit_depth, has_alpha) = match f {
                PixelFormat::Gray8 => (Mono, 8, false),
                PixelFormat::Gray10Le => (Mono, 10, false),
                PixelFormat::Gray12Le => (Mono, 12, false),
                PixelFormat::Gray16Le => (Mono, 16, false),
                PixelFormat::Ya8 => (Mono, 8, true),
                PixelFormat::Yuv420P | PixelFormat::YuvJ420P => (Yuv420, 8, false),
                PixelFormat::Yuv420P10Le => (Yuv420, 10, false),
                PixelFormat::Yuv420P12Le => (Yuv420, 12, false),
                PixelFormat::Yuv420P16Le => (Yuv420, 16, false),
                PixelFormat::Yuva420P => (Yuv420, 8, true),
                PixelFormat::Yuva420P10Le => (Yuv420, 10, true),
                PixelFormat::Yuv422P | PixelFormat::YuvJ422P => (Yuv422, 8, false),
                PixelFormat::Yuv422P10Le => (Yuv422, 10, false),
                PixelFormat::Yuv422P12Le => (Yuv422, 12, false),
                PixelFormat::Yuv422P16Le => (Yuv422, 16, false),
                PixelFormat::Yuva422P => (Yuv422, 8, true),
                PixelFormat::Yuva422P10Le => (Yuv422, 10, true),
                PixelFormat::Yuva422P12Le => (Yuv422, 12, true),
                PixelFormat::Yuva422P16Le => (Yuv422, 16, true),
                PixelFormat::Yuv444P | PixelFormat::YuvJ444P => (Yuv444, 8, false),
                PixelFormat::Yuv444P10Le => (Yuv444, 10, false),
                PixelFormat::Yuv444P12Le => (Yuv444, 12, false),
                PixelFormat::Yuv444P16Le => (Yuv444, 16, false),
                PixelFormat::Yuva444P => (Yuv444, 8, true),
                PixelFormat::Yuva444P10Le => (Yuv444, 10, true),
                PixelFormat::Yuva444P12Le => (Yuv444, 12, true),
                PixelFormat::Yuva444P16Le => (Yuv444, 16, true),
                _ => return None,
            };
            Some(Self {
                chroma,
                bit_depth,
                has_alpha,
            })
        }
    }

    impl HeifFrame {
        /// Convert into a framework `VideoFrame` plus its pixel format.
        ///
        /// Layouts without a framework equivalent (gray + alpha above
        /// 8 bits, 4:2:0 12-bit + alpha) are promoted to the matching
        /// 4:4:4 alpha layout with neutral chroma so no information is
        /// lost.
        pub fn to_core(&self) -> Result<(VideoFrame, PixelFormat)> {
            if let Some(pf) = self.format.to_core() {
                let planes = self
                    .planes
                    .iter()
                    .map(|p| VideoPlane {
                        stride: p.stride,
                        data: p.data.clone(),
                    })
                    .collect();
                return Ok((VideoFrame { pts: None, planes }, pf));
            }
            let promoted = self.promote_to_444()?;
            let pf = promoted.format.to_core().ok_or_else(|| {
                HeifError::unsupported(format!("no framework layout for {:?}", self.format))
            })?;
            let planes = promoted
                .planes
                .into_iter()
                .map(|p| VideoPlane {
                    stride: p.stride,
                    data: p.data,
                })
                .collect();
            Ok((VideoFrame { pts: None, planes }, pf))
        }

        /// Build from a framework frame of known geometry / format.
        pub fn from_core(
            frame: &VideoFrame,
            width: u32,
            height: u32,
            pf: PixelFormat,
        ) -> Result<Self> {
            let format = HeifPixelFormat::from_core(pf).ok_or_else(|| {
                HeifError::unsupported(format!(
                    "framework pixel format {pf:?} is not planar YCbCr / gray"
                ))
            })?;
            let image_planes = frame.image_planes();
            if image_planes.len() < format.plane_count() {
                return Err(HeifError::invalid(format!(
                    "framework frame has {} planes, {pf:?} needs {}",
                    image_planes.len(),
                    format.plane_count()
                )));
            }
            let f = Self {
                width,
                height,
                format,
                planes: image_planes[..format.plane_count()]
                    .iter()
                    .map(|p| HeifPlane {
                        stride: p.stride,
                        data: p.data.clone(),
                    })
                    .collect(),
            };
            f.validate()?;
            Ok(f)
        }

        /// Promote to 4:4:4 (chroma replicated, or neutral chroma added
        /// for monochrome), keeping depth and alpha.
        pub fn promote_to_444(&self) -> Result<Self> {
            let fmt = HeifPixelFormat {
                chroma: Chroma::Yuv444,
                ..self.format
            };
            let mid = 1u16 << (self.format.bit_depth - 1);
            let mut out = HeifFrame::filled(self.width, self.height, fmt, mid)?;
            // Luma.
            for y in 0..self.height {
                let row = self.row(0, y).to_vec();
                let stride = out.planes[0].stride;
                out.planes[0].data[y as usize * stride..y as usize * stride + row.len()]
                    .copy_from_slice(&row);
            }
            if self.format.chroma != Chroma::Mono {
                let (sx, sy) = self.format.chroma.shift();
                for p in 1..3 {
                    for y in 0..self.height {
                        for x in 0..self.width {
                            let v = self.sample(p, x >> sx, y >> sy);
                            out.set_sample(p, x, y, v);
                        }
                    }
                }
            }
            if let Some(a) = self.format.alpha_plane() {
                let dst = out.format.alpha_plane().expect("alpha kept");
                out.planes[dst] = self.planes[a].clone();
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(chroma: Chroma, depth: u8, alpha: bool) -> HeifPixelFormat {
        HeifPixelFormat::new(chroma, depth, alpha).unwrap()
    }

    #[test]
    fn plane_geometry() {
        let f = fmt(Chroma::Yuv420, 8, true);
        assert_eq!(f.plane_count(), 4);
        assert_eq!(f.alpha_plane(), Some(3));
        assert_eq!(f.plane_dims(1, 5, 3), (3, 2));
        assert_eq!(f.plane_dims(3, 5, 3), (5, 3));
        let g = fmt(Chroma::Mono, 10, false);
        assert_eq!(g.plane_count(), 1);
        assert_eq!(g.bytes_per_sample(), 2);
        assert_eq!(g.max_value(), 1023);
        assert_eq!(
            fmt(Chroma::Yuv422, 8, false).chroma.chroma_dims(5, 3),
            (3, 3)
        );
        assert!(HeifPixelFormat::new(Chroma::Mono, 7, false).is_err());
        assert_eq!(Chroma::from_idc(2), Some(Chroma::Yuv422));
        assert_eq!(Chroma::from_idc(4), None);
    }

    #[test]
    fn filled_sample_roundtrip_16bit() {
        let mut f = HeifFrame::filled(4, 2, fmt(Chroma::Yuv444, 10, false), 512).unwrap();
        f.validate().unwrap();
        assert_eq!(f.sample(2, 3, 1), 512);
        f.set_sample(2, 3, 1, 1023);
        assert_eq!(f.sample(2, 3, 1), 1023);
        assert_eq!(f.row(0, 1).len(), 8);
        let t = f.tight();
        assert_eq!(t, f);
        assert!(HeifFrame::zeroed(0, 1, f.format).is_err());
    }

    #[test]
    fn alpha_attach_and_detach() {
        let base = HeifFrame::filled(2, 2, fmt(Chroma::Yuv420, 8, false), 7).unwrap();
        let alpha = HeifFrame::filled(2, 2, fmt(Chroma::Mono, 8, false), 200).unwrap();
        let with = base.with_alpha_plane(&alpha).unwrap();
        assert!(with.format.has_alpha);
        assert_eq!(with.sample(3, 1, 1), 200);
        assert_eq!(with.without_alpha(), base);
        assert_eq!(with.alpha_as_frame().unwrap(), alpha);
        let bad = HeifFrame::filled(3, 2, fmt(Chroma::Mono, 8, false), 0).unwrap();
        assert!(base.with_alpha_plane(&bad).is_err());
        let deep = HeifFrame::filled(2, 2, fmt(Chroma::Mono, 10, false), 0).unwrap();
        assert!(base.with_alpha_plane(&deep).is_err());
    }

    #[test]
    fn validate_catches_short_planes() {
        let mut f = HeifFrame::zeroed(4, 4, fmt(Chroma::Yuv420, 8, false)).unwrap();
        f.planes[1].data.truncate(1);
        assert!(f.validate().is_err());
        f.planes.pop();
        assert!(f.validate().is_err());
    }

    #[cfg(feature = "registry")]
    #[test]
    fn core_bridge_round_trip_and_promotion() {
        use oxideav_core::PixelFormat;
        let f = HeifFrame::filled(4, 2, fmt(Chroma::Yuv420, 10, false), 300).unwrap();
        let (vf, pf) = f.to_core().unwrap();
        assert_eq!(pf, PixelFormat::Yuv420P10Le);
        let back = HeifFrame::from_core(&vf, 4, 2, pf).unwrap();
        assert_eq!(back, f);
        // Gray + alpha at 10 bits has no framework layout: promoted.
        let g = HeifFrame::filled(2, 2, fmt(Chroma::Mono, 10, true), 1000).unwrap();
        let (vf, pf) = g.to_core().unwrap();
        assert_eq!(pf, PixelFormat::Yuva444P10Le);
        assert_eq!(vf.planes.len(), 4);
        let back = HeifFrame::from_core(&vf, 2, 2, pf).unwrap();
        assert_eq!(back.sample(0, 1, 1), 1000);
        assert_eq!(back.sample(1, 1, 1), 512, "neutral chroma");
        assert_eq!(back.sample(3, 0, 0), 1000, "alpha kept");
        assert!(HeifPixelFormat::from_core(PixelFormat::Rgb24).is_none());
    }
}
