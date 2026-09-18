//! MIAF (ISO/IEC 23000-22) constraints as typed checks.
//!
//! [`check`] walks a parsed file and reports every violation of the
//! `miaf` brand's general requirements (§7) plus the shared conditions
//! (§8) and the codec constraints of the profile being checked
//! (Annex A). Checks are structural — they never decode pixels — and
//! are the same on the standalone and registry builds.

use crate::boxes::{fourcc_str, FourCc};
use crate::derived::{build_graph, ImageKind, ImageNode};
use crate::error::Result;
use crate::file::HeifFile;
use crate::ftyp::{
    BRAND_MA1A, BRAND_MA1B, BRAND_MIAB, BRAND_MIAF, BRAND_MIF1, BRAND_MIHA, BRAND_MIHB, BRAND_MIHE,
    BRAND_MSF1,
};
use crate::meta::{reference, ITEM_TYPE_AV01, ITEM_TYPE_HVC1, ITEM_TYPE_IDEN};
use crate::props::{AuxKind, Colr, Property};

/// A MIAF profile (Annex A) or the plain `miaf` brand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MiafProfile {
    /// The `miaf` brand: §7 general requirements only.
    Miaf,
    /// `MiHB` — HEVC Main / Main Still Picture, 4:2:0 8-bit (A.3).
    HevcBasic,
    /// `MiHA` — adds Main 10 / Main Intra / Main 4:2:2 10 Intra (A.4).
    HevcAdvanced,
    /// `MiHE` — adds Main 4:4:4 (10) / Monochrome (10) (A.5).
    HevcExtended,
    /// `MiAB` — AVC Basic (A.6); AVC items are not decodable here.
    AvcBasic,
    /// `MA1B` — AV1 Basic (AVIF §7.2).
    Av1Basic,
    /// `MA1A` — AV1 Advanced (AVIF §7.3).
    Av1Advanced,
}

impl MiafProfile {
    /// The brand that declares this profile.
    pub fn brand(&self) -> FourCc {
        match self {
            MiafProfile::Miaf => BRAND_MIAF,
            MiafProfile::HevcBasic => BRAND_MIHB,
            MiafProfile::HevcAdvanced => BRAND_MIHA,
            MiafProfile::HevcExtended => BRAND_MIHE,
            MiafProfile::AvcBasic => BRAND_MIAB,
            MiafProfile::Av1Basic => BRAND_MA1B,
            MiafProfile::Av1Advanced => BRAND_MA1A,
        }
    }

    /// Every profile a file declares through its brands.
    pub fn declared_by(file: &HeifFile) -> Vec<MiafProfile> {
        [
            MiafProfile::Miaf,
            MiafProfile::HevcBasic,
            MiafProfile::HevcAdvanced,
            MiafProfile::HevcExtended,
            MiafProfile::AvcBasic,
            MiafProfile::Av1Basic,
            MiafProfile::Av1Advanced,
        ]
        .into_iter()
        .filter(|p| file.file_type.has_brand(&p.brand()))
        .collect()
    }

    /// `true` when the profile adopts the Annex A "shared constraints"
    /// (self-containment, single-layer, grid-limit, single-track,
    /// matched-duration).
    pub fn adopts_shared_constraints(&self) -> bool {
        !matches!(self, MiafProfile::Miaf)
    }
}

/// One violated requirement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MiafViolation {
    /// The clause the requirement comes from (`"7.2.1.7"`, `"A.3.2"`, …).
    pub clause: &'static str,
    /// The item the violation concerns, when item-specific.
    pub item_id: Option<u32>,
    /// What went wrong.
    pub message: String,
}

/// Result of a MIAF check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MiafReport {
    /// The violations found, in discovery order.
    pub violations: Vec<MiafViolation>,
}

impl MiafReport {
    /// `true` when nothing was violated.
    pub fn is_conformant(&self) -> bool {
        self.violations.is_empty()
    }

    fn push(&mut self, clause: &'static str, item_id: Option<u32>, message: impl Into<String>) {
        self.violations.push(MiafViolation {
            clause,
            item_id,
            message: message.into(),
        });
    }
}

/// MIAF §8.4 grid-limit pixel budget.
pub const GRID_LIMIT_PIXELS: u64 = 128_000_000;
/// MIAF §7.3.3 / §8.4 maximum size factor between successive images.
pub const SIZE_FACTOR_LIMIT: u64 = 200;
/// MIAF §7.3.11.4.2 minimum tile edge.
pub const MIN_TILE_EDGE: u32 = 64;

/// Check `file` against the general MIAF requirements and, when
/// `profile` is a codec profile, its Annex A constraints.
pub fn check(file: &HeifFile, profile: MiafProfile) -> Result<MiafReport> {
    let mut rep = MiafReport::default();
    let meta = match &file.meta {
        Some(m) => m,
        None => {
            rep.push("7.2.1.4", None, "file-level MetaBox is missing");
            return Ok(rep);
        }
    };
    // §7.2.1.1: mif1 shall be present; msf1 when a sequence track exists.
    if !file.file_type.has_brand(&BRAND_MIF1) {
        rep.push("7.2.1.1", None, "compatible brands lack 'mif1'");
    }
    if file.has_moov() && !file.file_type.has_brand(&BRAND_MSF1) {
        rep.push(
            "7.2.1.1",
            None,
            "file carries a moov box but the brands lack 'msf1'",
        );
    }
    // §7.2.1.1: hdlr first in meta; §7.2.1.5: 'pict'.
    match meta.child_box_types.first() {
        Some(b"hdlr") => {}
        _ => rep.push(
            "7.2.1.1",
            None,
            "HandlerBox is not the first box in MetaBox",
        ),
    }
    match &meta.handler {
        Some(h) if h.handler_type == *b"pict" => {}
        Some(h) => rep.push(
            "7.2.1.5",
            None,
            format!(
                "handler_type '{}' (shall be 'pict')",
                fourcc_str(&h.handler_type)
            ),
        ),
        None => rep.push("7.2.1.5", None, "MetaBox has no HandlerBox"),
    }
    // §7.2.1.4: no xml/bxml in meta.
    if meta
        .child_box_types
        .iter()
        .any(|t| t == b"xml " || t == b"bxml")
    {
        rep.push("7.2.1.4", None, "XMLBox / BinaryXMLBox present in MetaBox");
    }
    // §7.2.1.10 / §7.3.2: primary item exists and is a master image.
    let primary = match file.primary_item() {
        Ok(p) => p.clone(),
        Err(e) => {
            rep.push("7.2.1.10", None, e.to_string());
            return Ok(rep);
        }
    };
    if !primary.is_image() {
        rep.push(
            "7.3.2",
            Some(primary.id),
            format!(
                "primary item type '{}' is not an image item",
                fourcc_str(&primary.item_type)
            ),
        );
        return Ok(rep);
    }
    if primary.is_hidden() {
        rep.push("HEIF 6.4.2", Some(primary.id), "primary item is hidden");
    }
    if !meta
        .references_from(primary.id, &reference::THMB)
        .is_empty()
    {
        rep.push("7.3.2", Some(primary.id), "primary item is a thumbnail");
    }
    if !meta
        .references_from(primary.id, &reference::AUXL)
        .is_empty()
    {
        rep.push(
            "7.3.2",
            Some(primary.id),
            "primary item is an auxiliary image",
        );
    }
    // Per-item structural checks.
    for it in meta.items.iter().filter(|i| i.is_image()) {
        let id = it.id;
        match meta.location(id) {
            Some(loc) => {
                // §7.2.1.7 construction methods; §7.2.1.13 bodies in mdat.
                if it.is_coded_image() && loc.construction_method != 0 {
                    rep.push(
                        "7.2.1.7",
                        Some(id),
                        format!(
                            "coded image item uses construction_method {}",
                            loc.construction_method
                        ),
                    );
                }
                if it.is_derived_image() && loc.construction_method > 1 {
                    rep.push(
                        "7.2.1.7",
                        Some(id),
                        "derived image item uses construction_method 2",
                    );
                }
                if profile.adopts_shared_constraints() && loc.data_reference_index != 0 {
                    rep.push(
                        "8.2.1",
                        Some(id),
                        format!(
                            "data_reference_index {} (shall be 0)",
                            loc.data_reference_index
                        ),
                    );
                }
            }
            None if it.item_type != ITEM_TYPE_IDEN => {
                rep.push("HEIF 6.2", Some(id), "image item has no iloc entry");
            }
            None => {}
        }
        // §7.2.1.8: unprotected.
        if it.protection_index != 0 {
            rep.push(
                "7.2.1.8",
                Some(id),
                "MIAF image item references item protection",
            );
        }
        let props = match crate::props::ItemProperties::resolve(meta, id) {
            Ok(p) => p,
            Err(e) => {
                rep.push("7.3.6", Some(id), e.to_string());
                continue;
            }
        };
        // §7.3.6.3 ispe mandatory.
        if props.ispe().is_none() {
            rep.push("7.3.6.3", Some(id), "no ispe property");
        }
        // §7.3.6.5 square pixels.
        if let Some(p) = props.pasp() {
            if !p.is_square() {
                rep.push(
                    "7.3.6.5",
                    Some(id),
                    format!("pasp {}:{} (shall be 1:1)", p.h_spacing, p.v_spacing),
                );
            }
        }
        // §7.3.9 transformative: essential, from {clap, irot, imir}, ordered.
        let mut last_rank = 0u8;
        for e in props.transformative() {
            let rank = match e.property.box_type() {
                t if &t == b"clap" => 1,
                t if &t == b"irot" => 2,
                t if &t == b"imir" => 3,
                _ => 0,
            };
            if rank == 0 {
                rep.push(
                    "7.3.9",
                    Some(id),
                    format!(
                        "transformative property '{}' outside the permitted set",
                        fourcc_str(&e.property.box_type())
                    ),
                );
                continue;
            }
            if !e.essential {
                rep.push(
                    "7.3.9",
                    Some(id),
                    format!(
                        "transformative property '{}' is not marked essential",
                        fourcc_str(&e.property.box_type())
                    ),
                );
            }
            if rank <= last_rank {
                rep.push(
                    "7.3.6.7",
                    Some(id),
                    "transformative properties not in clap → irot → imir order",
                );
            }
            last_rank = rank;
        }
        // HEIF B.2.3.1 / §6.5.2: decoder configuration essential.
        for e in props.iter() {
            if e.property.is_decoder_config() && !e.essential {
                rep.push(
                    "HEIF 6.5.2",
                    Some(id),
                    format!(
                        "decoder configuration '{}' is not marked essential",
                        fourcc_str(&e.property.box_type())
                    ),
                );
            }
        }
        for t in props.unsupported_essential() {
            rep.push(
                "HEIF 10.2.1",
                Some(id),
                format!("essential property '{}' is not recognized", fourcc_str(&t)),
            );
        }
        // §7.3.5.2 alpha with CICP: full range.
        if let Some(a) = props.auxc() {
            if a.kind() == AuxKind::Alpha {
                if let Some(Colr::Nclx { full_range, .. }) = props.nclx() {
                    if !full_range {
                        rep.push(
                            "7.3.5.2",
                            Some(id),
                            "alpha auxiliary carries CICP with full_range_flag 0",
                        );
                    }
                }
            }
            if !meta.references_from(id, &reference::AUXL).is_empty()
                && meta.references_from(id, &reference::THMB).len() > 1
            {
                rep.push(
                    "HEIF 6.4.6",
                    Some(id),
                    "item is both auxiliary and thumbnail",
                );
            }
        }
        // §7.3.6.4 colr count / kinds; HEIF §6.5.5 pairing rule.
        let colrs = props.colrs();
        let nclx_count = colrs
            .iter()
            .filter(|c| matches!(c, Colr::Nclx { .. }))
            .count();
        let icc_count = colrs.iter().filter(|c| c.is_icc()).count();
        if nclx_count > 1 || icc_count > 1 || colrs.len() > 2 {
            rep.push(
                "HEIF 6.5.5",
                Some(id),
                format!(
                    "{} colr properties (at most one nclx and one ICC)",
                    colrs.len()
                ),
            );
        }
        if icc_count == 1 && nclx_count == 1 {
            if let Some(Colr::Nclx {
                primaries,
                transfer,
                ..
            }) = props.nclx()
            {
                if *primaries != 2 || *transfer != 2 {
                    rep.push(
                        "HEIF 6.5.5",
                        Some(id),
                        "nclx paired with an ICC profile shall use colour_primaries = transfer_characteristics = 2",
                    );
                }
            }
        }
        // Metadata bodies in mdat (§7.2.1.13).
        for m in meta.metadata_of(id) {
            if let Some(loc) = meta.location(m) {
                if loc.construction_method != 0 {
                    rep.push(
                        "7.2.1.13",
                        Some(m),
                        "Exif / XMP metadata item is not stored in a MediaDataBox",
                    );
                }
            }
        }
        // Codec profile constraints.
        check_codec(&mut rep, profile, it.item_type, &props, id);
    }
    // §7.3.3 thumbnail factor.
    for it in meta.items.iter().filter(|i| i.is_image()) {
        let thumbs = meta.thumbnails_of(it.id);
        if thumbs.is_empty() {
            continue;
        }
        let master_px = pixel_count(meta, it.id);
        let mut sizes: Vec<u64> = thumbs.iter().map(|t| pixel_count(meta, *t)).collect();
        sizes.sort_unstable();
        sizes.push(master_px);
        for w in sizes.windows(2) {
            if w[0] > 0 && w[1] > w[0].saturating_mul(SIZE_FACTOR_LIMIT) {
                rep.push(
                    "7.3.3",
                    Some(it.id),
                    format!(
                        "pixel count {} → {} exceeds the factor-{SIZE_FACTOR_LIMIT} thumbnail ladder",
                        w[0], w[1]
                    ),
                );
            }
        }
    }
    // Derivation chain checks (§7.3.11) on the primary item.
    match build_graph(file, primary.id) {
        Ok(root) => check_derivations(&mut rep, &root, profile),
        Err(e) => rep.push("7.3.11.1", Some(primary.id), e.to_string()),
    }
    Ok(rep)
}

fn pixel_count(meta: &crate::meta::Meta, id: u32) -> u64 {
    crate::props::ItemProperties::resolve(meta, id)
        .ok()
        .and_then(|p| p.ispe())
        .map(|i| i.width as u64 * i.height as u64)
        .unwrap_or(0)
}

fn check_codec(
    rep: &mut MiafReport,
    profile: MiafProfile,
    item_type: FourCc,
    props: &crate::props::ItemProperties,
    id: u32,
) {
    match profile {
        MiafProfile::HevcBasic | MiafProfile::HevcAdvanced | MiafProfile::HevcExtended => {
            if item_type != ITEM_TYPE_HVC1 {
                // Other coding formats may be present (§7.3.4) but the
                // profile's items are hvc1; nothing to check here.
                return;
            }
            let Some(h) = props.hvcc() else {
                rep.push("HEIF B.2.3.1", Some(id), "hvc1 item without hvcC");
                return;
            };
            // Level 6 = 180.
            if h.general_level_idc > 180 {
                rep.push(
                    "A.3.2",
                    Some(id),
                    format!("general_level_idc {} exceeds level 6", h.general_level_idc),
                );
            }
            if h.general_tier_flag {
                rep.push("A.3.2", Some(id), "High tier (Main tier required)");
            }
            let bd = h.bit_depth_luma().max(h.bit_depth_chroma());
            let (max_depth, chroma_ok): (u8, bool) = match profile {
                MiafProfile::HevcBasic => (8, h.chroma_format_idc == 1),
                MiafProfile::HevcAdvanced => (10, matches!(h.chroma_format_idc, 1 | 2)),
                _ => (10, true),
            };
            if bd > max_depth {
                rep.push(
                    "A.3.2",
                    Some(id),
                    format!("bit depth {bd} exceeds the profile's {max_depth}"),
                );
            }
            if !chroma_ok {
                rep.push(
                    "A.3.2",
                    Some(id),
                    format!(
                        "chroma format {} not permitted by the profile",
                        h.chroma_name()
                    ),
                );
            }
        }
        MiafProfile::Av1Basic | MiafProfile::Av1Advanced => {
            if item_type != ITEM_TYPE_AV01 {
                return;
            }
            let Some(a) = props.av1c() else {
                rep.push("AVIF 4.2", Some(id), "av01 item without av1C");
                return;
            };
            let max_profile = if profile == MiafProfile::Av1Basic {
                0
            } else {
                1
            };
            if a.seq_profile > max_profile {
                rep.push(
                    "AVIF 7",
                    Some(id),
                    format!(
                        "seq_profile {} exceeds the profile's {max_profile}",
                        a.seq_profile
                    ),
                );
            }
            let max_level = if profile == MiafProfile::Av1Basic {
                13
            } else {
                16
            };
            if a.seq_level_idx_0 > max_level {
                rep.push(
                    "AVIF 7",
                    Some(id),
                    format!("seq_level_idx_0 {} exceeds {max_level}", a.seq_level_idx_0),
                );
            }
        }
        MiafProfile::Miaf | MiafProfile::AvcBasic => {}
    }
}

fn check_derivations(rep: &mut MiafReport, root: &ImageNode, profile: MiafProfile) {
    // §7.3.11.1 chain order: coded → iden? → grid? → iden? → iovl? → iden?
    let chain = root.chain_types();
    let mut stage = 0u8; // 0 = at top (iovl allowed), climbing down.
    let mut prev_iden = false;
    for t in &chain {
        let (kind, allowed) = match *t {
            crate::meta::ITEM_TYPE_IDEN => ("iden", true),
            crate::meta::ITEM_TYPE_IOVL => ("iovl", stage == 0),
            crate::meta::ITEM_TYPE_GRID => ("grid", stage <= 1),
            _ => ("coded", true),
        };
        if kind == "iden" && prev_iden {
            rep.push(
                "7.3.11.2",
                Some(root.item.id),
                "identity derivation derived from an identity derivation",
            );
        }
        prev_iden = kind == "iden";
        if !allowed {
            rep.push(
                "7.3.11.1",
                Some(root.item.id),
                format!(
                    "derivation chain {} is not in the permitted order",
                    chain.iter().map(fourcc_str).collect::<Vec<_>>().join(" → ")
                ),
            );
            break;
        }
        match kind {
            "iovl" => stage = 1,
            "grid" => stage = 2,
            _ => {}
        }
    }
    walk(rep, root, profile);
}

fn walk(rep: &mut MiafReport, node: &ImageNode, profile: MiafProfile) {
    let id = node.item.id;
    match &node.kind {
        ImageKind::Grid(g) => {
            // §7.3.11.4: same coding format + config across tiles; tile size rules.
            let mut first_cfg: Option<(FourCc, Vec<u8>, u8)> = None;
            let mut total_px: u64 = 0;
            for tile in &node.inputs {
                let cfg = tile.properties.iter().find_map(|e| match &e.property {
                    Property::HvcC(h) => {
                        Some((tile.item.item_type, h.raw.clone(), h.chroma_format_idc))
                    }
                    Property::Av1C(a) => {
                        Some((tile.item.item_type, a.raw.clone(), a.chroma_format_idc()))
                    }
                    _ => None,
                });
                if let Some(c) = cfg {
                    match &first_cfg {
                        None => first_cfg = Some(c),
                        Some(f) if f.0 != c.0 || f.1 != c.1 => {
                            rep.push(
                                "7.3.11.4.1",
                                Some(id),
                                format!(
                                    "grid tile {} differs in coding format / decoder configuration",
                                    tile.item.id
                                ),
                            );
                        }
                        _ => {}
                    }
                }
                if let Some((w, h)) = tile.ispe() {
                    total_px = total_px.saturating_add(w as u64 * h as u64);
                }
            }
            if let Some(first) = node.inputs.first() {
                if let Ok((tw, th)) = first.output_size() {
                    if tw < MIN_TILE_EDGE || th < MIN_TILE_EDGE {
                        rep.push(
                            "7.3.11.4.2",
                            Some(id),
                            format!("tile {tw}x{th} smaller than {MIN_TILE_EDGE} pixels"),
                        );
                    }
                    let chroma = first_cfg.as_ref().map(|c| c.2).unwrap_or(1);
                    let (need_even_w, need_even_h) = match chroma {
                        1 => (true, true),
                        2 => (true, false),
                        _ => (false, false),
                    };
                    if (need_even_w && (tw % 2 == 1 || g.output_width % 2 == 1))
                        || (need_even_h && (th % 2 == 1 || g.output_height % 2 == 1))
                    {
                        rep.push(
                            "7.3.11.4.2",
                            Some(id),
                            "tile or output size is odd for the chroma sampling format",
                        );
                    }
                    if (tw as u64) * (g.columns as u64) < g.output_width as u64
                        || (th as u64) * (g.rows as u64) < g.output_height as u64
                    {
                        rep.push(
                            "HEIF 6.6.2.3.1",
                            Some(id),
                            "tiles do not cover the declared output canvas",
                        );
                    }
                }
                for tile in &node.inputs {
                    if tile.output_size().ok() != first.output_size().ok() {
                        rep.push(
                            "HEIF 6.6.2.3.1",
                            Some(id),
                            format!("tile {} size differs from the first tile", tile.item.id),
                        );
                    }
                    if tile.item.item_type == ITEM_TYPE_IDEN
                        && tile
                            .inputs
                            .first()
                            .map(|i| !matches!(i.kind, ImageKind::Coded(_)))
                            .unwrap_or(true)
                    {
                        rep.push(
                            "7.3.11.4.1",
                            Some(id),
                            "grid of iden items whose iden does not refer directly to a coded image",
                        );
                    }
                }
            }
            if profile.adopts_shared_constraints() && total_px > GRID_LIMIT_PIXELS {
                rep.push(
                    "8.4",
                    Some(id),
                    format!("grid inputs total {total_px} pixels, over the {GRID_LIMIT_PIXELS} grid-limit"),
                );
            }
        }
        ImageKind::Overlay(_) => {
            // §7.3.11.3: inputs share bit depth and explicit colour info.
            let depths: Vec<u8> = node
                .inputs
                .iter()
                .filter_map(|i| {
                    i.properties
                        .hvcc()
                        .map(|h| h.bit_depth_luma())
                        .or_else(|| i.properties.av1c().map(|a| a.bit_depth()))
                })
                .collect();
            if depths.windows(2).any(|w| w[0] != w[1]) {
                rep.push("7.3.11.3", Some(id), "overlay inputs differ in bit depth");
            }
            let nclx: Vec<Option<&Colr>> =
                node.inputs.iter().map(|i| i.properties.nclx()).collect();
            if nclx.windows(2).any(|w| w[0] != w[1]) {
                rep.push(
                    "7.3.11.3",
                    Some(id),
                    "overlay inputs differ in explicit colour information",
                );
            }
            let total_px: u64 = node
                .inputs
                .iter()
                .filter_map(|i| i.ispe())
                .map(|(w, h)| w as u64 * h as u64)
                .sum();
            if profile.adopts_shared_constraints() && total_px > GRID_LIMIT_PIXELS {
                rep.push(
                    "8.4",
                    Some(id),
                    format!("overlay inputs total {total_px} pixels, over the {GRID_LIMIT_PIXELS} grid-limit"),
                );
            }
        }
        ImageKind::Identity => {
            if node
                .inputs
                .first()
                .map(|i| i.item.item_type == ITEM_TYPE_IDEN)
                .unwrap_or(false)
            {
                rep.push(
                    "7.3.11.2",
                    Some(id),
                    "iden item derived from another iden item",
                );
            }
        }
        ImageKind::Coded(_) | ImageKind::ToneMap(_) => {}
    }
    for i in &node.inputs {
        walk(rep, i, profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_map_to_brands() {
        assert_eq!(MiafProfile::HevcBasic.brand(), *b"MiHB");
        assert!(MiafProfile::HevcBasic.adopts_shared_constraints());
        assert!(!MiafProfile::Miaf.adopts_shared_constraints());
    }
}
