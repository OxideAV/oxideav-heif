//! Whole-file view: `ftyp`, the top-level boxes, the file-level `meta`
//! tree and item payload resolution across all three `iloc`
//! construction methods (ISO/IEC 14496-12 §8.11.3).

use std::borrow::Cow;

use crate::boxes::{fourcc_str, iter_boxes, payload, walk_boxes, BoxHeader, FourCc, WalkEntry};
use crate::error::{HeifError, Result};
use crate::ftyp::FileType;
use crate::meta::{reference, ItemInfo, ItemLocation, Meta};

/// Upper bound on the concatenated size of one item's data.
pub const MAX_ITEM_BYTES: u64 = 1 << 30;
/// Maximum construction-method-2 indirection depth.
pub const MAX_ILOC_DEPTH: usize = 8;
/// Box-tree walk limits used by [`HeifFile::box_walk`].
pub const MAX_WALK_DEPTH: usize = 32;
/// Maximum number of boxes [`HeifFile::box_walk`] enumerates.
pub const MAX_WALK_BOXES: usize = 1 << 20;

/// A parsed HEIF / HEIC / MIAF / AVIF file.
///
/// The bytes are borrowed when the file is opened with
/// [`HeifFile::parse`] and owned after [`HeifFile::from_vec`] /
/// [`HeifFile::into_owned`]; item payloads are borrowed from them
/// wherever an item is one contiguous span, so a decode never copies
/// the input.
#[derive(Clone, Debug)]
pub struct HeifFile<'a> {
    data: Cow<'a, [u8]>,
    /// The `ftyp` (or `styp`) box.
    pub file_type: FileType,
    /// The file-level `meta` box, when present.
    pub meta: Option<Meta>,
    /// Headers of every top-level box, in file order.
    pub top_level: Vec<BoxHeader>,
}

impl<'a> HeifFile<'a> {
    /// Parse a file held in memory, borrowing the bytes (no copy).
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        Self::from_cow(Cow::Borrowed(bytes))
    }

    /// Parse a file, taking ownership of its bytes.
    pub fn from_vec(data: Vec<u8>) -> Result<HeifFile<'static>> {
        HeifFile::from_cow(Cow::Owned(data))
    }

    /// Parse a file from borrowed or owned bytes.
    pub fn from_cow(data: Cow<'a, [u8]>) -> Result<Self> {
        let mut top_level = Vec::new();
        let mut file_type = None;
        let mut meta = None;
        for h in iter_boxes(&data) {
            let h = h?;
            let p = payload(&data, &h);
            match &h.box_type {
                b"ftyp" | b"styp" => {
                    if file_type.is_none() {
                        file_type = Some(FileType::parse(h.box_type, p)?);
                    }
                }
                b"meta" if meta.is_none() => meta = Some(Meta::parse(p)?),
                _ => {}
            }
            top_level.push(h);
        }
        let file_type = file_type
            .ok_or_else(|| HeifError::invalid("no ftyp box (ISO/IEC 23008-12 §10 requires one)"))?;
        if let Some(first) = top_level.first() {
            if !matches!(&first.box_type, b"ftyp" | b"styp") {
                return Err(HeifError::invalid(format!(
                    "first box is '{}' — ftyp shall be first (ISO/IEC 14496-12 §4.3)",
                    fourcc_str(&first.box_type)
                )));
            }
        }
        Ok(Self {
            data,
            file_type,
            meta,
            top_level,
        })
    }

    /// The file bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consume the view and return the file bytes (copied only when
    /// they were borrowed).
    pub fn into_bytes(self) -> Vec<u8> {
        self.data.into_owned()
    }

    /// Detach from the borrowed input (copies borrowed bytes once; a
    /// no-op for an owned file).
    pub fn into_owned(self) -> HeifFile<'static> {
        HeifFile {
            data: Cow::Owned(self.data.into_owned()),
            file_type: self.file_type,
            meta: self.meta,
            top_level: self.top_level,
        }
    }

    /// `true` when the bytes are owned by this view.
    pub fn is_owned(&self) -> bool {
        matches!(self.data, Cow::Owned(_))
    }

    /// `true` when a top-level `moov` box exists (image sequence / video).
    pub fn has_moov(&self) -> bool {
        self.top_level.iter().any(|h| &h.box_type == b"moov")
    }

    /// Payload of the first top-level box of `box_type`.
    pub fn top_level_payload(&self, box_type: &FourCc) -> Option<&[u8]> {
        self.top_level
            .iter()
            .find(|h| &h.box_type == box_type)
            .map(|h| payload(&self.data, h))
    }

    /// Flattened box tree (bounded walk), for inspection and traces.
    pub fn box_walk(&self) -> Result<Vec<WalkEntry>> {
        walk_boxes(&self.data, MAX_WALK_DEPTH, MAX_WALK_BOXES)
    }

    /// The `meta` box, or an error naming what is missing.
    pub fn meta(&self) -> Result<&Meta> {
        self.meta
            .as_ref()
            .ok_or_else(|| HeifError::invalid("file has no meta box"))
    }

    /// The primary item (`pitm`), or an error.
    pub fn primary_item(&self) -> Result<&ItemInfo> {
        let meta = self.meta()?;
        let id = meta
            .primary_item_id
            .ok_or_else(|| HeifError::invalid("meta has no pitm box"))?;
        meta.item(id)
            .ok_or_else(|| HeifError::invalid(format!("pitm names item {id} which is not in iinf")))
    }

    /// Resolve the payload bytes of an item (all extents concatenated,
    /// every construction method). Borrows from the file when the item
    /// is a single contiguous span.
    pub fn item_data(&self, item_id: u32) -> Result<Cow<'_, [u8]>> {
        self.item_data_depth(item_id, 0)
    }

    /// Owned copy of [`item_data`](Self::item_data).
    pub fn item_data_owned(&self, item_id: u32) -> Result<Vec<u8>> {
        self.item_data(item_id).map(Cow::into_owned)
    }

    /// The byte spans (absolute file offsets) of an item whose data
    /// lives in this file via construction method 0. Used by the
    /// demuxers to hand out packets without copying twice.
    pub fn item_file_spans(&self, item_id: u32) -> Result<Vec<(usize, usize)>> {
        let meta = self.meta()?;
        let loc = meta
            .location(item_id)
            .ok_or_else(|| HeifError::invalid(format!("item {item_id} has no iloc entry")))?;
        if loc.construction_method != 0 {
            return Err(HeifError::invalid(format!(
                "item {item_id} uses construction method {}",
                loc.construction_method
            )));
        }
        self.check_self_contained(meta, loc)?;
        self.spans_in(loc, self.data.len())
    }

    fn check_self_contained(&self, meta: &Meta, loc: &ItemLocation) -> Result<()> {
        if !meta.is_self_contained(loc) {
            return Err(HeifError::unsupported(format!(
                "item {}: data_reference_index {} points outside this file",
                loc.item_id, loc.data_reference_index
            )));
        }
        Ok(())
    }

    /// Spans of `loc`'s extents inside a source of `source_len` bytes.
    fn spans_in(&self, loc: &ItemLocation, source_len: usize) -> Result<Vec<(usize, usize)>> {
        let mut out = Vec::with_capacity(loc.extents.len());
        let mut total: u64 = 0;
        for (i, e) in loc.extents.iter().enumerate() {
            let start = loc
                .base_offset
                .checked_add(e.offset)
                .ok_or_else(|| HeifError::invalid("iloc offset overflow"))?;
            if start > source_len as u64 {
                return Err(HeifError::invalid(format!(
                    "item {} extent {i} starts at {start}, past the {source_len}-byte source",
                    loc.item_id
                )));
            }
            let len = if e.length == 0 {
                source_len as u64 - start
            } else {
                e.length
            };
            let end = start
                .checked_add(len)
                .ok_or_else(|| HeifError::invalid("iloc extent end overflow"))?;
            if end > source_len as u64 {
                return Err(HeifError::invalid(format!(
                    "item {} extent {i} ends at {end}, past the {source_len}-byte source",
                    loc.item_id
                )));
            }
            total = total.saturating_add(len);
            if total > MAX_ITEM_BYTES {
                return Err(HeifError::exhausted(format!(
                    "item {} exceeds {MAX_ITEM_BYTES} bytes",
                    loc.item_id
                )));
            }
            out.push((start as usize, end as usize));
        }
        Ok(out)
    }

    fn item_data_depth(&self, item_id: u32, depth: usize) -> Result<Cow<'_, [u8]>> {
        if depth > MAX_ILOC_DEPTH {
            return Err(HeifError::exhausted(format!(
                "item {item_id}: construction-method-2 chain deeper than {MAX_ILOC_DEPTH}"
            )));
        }
        let meta = self.meta()?;
        let loc = meta
            .location(item_id)
            .ok_or_else(|| HeifError::invalid(format!("item {item_id} has no iloc entry")))?;
        if loc.extents.is_empty() {
            return Ok(Cow::Borrowed(&[]));
        }
        match loc.construction_method {
            0 => {
                self.check_self_contained(meta, loc)?;
                let spans = self.spans_in(loc, self.data.len())?;
                Ok(concat_spans(&self.data, &spans))
            }
            1 => {
                let idat = meta.idat.as_deref().ok_or_else(|| {
                    HeifError::invalid(format!(
                        "item {item_id} uses idat offsets but meta has no idat box"
                    ))
                })?;
                let spans = self.spans_in(loc, idat.len())?;
                Ok(concat_spans(idat, &spans))
            }
            2 => {
                let sources = meta.references_from(item_id, &reference::ILOC);
                let mut out = Vec::new();
                let mut total: u64 = 0;
                for (i, e) in loc.extents.iter().enumerate() {
                    let idx = if e.index == 0 { 1 } else { e.index } as usize;
                    let src_id = *sources.get(idx - 1).ok_or_else(|| {
                        HeifError::invalid(format!(
                            "item {item_id} extent {i}: extent_index {idx} but only {} 'iloc' references",
                            sources.len()
                        ))
                    })?;
                    if src_id == item_id {
                        return Err(HeifError::invalid(format!(
                            "item {item_id} references itself through construction method 2"
                        )));
                    }
                    let src = self.item_data_depth(src_id, depth + 1)?;
                    let start = loc
                        .base_offset
                        .checked_add(e.offset)
                        .ok_or_else(|| HeifError::invalid("iloc offset overflow"))?;
                    if start > src.len() as u64 {
                        return Err(HeifError::invalid(format!(
                            "item {item_id} extent {i} starts past the end of item {src_id}"
                        )));
                    }
                    let len = if e.length == 0 {
                        src.len() as u64 - start
                    } else {
                        e.length
                    };
                    let end = start
                        .checked_add(len)
                        .ok_or_else(|| HeifError::invalid("iloc extent end overflow"))?;
                    if end > src.len() as u64 {
                        return Err(HeifError::invalid(format!(
                            "item {item_id} extent {i} ends past the end of item {src_id}"
                        )));
                    }
                    total = total.saturating_add(len);
                    if total > MAX_ITEM_BYTES {
                        return Err(HeifError::exhausted(format!(
                            "item {item_id} exceeds {MAX_ITEM_BYTES} bytes"
                        )));
                    }
                    out.extend_from_slice(&src[start as usize..end as usize]);
                }
                Ok(Cow::Owned(out))
            }
            m => Err(HeifError::invalid(format!(
                "item {item_id}: construction_method {m}"
            ))),
        }
    }
}

fn concat_spans<'a>(source: &'a [u8], spans: &[(usize, usize)]) -> Cow<'a, [u8]> {
    if spans.len() == 1 {
        return Cow::Borrowed(&source[spans[0].0..spans[0].1]);
    }
    let total: usize = spans.iter().map(|(s, e)| e - s).sum();
    let mut v = Vec::with_capacity(total);
    for (s, e) in spans {
        v.extend_from_slice(&source[*s..*e]);
    }
    Cow::Owned(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::write::{boxed, full_boxed};

    fn ftyp() -> Vec<u8> {
        let mut b = b"heic".to_vec();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"mif1");
        boxed(b"ftyp", &b)
    }

    fn hdlr() -> Vec<u8> {
        let mut b = vec![0u8; 4];
        b.extend_from_slice(b"pict");
        b.extend_from_slice(&[0u8; 13]);
        full_boxed(b"hdlr", 0, 0, &b)
    }

    fn infe(id: u16, ty: &[u8; 4]) -> Vec<u8> {
        let mut b = id.to_be_bytes().to_vec();
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(ty);
        b.push(0);
        full_boxed(b"infe", 2, 0, &b)
    }

    fn iinf(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut b = (entries.len() as u16).to_be_bytes().to_vec();
        for e in entries {
            b.extend_from_slice(e);
        }
        full_boxed(b"iinf", 0, 0, &b)
    }

    type IlocItem = (u16, u8, Vec<(u32, u32, u32)>);

    /// iloc v1 with 4-byte offsets/lengths, 4-byte index.
    fn iloc(items: &[IlocItem]) -> Vec<u8> {
        let mut b = vec![0x44u8, 0x04];
        b.extend_from_slice(&(items.len() as u16).to_be_bytes());
        for (id, cm, ex) in items {
            b.extend_from_slice(&id.to_be_bytes());
            b.extend_from_slice(&(*cm as u16).to_be_bytes());
            b.extend_from_slice(&[0, 0]);
            b.extend_from_slice(&(ex.len() as u16).to_be_bytes());
            for (i, o, l) in ex {
                b.extend_from_slice(&i.to_be_bytes());
                b.extend_from_slice(&o.to_be_bytes());
                b.extend_from_slice(&l.to_be_bytes());
            }
        }
        full_boxed(b"iloc", 1, 0, &b)
    }

    fn iref_iloc(from: u16, to: u16) -> Vec<u8> {
        let mut b = from.to_be_bytes().to_vec();
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&to.to_be_bytes());
        full_boxed(b"iref", 0, 0, &boxed(b"iloc", &b))
    }

    fn build(meta_children: &[Vec<u8>], mdat: &[u8]) -> (Vec<u8>, usize) {
        let mut body = vec![0u8; 4];
        for c in meta_children {
            body.extend_from_slice(c);
        }
        let mut f = ftyp();
        f.extend(boxed(b"meta", &body));
        let mdat_payload_off = f.len() + 8;
        f.extend(boxed(b"mdat", mdat));
        (f, mdat_payload_off)
    }

    #[test]
    fn resolves_file_idat_and_item_offsets() {
        let mdat = b"0123456789abcdef".to_vec();
        // Two-pass: we need the mdat offset before building iloc, so
        // build once with a placeholder to learn the offset.
        let (_, off) = build(
            &[
                hdlr(),
                iinf(&[infe(1, b"hvc1"), infe(2, b"grid"), infe(3, b"hvc1")]),
                iloc(&[
                    (1, 0, vec![(0, 0, 4), (0, 12, 4)]),
                    (2, 1, vec![(0, 2, 3)]),
                    (3, 2, vec![(1, 1, 2)]),
                ]),
                iref_iloc(3, 1),
                boxed(b"idat", b"idat!"),
            ],
            &mdat,
        );
        let (file, off2) = build(
            &[
                hdlr(),
                iinf(&[infe(1, b"hvc1"), infe(2, b"grid"), infe(3, b"hvc1")]),
                iloc(&[
                    (1, 0, vec![(0, off as u32, 4), (0, off as u32 + 12, 4)]),
                    (2, 1, vec![(0, 2, 3)]),
                    (3, 2, vec![(1, 1, 2)]),
                ]),
                iref_iloc(3, 1),
                boxed(b"idat", b"idat!"),
            ],
            &mdat,
        );
        assert_eq!(off, off2);
        let f = HeifFile::parse(&file).unwrap();
        assert_eq!(f.item_data(1).unwrap().as_ref(), b"0123cdef");
        assert!(matches!(f.item_data(1).unwrap(), Cow::Owned(_)));
        assert_eq!(f.item_data(2).unwrap().as_ref(), b"at!");
        assert!(matches!(f.item_data(2).unwrap(), Cow::Borrowed(_)));
        // cm=2: bytes 1..3 of item 1's concatenated data.
        assert_eq!(f.item_data(3).unwrap().as_ref(), b"12");
        assert_eq!(f.item_file_spans(1).unwrap().len(), 2);
        assert!(f.item_file_spans(2).is_err());
        assert!(!f.has_moov());
        assert!(f.primary_item().unwrap_err().to_string().contains("pitm"));
    }

    #[test]
    fn rejects_out_of_range_and_self_reference() {
        let (file, _) = build(
            &[
                hdlr(),
                iinf(&[infe(1, b"hvc1"), infe(2, b"hvc1")]),
                iloc(&[(1, 0, vec![(0, 5000, 4)]), (2, 2, vec![(1, 0, 0)])]),
                iref_iloc(2, 2),
            ],
            b"xx",
        );
        let f = HeifFile::parse(&file).unwrap();
        assert!(f.item_data(1).is_err());
        assert!(f.item_data(2).unwrap_err().to_string().contains("itself"));
        assert!(f.item_data(9).is_err());
    }

    #[test]
    fn zero_length_extent_runs_to_end() {
        let (_, off) = build(
            &[
                hdlr(),
                iinf(&[infe(1, b"hvc1")]),
                iloc(&[(1, 0, vec![(0, 0, 0)])]),
            ],
            b"abcdef",
        );
        let (file, _) = build(
            &[
                hdlr(),
                iinf(&[infe(1, b"hvc1")]),
                iloc(&[(1, 0, vec![(0, off as u32 + 2, 0)])]),
            ],
            b"abcdef",
        );
        let f = HeifFile::parse(&file).unwrap();
        assert_eq!(f.item_data(1).unwrap().as_ref(), b"cdef");
    }

    #[test]
    fn requires_ftyp_first() {
        let mut file = boxed(b"free", &[]);
        file.extend(ftyp());
        assert!(HeifFile::parse(&file).is_err());
        assert!(HeifFile::parse(&boxed(b"mdat", &[1, 2])).is_err());
    }
}
