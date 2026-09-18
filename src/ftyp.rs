//! File type box (`ftyp`, ISO/IEC 14496-12 §4.3) and the HEIF / MIAF /
//! AVIF brand vocabulary (ISO/IEC 23008-12 §10 + Annex B.4, ISO/IEC
//! 23000-22 §10 + Annex A, AV1-ISOBMFF / AVIF brands).
//!
//! `styp` (segment type, §8.16.2) has the same body layout and is
//! accepted by the same parser for fragmented image sequences.

use crate::boxes::{fourcc_str, FourCc, Reader};
use crate::error::{HeifError, Result};

// ───────── structural brands (HEIF §10) ─────────
/// HEIF image / image collection structural brand (§10.2.2).
pub const BRAND_MIF1: FourCc = *b"mif1";
/// HEIF structural brand requiring `mif1` + `altr` / `prem` / CICP alpha (§10.2.3).
pub const BRAND_MIF2: FourCc = *b"mif2";
/// HEIF image sequence structural brand (§10.3.1).
pub const BRAND_MSF1: FourCc = *b"msf1";
/// Predictively coded image items brand (§10.2.4).
pub const BRAND_PRED: FourCc = *b"pred";
/// Single intra-coded picture brand (§10.2.5).
pub const BRAND_1PIC: FourCc = *b"1pic";
// ───────── HEVC brands (HEIF Annex B.4) ─────────
/// HEVC Main / Main Still Picture image item brand (B.4.1).
pub const BRAND_HEIC: FourCc = *b"heic";
/// HEVC Main 10 / RExt image item brand (B.4.1).
pub const BRAND_HEIX: FourCc = *b"heix";
/// HEVC Main image sequence brand (B.4.2).
pub const BRAND_HEVC: FourCc = *b"hevc";
/// HEVC Main 10 / RExt image sequence brand (B.4.2).
pub const BRAND_HEVX: FourCc = *b"hevx";
/// L-HEVC image item brands (B.4.3).
pub const BRAND_HEIM: FourCc = *b"heim";
/// L-HEVC image item brand, scalable (B.4.3).
pub const BRAND_HEIS: FourCc = *b"heis";
/// L-HEVC image sequence brands (B.4.4).
pub const BRAND_HEVM: FourCc = *b"hevm";
/// L-HEVC image sequence brand, scalable (B.4.4).
pub const BRAND_HEVS: FourCc = *b"hevs";
// ───────── MIAF brands (23000-22) ─────────
/// MIAF conformance brand (§10.1).
pub const BRAND_MIAF: FourCc = *b"miaf";
/// MIAF HEVC Basic profile (Annex A.3).
pub const BRAND_MIHB: FourCc = *b"MiHB";
/// MIAF HEVC Advanced profile (Annex A.4).
pub const BRAND_MIHA: FourCc = *b"MiHA";
/// MIAF HEVC Extended profile (Annex A.5).
pub const BRAND_MIHE: FourCc = *b"MiHE";
/// MIAF AVC Basic profile (Annex A.6).
pub const BRAND_MIAB: FourCc = *b"MiAB";
/// MIAF progressive application brand (§10.2).
pub const BRAND_MIPR: FourCc = *b"MiPr";
/// MIAF animation application brand (§10.3).
pub const BRAND_MIAN: FourCc = *b"MiAn";
/// MIAF burst capture application brand (§10.4).
pub const BRAND_MIBU: FourCc = *b"MiBu";
/// MIAF common media fragmented brand (§10.6).
pub const BRAND_MICM: FourCc = *b"MiCm";
// ───────── AV1 family (AVIF) ─────────
/// AVIF image brand.
pub const BRAND_AVIF: FourCc = *b"avif";
/// AVIF image sequence brand.
pub const BRAND_AVIS: FourCc = *b"avis";
/// AVIF intra-only sequence brand.
pub const BRAND_AVIO: FourCc = *b"avio";
/// AVIF MIAF AV1 Basic profile.
pub const BRAND_MA1B: FourCc = *b"MA1B";
/// AVIF MIAF AV1 Advanced profile.
pub const BRAND_MA1A: FourCc = *b"MA1A";
// ───────── ISOBMFF brands that co-occur ─────────
/// ISOBMFF `iso8` (required alongside `msf1`).
pub const BRAND_ISO8: FourCc = *b"iso8";
/// JPEG image item brand (HEIF Annex H).
pub const BRAND_JPEG: FourCc = *b"jpeg";
/// JPEG image sequence brand (HEIF Annex H).
pub const BRAND_JPGS: FourCc = *b"jpgs";
/// AVC image item brand (HEIF Annex E).
pub const BRAND_AVCI: FourCc = *b"avci";
/// AVC image sequence brand (HEIF Annex E).
pub const BRAND_AVCS: FourCc = *b"avcs";

/// Brands that identify a file as one this crate owns (HEIF structural,
/// HEVC-specific, MIAF, AV1/AVIF). Presence of any of them as the major
/// brand or among the compatible brands makes the container probe fire.
pub const HEIF_FAMILY_BRANDS: &[FourCc] = &[
    BRAND_MIF1, BRAND_MIF2, BRAND_MSF1, BRAND_HEIC, BRAND_HEIX, BRAND_HEVC, BRAND_HEVX, BRAND_HEIM,
    BRAND_HEIS, BRAND_HEVM, BRAND_HEVS, BRAND_MIAF, BRAND_MIHB, BRAND_MIHA, BRAND_MIHE, BRAND_MIAB,
    BRAND_AVIF, BRAND_AVIS, BRAND_AVIO, BRAND_MA1B, BRAND_MA1A, BRAND_JPEG, BRAND_JPGS, BRAND_AVCI,
    BRAND_AVCS, BRAND_1PIC, BRAND_PRED,
];

/// Parsed `ftyp` / `styp` box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileType {
    /// The box type that carried the brands (`ftyp` or `styp`).
    pub box_type: FourCc,
    /// `major_brand`.
    pub major_brand: FourCc,
    /// `minor_version` (HEIF §10.1: written as 0, ignored by readers).
    pub minor_version: u32,
    /// `compatible_brands[]`, in file order.
    pub compatible_brands: Vec<FourCc>,
}

impl FileType {
    /// Parse the payload of an `ftyp` / `styp` box.
    pub fn parse(box_type: FourCc, payload: &[u8]) -> Result<Self> {
        let mut r = Reader::new(payload);
        let major_brand = r.fourcc("ftyp major_brand")?;
        let minor_version = r.u32("ftyp minor_version")?;
        let mut compatible_brands = Vec::with_capacity(r.remaining() / 4);
        while r.remaining() >= 4 {
            compatible_brands.push(r.fourcc("ftyp compatible_brand")?);
        }
        if r.remaining() != 0 {
            return Err(HeifError::invalid(format!(
                "{} body has {} trailing bytes (brands are 4-byte aligned)",
                fourcc_str(&box_type),
                r.remaining()
            )));
        }
        Ok(Self {
            box_type,
            major_brand,
            minor_version,
            compatible_brands,
        })
    }

    /// `true` when `brand` is the major brand or listed as compatible.
    pub fn has_brand(&self, brand: &FourCc) -> bool {
        &self.major_brand == brand || self.compatible_brands.contains(brand)
    }

    /// Every brand (major first, then compatible, de-duplicated).
    pub fn all_brands(&self) -> Vec<FourCc> {
        let mut v = vec![self.major_brand];
        for b in &self.compatible_brands {
            if !v.contains(b) {
                v.push(*b);
            }
        }
        v
    }

    /// `true` when any HEIF-family brand is declared.
    pub fn is_heif_family(&self) -> bool {
        self.all_brands()
            .iter()
            .any(|b| HEIF_FAMILY_BRANDS.contains(b))
    }

    /// Structural classification of the declared brands.
    pub fn classify(&self) -> BrandClass {
        let brands = self.all_brands();
        let has = |b: &FourCc| brands.contains(b);
        BrandClass {
            image_collection: has(&BRAND_MIF1)
                || has(&BRAND_MIF2)
                || has(&BRAND_HEIC)
                || has(&BRAND_HEIX)
                || has(&BRAND_HEIM)
                || has(&BRAND_HEIS)
                || has(&BRAND_AVIF)
                || has(&BRAND_JPEG)
                || has(&BRAND_AVCI)
                || has(&BRAND_MIAF),
            image_sequence: has(&BRAND_MSF1)
                || has(&BRAND_HEVC)
                || has(&BRAND_HEVX)
                || has(&BRAND_HEVM)
                || has(&BRAND_HEVS)
                || has(&BRAND_AVIS)
                || has(&BRAND_AVIO)
                || has(&BRAND_JPGS)
                || has(&BRAND_AVCS),
            miaf: has(&BRAND_MIAF)
                || has(&BRAND_MIHB)
                || has(&BRAND_MIHA)
                || has(&BRAND_MIHE)
                || has(&BRAND_MIAB)
                || has(&BRAND_MA1B)
                || has(&BRAND_MA1A),
            hevc: has(&BRAND_HEIC)
                || has(&BRAND_HEIX)
                || has(&BRAND_HEVC)
                || has(&BRAND_HEVX)
                || has(&BRAND_HEIM)
                || has(&BRAND_HEIS)
                || has(&BRAND_HEVM)
                || has(&BRAND_HEVS)
                || has(&BRAND_MIHB)
                || has(&BRAND_MIHA)
                || has(&BRAND_MIHE),
            av1: has(&BRAND_AVIF)
                || has(&BRAND_AVIS)
                || has(&BRAND_AVIO)
                || has(&BRAND_MA1B)
                || has(&BRAND_MA1A),
            predictive_items: has(&BRAND_PRED),
        }
    }

    /// Serialize as an `ftyp` (or `styp`) box.
    pub fn to_box(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(8 + 4 * self.compatible_brands.len());
        body.extend_from_slice(&self.major_brand);
        body.extend_from_slice(&self.minor_version.to_be_bytes());
        for b in &self.compatible_brands {
            body.extend_from_slice(b);
        }
        crate::boxes::write::boxed(&self.box_type, &body)
    }
}

/// What the declared brands say about the file layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BrandClass {
    /// `mif1` / `heic` / `avif` / … — a `meta`-box image collection.
    pub image_collection: bool,
    /// `msf1` / `hevc` / `avis` / … — a `moov` image sequence.
    pub image_sequence: bool,
    /// Any MIAF brand or profile brand.
    pub miaf: bool,
    /// Any HEVC codec brand.
    pub hevc: bool,
    /// Any AV1 codec brand.
    pub av1: bool,
    /// `pred` (predictively coded image items) declared.
    pub predictive_items: bool,
}

/// Content probe over the first bytes of a file: `100` when an `ftyp`
/// at offset 0 declares a HEIF-family brand, `0` otherwise. Files with
/// only generic ISOBMFF / QuickTime brands (`isom`, `mp42`, `qt  `, …)
/// score 0 so the MP4 / MOV demuxers keep them.
pub fn probe_score(buf: &[u8]) -> u8 {
    if buf.len() < 16 || &buf[4..8] != b"ftyp" {
        return 0;
    }
    let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if size < 16 {
        return 0;
    }
    let end = size.min(buf.len());
    let body = &buf[8..end];
    let n = body.len() / 4;
    for i in 0..n {
        if i == 1 {
            continue; // minor_version
        }
        let b: FourCc = [
            body[i * 4],
            body[i * 4 + 1],
            body[i * 4 + 2],
            body[i * 4 + 3],
        ];
        if HEIF_FAMILY_BRANDS.contains(&b) {
            return 100;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let ft = FileType {
            box_type: *b"ftyp",
            major_brand: *major,
            minor_version: 0,
            compatible_brands: compat.iter().map(|b| **b).collect(),
        };
        ft.to_box()
    }

    #[test]
    fn parse_round_trip_and_classify() {
        let bytes = ftyp(b"heic", &[b"mif1", b"heic", b"miaf"]);
        let h = crate::boxes::parse_box_header(&bytes, 0).unwrap();
        let ft = FileType::parse(h.box_type, crate::boxes::payload(&bytes, &h)).unwrap();
        assert_eq!(ft.major_brand, BRAND_HEIC);
        assert_eq!(ft.compatible_brands.len(), 3);
        assert!(ft.has_brand(&BRAND_MIAF));
        assert!(ft.is_heif_family());
        let c = ft.classify();
        assert!(c.image_collection && c.miaf && c.hevc);
        assert!(!c.image_sequence && !c.av1);
        assert_eq!(ft.to_box(), bytes);
        assert_eq!(ft.all_brands(), vec![BRAND_HEIC, BRAND_MIF1, BRAND_MIAF]);
    }

    #[test]
    fn sequence_brands_classify() {
        let bytes = ftyp(
            b"hevc",
            &[b"mif1", b"heic", b"miaf", b"msf1", b"hevc", b"iso8"],
        );
        let h = crate::boxes::parse_box_header(&bytes, 0).unwrap();
        let ft = FileType::parse(h.box_type, crate::boxes::payload(&bytes, &h)).unwrap();
        let c = ft.classify();
        assert!(c.image_collection && c.image_sequence && c.hevc);
    }

    #[test]
    fn probe_scores() {
        assert_eq!(probe_score(&ftyp(b"heic", &[b"mif1"])), 100);
        assert_eq!(probe_score(&ftyp(b"mif1", &[b"miaf"])), 100);
        assert_eq!(probe_score(&ftyp(b"avif", &[b"mif1"])), 100);
        assert_eq!(probe_score(&ftyp(b"isom", &[b"mp42"])), 0);
        assert_eq!(probe_score(&ftyp(b"qt  ", &[])), 0);
        // A HEIF brand among the compatible brands of a generic major
        // brand still counts (some writers put `isom` first).
        assert_eq!(probe_score(&ftyp(b"isom", &[b"mif1", b"heic"])), 100);
        assert_eq!(probe_score(b"short"), 0);
        // Only the first 16 bytes of a 16-byte ftyp: major brand is heic.
        let b = ftyp(b"heic", &[]);
        assert_eq!(b.len(), 16);
        assert_eq!(probe_score(&b), 100);
    }

    #[test]
    fn rejects_misaligned_body() {
        let mut bytes = ftyp(b"heic", &[b"mif1"]);
        bytes[3] += 1;
        bytes.push(0);
        let h = crate::boxes::parse_box_header(&bytes, 0).unwrap();
        assert!(FileType::parse(h.box_type, crate::boxes::payload(&bytes, &h)).is_err());
    }
}
