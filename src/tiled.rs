//! Tiled image items (ISO/IEC 23008-12:2025/Amd 2:2026 §6.11): a `tili`
//! item is a rectangle of uniform, independently coded tiles addressed
//! through a `DataEntryTiledItemBox` (`deti`) in the `dref` — either
//! URLs of external tile files, or a `TiledImageOffsetTable` inside the
//! item's own data range giving every tile's offset (and size). The
//! tiles' coding format and properties come from the `tilC` property
//! (`tile_item_type` + `TileItemPropertyAssociationBox`).
//!
//! This crate composes in-file tiles (`external_tiles_urls == 0`);
//! external tiles are a typed [`HeifError::Unsupported`], as are the
//! extra (non-spatial) dimensions of hyperrectangles.

use crate::boxes::Reader;
use crate::error::{HeifError, Result};
use crate::meta::DataReference;

/// The empty-tile marker: a `tile_start_offset` of all ones
/// (§6.11.5.3, written for a 32-bit field as `0xFFFFFFFF`).
pub const EMPTY_TILE: u64 = u64::MAX;

/// Upper bound on the tiles of one item this crate composes.
pub const MAX_TILES: u64 = 1 << 20;

/// The result of [`DataEntryTiledItem::pack_tiles`]: the item data, the
/// parsed form of the `deti` entry describing it and that entry's
/// `(flags, payload)` for a `dref`.
pub type PackedTiles = (Vec<u8>, DataEntryTiledItem, (u32, Vec<u8>));

/// A parsed `DataEntryTiledItemBox` (§6.11.5).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DataEntryTiledItem {
    /// Bits of a `tile_start_offset` (32 / 40 / 48 / 64).
    pub offset_bits: u8,
    /// Bits of a `tile_size` (0 = sizes inferred / 24 / 32 / 64).
    pub size_bits: u8,
    /// `sequential_order`: tile data stored consecutively in tile order.
    pub sequential_order: bool,
    /// `no_of_input_items`.
    pub input_items: u64,
    /// External tiles (`external_tiles_urls`): the URL template
    /// fields, else `None`.
    pub external: Option<ExternalTiles>,
    /// `(tile_offset_table_start_offset, tile_offset_table_size)`
    /// relative to the item's referenced data, for in-file tiles.
    pub offset_table: Option<(u64, u32)>,
}
impl DataEntryTiledItem {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(
        offset_bits: u8,
        size_bits: u8,
        sequential_order: bool,
        input_items: u64,
        external: Option<ExternalTiles>,
        offset_table: Option<(u64, u32)>,
    ) -> Self {
        Self {
            offset_bits,
            size_bits,
            sequential_order,
            input_items,
            external,
            offset_table,
        }
    }
}

/// The URL construction fields of an external-tile `deti`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExternalTiles {
    /// `directoryIDstart` / `directoryIDend` when `directory_ID_flag`.
    pub directory_ids: Option<(u16, u16)>,
    /// `tileIDstart`.
    pub tile_id_start: u64,
    /// `baseurl`.
    pub base_url: String,
    /// `urlextension`.
    pub url_extension: String,
    /// `tileitemrequesttemplate`.
    pub request_template: String,
}
impl ExternalTiles {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(
        directory_ids: Option<(u16, u16)>,
        tile_id_start: u64,
        base_url: String,
        url_extension: String,
        request_template: String,
    ) -> Self {
        Self {
            directory_ids,
            tile_id_start,
            base_url,
            url_extension,
            request_template,
        }
    }
}

impl DataEntryTiledItem {
    /// Parse a `deti` `dref` entry.
    pub fn parse(entry: &DataReference) -> Result<Self> {
        if &entry.entry_type != b"deti" {
            return Err(HeifError::invalid(format!(
                "data reference '{}' is not a deti entry",
                crate::boxes::fourcc_str(&entry.entry_type)
            )));
        }
        let flags = entry.flags;
        let offset_bits = [32u8, 40, 48, 64][(flags & 3) as usize];
        let size_bits = [0u8, 24, 32, 64][((flags >> 2) & 3) as usize];
        let sequential_order = (flags >> 4) & 1 == 1;
        let count_bits = [8u8, 16, 32, 64][((flags >> 5) & 3) as usize];
        let external_flag = (flags >> 7) & 1 == 1;
        let mut r = Reader::new(&entry.payload);
        let input_items = r.uint(count_bits as usize / 8, "deti no_of_input_items")?;
        let (external, offset_table) = if external_flag {
            let dir_flag = r.u8("deti directory_ID_flag")? & 1 == 1;
            let directory_ids = if dir_flag {
                Some((
                    r.u16("deti directoryIDstart")?,
                    r.u16("deti directoryIDend")?,
                ))
            } else {
                None
            };
            let tile_id_start = r.u64("deti tileIDstart")?;
            let base_url = r.cstr("deti baseurl")?;
            let url_extension = r.cstr("deti urlextension")?;
            let request_template = r.cstr("deti tileitemrequesttemplate")?;
            (
                Some(ExternalTiles {
                    directory_ids,
                    tile_id_start,
                    base_url,
                    url_extension,
                    request_template,
                }),
                None,
            )
        } else {
            let start = r.uint(
                offset_bits as usize / 8,
                "deti tile_offset_table_start_offset",
            )?;
            let size = r.u32("deti tile_offset_table_size")?;
            (None, Some((start, size)))
        };
        Ok(Self {
            offset_bits,
            size_bits,
            sequential_order,
            input_items,
            external,
            offset_table,
        })
    }

    /// The `(offset, length)` of every tile inside the item's referenced
    /// `data` (§6.11.5.3, `TiledImageOffsetTable`), `None` for an empty
    /// tile; `num_tiles` is `NumTiles` from the `tilC` / `ispe`
    /// geometry. Sizes absent from the table are inferred from the
    /// offset differences (sorted when the tiles are not sequential;
    /// the last tile runs to the end of the data).
    pub fn tile_spans(&self, data: &[u8], num_tiles: u64) -> Result<Vec<Option<(u64, u64)>>> {
        let (start, size) = self.offset_table.ok_or_else(|| {
            HeifError::unsupported("tili: tiles stored in external files (URL template)")
        })?;
        if num_tiles > MAX_TILES {
            return Err(HeifError::exhausted(format!(
                "tili: {num_tiles} tiles (cap {MAX_TILES})"
            )));
        }
        if self.input_items != num_tiles {
            return Err(HeifError::invalid(format!(
                "tili: deti no_of_input_items {} but the tilC / ispe geometry has {num_tiles} tiles",
                self.input_items
            )));
        }
        let entry_bytes = (self.offset_bits as u64 + self.size_bits as u64) / 8;
        let need = num_tiles * entry_bytes;
        if (size as u64) < need {
            return Err(HeifError::invalid(format!(
                "tili: offset table of {size} bytes holds fewer than {num_tiles} entries of {entry_bytes} bytes"
            )));
        }
        let end = start
            .checked_add(size as u64)
            .filter(|e| *e <= data.len() as u64)
            .ok_or_else(|| {
                HeifError::invalid(format!(
                    "tili: offset table {start}+{size} runs past the {} data bytes",
                    data.len()
                ))
            })?;
        let mut r = Reader::new(&data[start as usize..end as usize]);
        let empty_marker = if self.offset_bits == 64 {
            u64::MAX
        } else {
            (1u64 << self.offset_bits) - 1
        };
        let mut offsets: Vec<Option<u64>> = Vec::with_capacity(num_tiles as usize);
        let mut sizes: Vec<Option<u64>> = Vec::with_capacity(num_tiles as usize);
        for _ in 0..num_tiles {
            let off = r.uint(self.offset_bits as usize / 8, "tili tile_start_offset")?;
            offsets.push((off != empty_marker).then_some(off));
            if self.size_bits > 0 {
                sizes.push(Some(r.uint(self.size_bits as usize / 8, "tili tile_size")?));
            } else {
                sizes.push(None);
            }
        }
        // Inferred sizes: the distance to the next tile start in file
        // order (sorted when not sequential), the last to the data end.
        // The offset table itself bounds a tile that precedes it.
        let mut starts: Vec<u64> = offsets.iter().flatten().copied().collect();
        starts.push(start);
        starts.push(data.len() as u64);
        starts.sort_unstable();
        starts.dedup();
        let mut out = Vec::with_capacity(num_tiles as usize);
        for (off, size) in offsets.iter().zip(&sizes) {
            let Some(off) = off else {
                out.push(None);
                continue;
            };
            let len = match size {
                Some(s) => *s,
                None => {
                    let next = starts
                        .iter()
                        .find(|s| **s > *off)
                        .copied()
                        .unwrap_or(data.len() as u64);
                    next.saturating_sub(*off)
                }
            };
            if off
                .checked_add(len)
                .filter(|e| *e <= data.len() as u64)
                .is_none()
            {
                return Err(HeifError::invalid(format!(
                    "tili: tile at {off}+{len} runs past the {} data bytes",
                    data.len()
                )));
            }
            out.push(Some((*off, len)));
        }
        Ok(out)
    }

    /// Serialise as a `dref` entry `(flags, payload)` for in-file tiles
    /// with 32-bit offsets, 32-bit sizes, a 32-bit count, sequential
    /// storage.
    pub fn in_file(num_tiles: u32, table_start: u32, table_size: u32) -> (u32, Vec<u8>) {
        // offset code 0 (32), size code 2 (32), sequential, count code 2 (32).
        let flags = (2 << 2) | (1 << 4) | (2 << 5);
        let mut b = num_tiles.to_be_bytes().to_vec();
        b.extend_from_slice(&table_start.to_be_bytes());
        b.extend_from_slice(&table_size.to_be_bytes());
        (flags, b)
    }

    /// Build the item data of an in-file tiled item from its tiles
    /// (`None` = empty tile): the tiles back to back, then the
    /// `TiledImageOffsetTable` (32-bit offsets + sizes). Returns the
    /// data and the `deti` entry describing it.
    pub fn pack_tiles(tiles: &[Option<Vec<u8>>]) -> Result<PackedTiles> {
        let mut data = Vec::new();
        let mut table = Vec::with_capacity(tiles.len() * 8);
        for t in tiles {
            match t {
                Some(bytes) => {
                    let off = data.len();
                    if off as u64 + bytes.len() as u64 >= u32::MAX as u64 {
                        return Err(HeifError::exhausted(
                            "tili: tile data beyond the 32-bit offset table",
                        ));
                    }
                    table.extend_from_slice(&(off as u32).to_be_bytes());
                    table.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                    data.extend_from_slice(bytes);
                }
                None => {
                    table.extend_from_slice(&u32::MAX.to_be_bytes());
                    table.extend_from_slice(&0u32.to_be_bytes());
                }
            }
        }
        let table_start = data.len() as u32;
        let table_size = table.len() as u32;
        data.extend_from_slice(&table);
        let (flags, payload) = Self::in_file(tiles.len() as u32, table_start, table_size);
        let entry = DataEntryTiledItem {
            offset_bits: 32,
            size_bits: 32,
            sequential_order: true,
            input_items: tiles.len() as u64,
            external: None,
            offset_table: Some((table_start as u64, table_size)),
        };
        Ok((data, entry, (flags, payload)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(flags: u32, payload: Vec<u8>) -> DataReference {
        DataReference {
            entry_type: *b"deti",
            self_contained: true,
            location: String::new(),
            name: String::new(),
            flags,
            payload,
        }
    }

    #[test]
    fn pack_and_span_round_trip() {
        let tiles = vec![
            Some(vec![1u8; 10]),
            None,
            Some(vec![2u8; 4]),
            Some(vec![3u8; 7]),
        ];
        let (data, deti, (flags, payload)) = DataEntryTiledItem::pack_tiles(&tiles).unwrap();
        let parsed = DataEntryTiledItem::parse(&entry(flags, payload)).unwrap();
        assert_eq!(parsed, deti);
        let spans = parsed.tile_spans(&data, 4).unwrap();
        assert_eq!(
            spans,
            vec![Some((0, 10)), None, Some((10, 4)), Some((14, 7))]
        );
        assert_eq!(&data[14..21], &[3u8; 7]);
        assert!(parsed.tile_spans(&data, 5).is_err(), "count mismatch");
    }

    #[test]
    fn sizes_are_inferred_without_a_size_field() {
        // offset code 0 (32-bit), size code 0 (none), non-sequential,
        // count code 0 (8-bit): tiles stored 2, 0, 1.
        let mut data = vec![9u8; 30]; // tile bytes: [0..10) tile 2, [10..15) tile 0, [15..30) tile 1
        let table_start = data.len() as u32;
        for off in [10u32, 15, 0] {
            data.extend_from_slice(&off.to_be_bytes());
        }
        let mut payload = vec![3u8];
        payload.extend_from_slice(&table_start.to_be_bytes());
        payload.extend_from_slice(&12u32.to_be_bytes());
        let parsed = DataEntryTiledItem::parse(&entry(0, payload)).unwrap();
        assert_eq!(parsed.size_bits, 0);
        let spans = parsed.tile_spans(&data, 3).unwrap();
        assert_eq!(spans, vec![Some((10, 5)), Some((15, 15)), Some((0, 10))]);
    }

    #[test]
    fn external_tiles_parse_and_refuse_spans() {
        let mut payload = vec![0u8; 2]; // count (16-bit code 1) = 0
        payload[1] = 4;
        payload.push(1); // directory_ID_flag
        payload.extend_from_slice(&10u16.to_be_bytes());
        payload.extend_from_slice(&25u16.to_be_bytes());
        payload.extend_from_slice(&1000u64.to_be_bytes());
        payload.extend_from_slice(b"http://cdn.example.com/pictures/134532/image/\0");
        payload.extend_from_slice(b"Representation1\0");
        payload.extend_from_slice(b"$tileID$.heif\0");
        let flags = (1 << 5) | (1 << 7);
        let parsed = DataEntryTiledItem::parse(&entry(flags, payload)).unwrap();
        let ext = parsed.external.as_ref().unwrap();
        assert_eq!(ext.directory_ids, Some((10, 25)));
        assert_eq!(ext.tile_id_start, 1000);
        assert_eq!(ext.request_template, "$tileID$.heif");
        assert!(matches!(
            parsed.tile_spans(&[], 4),
            Err(HeifError::Unsupported(_))
        ));
    }
}
