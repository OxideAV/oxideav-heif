//! ISO Base Media File Format box reader (ISO/IEC 14496-12 §4.2).
//!
//! A box is `size(32) + type(32)`, optionally followed by `largesize(64)`
//! when `size == 1`, or extending to the end of the enclosing buffer
//! when `size == 0`; `uuid` boxes carry a 16-byte extended type after
//! the header. A `FullBox` (§4.2.2) prefixes its payload with
//! `version(8) + flags(24)`.
//!
//! Everything here works on in-memory slices — a HEIF file is small
//! enough to hold whole, and the item location model (`iloc`) is
//! expressed in absolute file offsets that slice indexing serves
//! directly. Every arithmetic step is bounds-checked so a hostile size
//! field can never index out of range or overflow.

use crate::error::{HeifError, Result};

/// Four-character box / brand / item type, compared bytewise.
pub type FourCc = [u8; 4];

/// Render a four-character code for diagnostics (non-ASCII bytes are
/// escaped as `\xNN`).
pub fn fourcc_str(t: &FourCc) -> String {
    let mut s = String::with_capacity(4);
    for &b in t {
        if (0x20..0x7f).contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("\\x{b:02x}"));
        }
    }
    s
}

/// One parsed box header plus the payload span inside the parent slice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    /// Box type (`ftyp`, `meta`, …).
    pub box_type: FourCc,
    /// Extended type of a `uuid` box, when present.
    pub user_type: Option<[u8; 16]>,
    /// Offset of the first header byte inside the parent slice.
    pub start: usize,
    /// Offset of the first payload byte inside the parent slice.
    pub payload_start: usize,
    /// Payload length in bytes (header bytes excluded).
    pub payload_len: usize,
}

impl BoxHeader {
    /// Offset one past the last payload byte.
    pub fn end(&self) -> usize {
        self.payload_start + self.payload_len
    }

    /// Total box size including the header.
    pub fn total_len(&self) -> usize {
        self.end() - self.start
    }

    /// Length of the header bytes.
    pub fn header_len(&self) -> usize {
        self.payload_start - self.start
    }
}

/// Parse the box header starting at `start` inside `buf`.
pub fn parse_box_header(buf: &[u8], start: usize) -> Result<BoxHeader> {
    let hdr_end = start
        .checked_add(8)
        .ok_or_else(|| HeifError::invalid("box header offset overflow"))?;
    if hdr_end > buf.len() {
        return Err(HeifError::invalid(format!(
            "truncated box header at offset {start} (buffer is {} bytes)",
            buf.len()
        )));
    }
    let size32 = u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]]);
    let box_type: FourCc = [
        buf[start + 4],
        buf[start + 5],
        buf[start + 6],
        buf[start + 7],
    ];
    let remaining = buf.len() - start;
    let mut cursor = start + 8;
    let total: usize = match size32 {
        0 => remaining,
        1 => {
            let ls_end = cursor
                .checked_add(8)
                .ok_or_else(|| HeifError::invalid("largesize offset overflow"))?;
            if ls_end > buf.len() {
                return Err(HeifError::invalid(format!(
                    "truncated largesize for box '{}' at offset {start}",
                    fourcc_str(&box_type)
                )));
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[cursor..ls_end]);
            let ls = u64::from_be_bytes(b);
            cursor = ls_end;
            if ls < 16 || ls > remaining as u64 {
                return Err(HeifError::invalid(format!(
                    "box '{}' largesize {ls} out of range at offset {start} ({remaining} bytes remain)",
                    fourcc_str(&box_type)
                )));
            }
            ls as usize
        }
        s => {
            let s = s as usize;
            if s < 8 || s > remaining {
                return Err(HeifError::invalid(format!(
                    "box '{}' size {s} out of range at offset {start} ({remaining} bytes remain)",
                    fourcc_str(&box_type)
                )));
            }
            s
        }
    };
    let mut user_type = None;
    if &box_type == b"uuid" {
        let ut_end = cursor
            .checked_add(16)
            .ok_or_else(|| HeifError::invalid("uuid type offset overflow"))?;
        if ut_end > start + total {
            return Err(HeifError::invalid(format!(
                "uuid box at offset {start} too short for its extended type"
            )));
        }
        let mut ut = [0u8; 16];
        ut.copy_from_slice(&buf[cursor..ut_end]);
        user_type = Some(ut);
        cursor = ut_end;
    }
    let header_len = cursor - start;
    if header_len > total {
        return Err(HeifError::invalid(format!(
            "box '{}' at offset {start} declares a size smaller than its header",
            fourcc_str(&box_type)
        )));
    }
    Ok(BoxHeader {
        box_type,
        user_type,
        start,
        payload_start: cursor,
        payload_len: total - header_len,
    })
}

/// Iterator over the boxes packed contiguously in a slice.
pub struct BoxIter<'a> {
    buf: &'a [u8],
    cursor: usize,
    failed: bool,
}

impl<'a> BoxIter<'a> {
    /// Byte offset the iterator will read the next header from.
    pub fn position(&self) -> usize {
        self.cursor
    }
}

impl Iterator for BoxIter<'_> {
    type Item = Result<BoxHeader>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.cursor >= self.buf.len() {
            return None;
        }
        match parse_box_header(self.buf, self.cursor) {
            Ok(h) => {
                self.cursor = h.end();
                Some(Ok(h))
            }
            Err(e) => {
                self.failed = true;
                Some(Err(e))
            }
        }
    }
}

/// Iterate the boxes packed contiguously inside `buf`.
pub fn iter_boxes(buf: &[u8]) -> BoxIter<'_> {
    BoxIter {
        buf,
        cursor: 0,
        failed: false,
    }
}

/// Payload slice of a header inside its parent buffer.
pub fn payload<'a>(buf: &'a [u8], h: &BoxHeader) -> &'a [u8] {
    &buf[h.payload_start..h.end()]
}

/// Find the first box of type `target` among the contiguous boxes in
/// `buf`. Returns `Ok(None)` when absent, `Err` on a malformed walk.
pub fn find_box<'a>(buf: &'a [u8], target: &FourCc) -> Result<Option<(BoxHeader, &'a [u8])>> {
    for h in iter_boxes(buf) {
        let h = h?;
        if &h.box_type == target {
            return Ok(Some((h.clone(), payload(buf, &h))));
        }
    }
    Ok(None)
}

/// Find every box of type `target` among the contiguous boxes in `buf`.
pub fn find_boxes<'a>(buf: &'a [u8], target: &FourCc) -> Result<Vec<(BoxHeader, &'a [u8])>> {
    let mut out = Vec::new();
    for h in iter_boxes(buf) {
        let h = h?;
        if &h.box_type == target {
            out.push((h.clone(), payload(buf, &h)));
        }
    }
    Ok(out)
}

/// `FullBox` prefix: `(version, flags, body)`.
pub fn parse_full_box(p: &[u8]) -> Result<(u8, u32, &[u8])> {
    if p.len() < 4 {
        return Err(HeifError::invalid("truncated FullBox header"));
    }
    let flags = ((p[1] as u32) << 16) | ((p[2] as u32) << 8) | (p[3] as u32);
    Ok((p[0], flags, &p[4..]))
}

/// Bounds-checked big-endian cursor over a byte slice.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Wrap a slice.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Current offset.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// The unread tail.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    /// `true` once every byte has been consumed.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn need(&self, n: usize, what: &str) -> Result<()> {
        if self.remaining() < n {
            Err(HeifError::invalid(format!(
                "truncated {what}: need {n} bytes at offset {}, {} remain",
                self.pos,
                self.remaining()
            )))
        } else {
            Ok(())
        }
    }

    /// Take `n` raw bytes.
    pub fn bytes(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        self.need(n, what)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Skip `n` bytes.
    pub fn skip(&mut self, n: usize, what: &str) -> Result<()> {
        self.bytes(n, what).map(|_| ())
    }

    /// Read one byte.
    pub fn u8(&mut self, what: &str) -> Result<u8> {
        Ok(self.bytes(1, what)?[0])
    }

    /// Read a big-endian `u16`.
    pub fn u16(&mut self, what: &str) -> Result<u16> {
        let b = self.bytes(2, what)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Read a big-endian `u24`.
    pub fn u24(&mut self, what: &str) -> Result<u32> {
        let b = self.bytes(3, what)?;
        Ok(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32)
    }

    /// Read a big-endian `u32`.
    pub fn u32(&mut self, what: &str) -> Result<u32> {
        let b = self.bytes(4, what)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a big-endian `u64`.
    pub fn u64(&mut self, what: &str) -> Result<u64> {
        let b = self.bytes(8, what)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    /// Read a big-endian `i16`.
    pub fn i16(&mut self, what: &str) -> Result<i16> {
        self.u16(what).map(|v| v as i16)
    }

    /// Read a big-endian `i32`.
    pub fn i32(&mut self, what: &str) -> Result<i32> {
        self.u32(what).map(|v| v as i32)
    }

    /// Read a four-character code.
    pub fn fourcc(&mut self, what: &str) -> Result<FourCc> {
        let b = self.bytes(4, what)?;
        Ok([b[0], b[1], b[2], b[3]])
    }

    /// Read an unsigned integer of `width` bytes (0, 4 or 8 — the field
    /// widths the `iloc` box permits; 0 yields 0). Widths 1 and 2 are
    /// accepted too for other users.
    pub fn uint(&mut self, width: usize, what: &str) -> Result<u64> {
        match width {
            0 => Ok(0),
            1 => self.u8(what).map(u64::from),
            2 => self.u16(what).map(u64::from),
            4 => self.u32(what).map(u64::from),
            8 => self.u64(what),
            w => Err(HeifError::invalid(format!(
                "{what}: unsupported field width {w}"
            ))),
        }
    }

    /// Read a NUL-terminated string (UTF-8, lossily decoded). The
    /// terminator is consumed. A string running to the end of the
    /// buffer without a terminator is accepted (some writers omit it on
    /// the last field of a box).
    pub fn cstr(&mut self, what: &str) -> Result<String> {
        let rest = self.rest();
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.pos += end.min(rest.len());
        if self.pos < self.buf.len() {
            self.pos += 1; // terminator
        }
        let _ = what;
        Ok(s)
    }
}

/// Container boxes whose children are plain boxes (no header fields
/// before the first child). `meta` and `iref` are FullBoxes and are
/// handled by the walker explicitly; `stsd` / `dref` carry an entry
/// count and are treated as leaves here.
pub const PLAIN_CONTAINERS: &[&FourCc] = &[
    b"moov", b"trak", b"mdia", b"minf", b"dinf", b"stbl", b"iprp", b"ipco", b"edts", b"udta",
    b"mvex", b"moof", b"traf", b"grpl", b"tref",
];

/// One entry of a flattened box walk: the header plus its nesting depth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkEntry {
    /// Header with offsets expressed relative to the whole file.
    pub header: BoxHeader,
    /// Nesting depth (0 for top-level boxes).
    pub depth: usize,
}

/// Flatten the box tree of a whole file. Descends into
/// [`PLAIN_CONTAINERS`], into `meta` (skipping its FullBox header) and
/// into `iinf` (skipping the entry count). Errors inside a container are
/// surfaced; the walk is bounded by `max_depth` and `max_boxes` so a
/// hostile file cannot make it recurse or enumerate without bound.
pub fn walk_boxes(file: &[u8], max_depth: usize, max_boxes: usize) -> Result<Vec<WalkEntry>> {
    let mut out = Vec::new();
    walk_into(file, 0, file.len(), 0, max_depth, max_boxes, &mut out)?;
    Ok(out)
}

fn walk_into(
    file: &[u8],
    start: usize,
    end: usize,
    depth: usize,
    max_depth: usize,
    max_boxes: usize,
    out: &mut Vec<WalkEntry>,
) -> Result<()> {
    let mut cursor = start;
    while cursor < end {
        let h = parse_box_header(&file[..end], cursor)?;
        if out.len() >= max_boxes {
            return Err(HeifError::exhausted(format!(
                "box walk exceeded {max_boxes} boxes"
            )));
        }
        out.push(WalkEntry {
            header: h.clone(),
            depth,
        });
        let descend_from = if PLAIN_CONTAINERS.contains(&&h.box_type) {
            Some(h.payload_start)
        } else if matches!(&h.box_type, b"meta" | b"iref") {
            Some(h.payload_start + 4)
        } else if &h.box_type == b"iinf" {
            // FullBox + entry_count (u16 for v0, u32 otherwise).
            let p = payload(file, &h);
            match parse_full_box(p) {
                Ok((0, _, _)) => Some(h.payload_start + 6),
                Ok(_) => Some(h.payload_start + 8),
                Err(_) => None,
            }
        } else {
            None
        };
        if let Some(child_start) = descend_from {
            if depth + 1 > max_depth {
                return Err(HeifError::exhausted(format!(
                    "box nesting deeper than {max_depth}"
                )));
            }
            if child_start <= h.end() {
                walk_into(
                    file,
                    child_start,
                    h.end(),
                    depth + 1,
                    max_depth,
                    max_boxes,
                    out,
                )?;
            }
        }
        cursor = h.end();
    }
    Ok(())
}

/// Serialize helpers used by the writer side.
pub mod write {
    use super::FourCc;

    /// Append `size(32) + type(32) + body` to `out`, using a `largesize`
    /// header when the box would exceed `u32::MAX` bytes.
    pub fn push_box(out: &mut Vec<u8>, box_type: &FourCc, body: &[u8]) {
        let total = body.len() as u64 + 8;
        if total > u32::MAX as u64 {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(box_type);
            out.extend_from_slice(&(total + 8).to_be_bytes());
        } else {
            out.extend_from_slice(&(total as u32).to_be_bytes());
            out.extend_from_slice(box_type);
        }
        out.extend_from_slice(body);
    }

    /// Append a FullBox: `size + type + version + flags(24) + body`.
    pub fn push_full_box(
        out: &mut Vec<u8>,
        box_type: &FourCc,
        version: u8,
        flags: u32,
        body: &[u8],
    ) {
        let mut b = Vec::with_capacity(body.len() + 4);
        b.push(version);
        b.extend_from_slice(&flags.to_be_bytes()[1..]);
        b.extend_from_slice(body);
        push_box(out, box_type, &b);
    }

    /// Build a box as a standalone byte vector.
    pub fn boxed(box_type: &FourCc, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(body.len() + 8);
        push_box(&mut v, box_type, body);
        v
    }

    /// Build a FullBox as a standalone byte vector.
    pub fn full_boxed(box_type: &FourCc, version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(body.len() + 12);
        push_full_box(&mut v, box_type, version, flags, body);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
        write::boxed(t, body)
    }

    #[test]
    fn walks_two_boxes() {
        let mut buf = boxed(b"ftyp", &[0u8; 24]);
        buf.extend(boxed(b"meta", &[]));
        let hs: Vec<_> = iter_boxes(&buf).collect::<Result<_>>().unwrap();
        assert_eq!(hs.len(), 2);
        assert_eq!(&hs[0].box_type, b"ftyp");
        assert_eq!(hs[0].total_len(), 32);
        assert_eq!(&hs[1].box_type, b"meta");
        assert_eq!(hs[1].payload_len, 0);
        assert_eq!(hs[1].start, 32);
    }

    #[test]
    fn size_zero_extends_to_end() {
        let mut buf = 0u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&[1, 2, 3]);
        let h = parse_box_header(&buf, 0).unwrap();
        assert_eq!(h.payload_len, 3);
    }

    #[test]
    fn largesize_header() {
        let mut buf = 1u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&20u64.to_be_bytes());
        buf.extend_from_slice(&[9; 4]);
        let h = parse_box_header(&buf, 0).unwrap();
        assert_eq!(h.payload_start, 16);
        assert_eq!(h.payload_len, 4);
        // A largesize below 16 is rejected.
        let mut bad = 1u32.to_be_bytes().to_vec();
        bad.extend_from_slice(b"mdat");
        bad.extend_from_slice(&8u64.to_be_bytes());
        assert!(parse_box_header(&bad, 0).is_err());
    }

    #[test]
    fn uuid_extended_type() {
        let mut body = vec![0xAB; 16];
        body.extend_from_slice(&[1, 2]);
        let buf = boxed(b"uuid", &body);
        let h = parse_box_header(&buf, 0).unwrap();
        assert_eq!(h.user_type, Some([0xAB; 16]));
        assert_eq!(h.payload_len, 2);
    }

    #[test]
    fn rejects_truncated_and_overflowing() {
        let buf = [0, 0, 0, 0x20, b'f', b't', b'y', b'p', 0, 0];
        assert!(parse_box_header(&buf, 0).is_err());
        assert!(parse_box_header(&buf, usize::MAX - 2).is_err());
        let tiny = [0, 0, 0, 4, b'x', b'x', b'x', b'x'];
        assert!(parse_box_header(&tiny, 0).is_err());
    }

    #[test]
    fn iterator_stops_after_error() {
        let mut buf = boxed(b"ftyp", &[0u8; 4]);
        buf.extend_from_slice(&[0, 0, 0, 0xFF, b'b', b'a', b'd', b'!']);
        let mut it = iter_boxes(&buf);
        assert!(it.next().unwrap().is_ok());
        assert!(it.next().unwrap().is_err());
        assert!(it.next().is_none());
    }

    #[test]
    fn reader_primitives() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05, b'a', b'b', 0, 0xFF];
        let mut r = Reader::new(&data);
        assert_eq!(r.u8("a").unwrap(), 1);
        assert_eq!(r.u16("b").unwrap(), 0x0203);
        assert_eq!(r.u16("c").unwrap(), 0x0405);
        assert_eq!(r.cstr("s").unwrap(), "ab");
        assert_eq!(r.remaining(), 1);
        assert!(r.u16("x").is_err());
        assert_eq!(r.uint(0, "z").unwrap(), 0);
        assert!(r.uint(3, "z").is_err());
    }

    #[test]
    fn cstr_without_terminator_runs_to_end() {
        let data = b"hello";
        let mut r = Reader::new(data);
        assert_eq!(r.cstr("s").unwrap(), "hello");
        assert!(r.is_empty());
    }

    #[test]
    fn walk_descends_into_meta_and_containers() {
        let inner = boxed(b"hdlr", &[0u8; 20]);
        let mut meta_body = vec![0u8; 4];
        meta_body.extend(&inner);
        let mut file = boxed(b"ftyp", &[0u8; 8]);
        file.extend(boxed(b"meta", &meta_body));
        let stbl = boxed(b"stbl", &boxed(b"stts", &[0u8; 8]));
        let minf = boxed(b"minf", &stbl);
        let mdia = boxed(b"mdia", &minf);
        let trak = boxed(b"trak", &mdia);
        file.extend(boxed(b"moov", &trak));
        let w = walk_boxes(&file, 16, 1024).unwrap();
        let types: Vec<String> = w.iter().map(|e| fourcc_str(&e.header.box_type)).collect();
        assert_eq!(
            types,
            ["ftyp", "meta", "hdlr", "moov", "trak", "mdia", "minf", "stbl", "stts"]
        );
        assert_eq!(w[2].depth, 1);
        assert_eq!(w[8].depth, 5);
        assert!(walk_boxes(&file, 2, 1024).is_err());
        assert!(walk_boxes(&file, 16, 3).is_err());
    }

    #[test]
    fn full_box_writer_round_trips() {
        let v = write::full_boxed(b"pitm", 0, 0, &[0, 1]);
        let h = parse_box_header(&v, 0).unwrap();
        let (ver, flags, body) = parse_full_box(payload(&v, &h)).unwrap();
        assert_eq!((ver, flags), (0, 0));
        assert_eq!(body, &[0, 1]);
    }
}
