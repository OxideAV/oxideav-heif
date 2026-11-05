//! Tiled image items (ISO/IEC 23008-12:2025/Amd 2:2026 §6.11): a `tili`
//! item is a rectangle of uniform, independently coded tiles addressed
//! through a `DataEntryTiledItemBox` (`deti`) in the `dref` — either
//! URLs of external tile files, or a `TiledImageOffsetTable` inside the
//! item's own data range giving every tile's offset (and size). The
//! tiles' coding format and properties come from the `tilC` property
//! (`tile_item_type` + `TileItemPropertyAssociationBox`).
//!
//! In-file tiles (`external_tiles_urls == 0`) compose from the item's
//! own data. External tiles (`external_tiles_urls == 1`, §6.11.2: one
//! HEIF file per tile, addressed by URL) compose through a
//! caller-supplied [`ExternalTileResolver`] — this crate never touches
//! a filesystem or a network on its own, so a file opened from bytes
//! alone refuses them typed ([`HeifError::external_tile_unresolved`]),
//! naming the first missing reference. The extra (non-spatial)
//! dimensions of hyperrectangles are a typed [`HeifError::Unsupported`].

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
#[doc(hidden)]
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
    /// Every field as a positional argument, in declaration order.
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

/// The URL construction fields of an external-tile `deti`
/// (§6.11.5.2, `external_tiles_urls == 1`).
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
    /// Every field as a positional argument, in declaration order.
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

/// The `$tileID$` placeholder of a `tileitemrequesttemplate`
/// (§6.11.5.3: "the tileID is replaced by the actual value of the tile").
pub const TILE_ID_PLACEHOLDER: &str = "$tileID$";

impl ExternalTiles {
    /// The tile ID of the tile at row-major `index` (§6.11.5.3: IDs run
    /// from `tileIDstart` in steps of one, in row-major order).
    pub fn tile_id(&self, index: u64) -> Result<u64> {
        self.tile_id_start.checked_add(index).ok_or_else(|| {
            HeifError::invalid(format!(
                "tili: tileIDstart {} + tile {index} overflows 64 bits",
                self.tile_id_start
            ))
        })
    }

    /// The directory ID of the tile at row-major `index` of `num_tiles`
    /// (`None` without `directory_ID_flag`). §6.11.5.3: every directory
    /// holds `NumTiles / (directoryIDend − directoryIDstart)` tiles, the
    /// last one (`directoryIDend`) also the remainder. Where that
    /// quotient is undefined (`directoryIDend <= directoryIDstart`) or
    /// zero (fewer tiles than directories) the text gives no
    /// assignment: a typed [`HeifError::Unsupported`].
    pub fn directory_id(&self, index: u64, num_tiles: u64) -> Result<Option<u16>> {
        let Some((start, end)) = self.directory_ids else {
            return Ok(None);
        };
        if end <= start {
            return Err(HeifError::unsupported(format!(
                "tili: directoryIDend {end} <= directoryIDstart {start} \
                 (the tiles per directory, NumTiles / (end - start), are undefined)"
            )));
        }
        let per_directory = num_tiles / (end - start) as u64;
        if per_directory == 0 {
            return Err(HeifError::unsupported(format!(
                "tili: {num_tiles} tiles over directories {start}..={end} \
                 (NumTiles / (end - start) is 0 tiles per directory)"
            )));
        }
        let d = (start as u64 + index / per_directory).min(end as u64);
        Ok(Some(d as u16))
    }

    /// The URL of the tile at row-major `index` of `num_tiles`
    /// (§6.11.5.3, Table 12): `baseurl`, `urlextension`, `/`, then —
    /// with directory IDs — the tile's directory ID and `/`, then the
    /// `tileitemrequesttemplate` with every `$tileID$` replaced by the
    /// tile ID in decimal. The worked example of Table 12 (`…/image/` +
    /// `Representation1` + `/10/` + `1000.heif`) fixes the separator
    /// after the directory ID that the prose leaves out.
    pub fn url(&self, index: u64, num_tiles: u64) -> Result<String> {
        let tile_id = self.tile_id(index)?;
        let request = self
            .request_template
            .replace(TILE_ID_PLACEHOLDER, &tile_id.to_string());
        Ok(match self.directory_id(index, num_tiles)? {
            Some(d) => format!("{}{}/{d}/{request}", self.base_url, self.url_extension),
            None => format!("{}{}/{request}", self.base_url, self.url_extension),
        })
    }

    /// The request for the tile at row-major `index` of a
    /// `columns`-wide, `num_tiles`-tile `tili` item `item_id`.
    pub fn request(
        &self,
        item_id: u32,
        index: u64,
        columns: u32,
        num_tiles: u64,
    ) -> Result<ExternalTileRequest> {
        if columns == 0 || index >= num_tiles {
            return Err(HeifError::invalid(format!(
                "tili item {item_id}: tile {index} outside {num_tiles} tiles"
            )));
        }
        Ok(ExternalTileRequest {
            item_id,
            index,
            column: (index % columns as u64) as u32,
            row: (index / columns as u64) as u32,
            tile_id: self.tile_id(index)?,
            directory_id: self.directory_id(index, num_tiles)?,
            url: self.url(index, num_tiles)?,
        })
    }
}

/// One external tile of a `tili` item to fetch (§6.11.2: a HEIF file
/// holding the tile as its only image item), as handed to an
/// [`ExternalTileResolver`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExternalTileRequest {
    /// The `tili` item.
    pub item_id: u32,
    /// Row-major tile index (`row × TileColumns + column`).
    pub index: u64,
    /// Tile column.
    pub column: u32,
    /// Tile row.
    pub row: u32,
    /// The tile ID (`tileIDstart + index`).
    pub tile_id: u64,
    /// The tile's directory ID, when the `deti` uses directories.
    pub directory_id: Option<u16>,
    /// The URL built from the `deti` fields ([`ExternalTiles::url`]).
    pub url: String,
}
impl ExternalTileRequest {
    /// Every field as a positional argument, in declaration order.
    pub fn new(
        item_id: u32,
        index: u64,
        column: u32,
        row: u32,
        tile_id: u64,
        directory_id: Option<u16>,
        url: String,
    ) -> Self {
        Self {
            item_id,
            index,
            column,
            row,
            tile_id,
            directory_id,
            url,
        }
    }
}

/// Supplies the bytes of external tiles (§6.11.2). The reader never
/// fetches anything itself: the caller decides what a URL means (a
/// path next to the file, an HTTP cache, an in-memory map) and returns
/// the whole tile file. Any error is passed through as the decode's
/// error. Implemented for every `Fn(&ExternalTileRequest) ->
/// Result<Vec<u8>> + Sync` closure.
pub trait ExternalTileResolver: Sync {
    /// The bytes of the HEIF file holding `request`'s tile.
    fn resolve(&self, request: &ExternalTileRequest) -> Result<Vec<u8>>;
}

impl<F> ExternalTileResolver for F
where
    F: Fn(&ExternalTileRequest) -> Result<Vec<u8>> + Sync,
{
    fn resolve(&self, request: &ExternalTileRequest) -> Result<Vec<u8>> {
        self(request)
    }
}

/// A shareable [`ExternalTileResolver`] (the form
/// [`DecodeOptions::tile_resolver`](crate::DecodeOptions::tile_resolver)
/// holds). Two handles compare equal when they share one resolver.
#[derive(Clone)]
pub struct SharedTileResolver(std::sync::Arc<dyn ExternalTileResolver + Send>);

impl SharedTileResolver {
    /// Wrap `resolver`.
    pub fn new(resolver: impl ExternalTileResolver + Send + 'static) -> Self {
        Self(std::sync::Arc::new(resolver))
    }

    /// The resolver.
    pub fn resolver(&self) -> &dyn ExternalTileResolver {
        &*self.0
    }
}

impl std::fmt::Debug for SharedTileResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedTileResolver(..)")
    }
}

impl PartialEq for SharedTileResolver {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Check that `tile` can be the external tile file of a `tili` item
/// whose `MetaBox` handler is `handler_type` (§6.11.2: a HEIF compliant
/// file holding the tile as its only image item, no entity grouping,
/// the same `handler_type`). Returns the tile's item ID.
pub fn check_external_tile<D: AsRef<[u8]>>(
    tile: &crate::file::HeifFile<D>,
    handler_type: Option<crate::boxes::FourCc>,
) -> Result<u32> {
    let fourcc = crate::boxes::fourcc_str;
    if !tile.file_type.is_heif_family() {
        return Err(HeifError::invalid(
            "external tile: not a HEIF file (no HEIF-family brand; §6.11.2)",
        ));
    }
    let meta = tile.meta()?;
    if !meta.entity_groups.is_empty() {
        return Err(HeifError::invalid(format!(
            "external tile: {} entity group(s) (§6.11.2: shall not contain any)",
            meta.entity_groups.len()
        )));
    }
    let images: Vec<&crate::meta::ItemInfo> = meta.items.iter().filter(|i| i.is_image()).collect();
    let [item] = images.as_slice() else {
        return Err(HeifError::invalid(format!(
            "external tile: {} image items (§6.11.2: only one input tile as an image item)",
            images.len()
        )));
    };
    let tile_handler = meta.handler.as_ref().map(|h| h.handler_type);
    if tile_handler != handler_type {
        let name = |h: Option<crate::boxes::FourCc>| h.map(|h| fourcc(&h)).unwrap_or_default();
        return Err(HeifError::invalid(format!(
            "external tile: handler_type '{}' differs from the tiled item's '{}' (§6.11.2)",
            name(tile_handler),
            name(handler_type)
        )));
    }
    Ok(item.id)
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

    /// Serialise as a `dref` entry `(flags, payload)` for external
    /// tiles (`external_tiles_urls = 1`, a 32-bit `no_of_input_items`;
    /// the offset / size codes and `sequential_order` are 0 — they
    /// describe an offset table this form does not have).
    pub fn external_entry(num_tiles: u32, tiles: &ExternalTiles) -> Result<(u32, Vec<u8>)> {
        for (what, s) in [
            ("baseurl", &tiles.base_url),
            ("urlextension", &tiles.url_extension),
            ("tileitemrequesttemplate", &tiles.request_template),
        ] {
            if s.contains('\0') {
                return Err(HeifError::invalid(format!(
                    "deti {what}: a utf8string cannot hold a NUL"
                )));
            }
        }
        let flags = (2 << 5) | (1 << 7);
        let mut b = num_tiles.to_be_bytes().to_vec();
        match tiles.directory_ids {
            Some((start, end)) => {
                b.push(1);
                b.extend_from_slice(&start.to_be_bytes());
                b.extend_from_slice(&end.to_be_bytes());
            }
            None => b.push(0),
        }
        b.extend_from_slice(&tiles.tile_id_start.to_be_bytes());
        for s in [
            &tiles.base_url,
            &tiles.url_extension,
            &tiles.request_template,
        ] {
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        }
        Ok((flags, b))
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
        // Table 12: the first tile of a 150-tile item (10 per
        // directory over 10..=25).
        assert_eq!(
            ext.url(0, 150).unwrap(),
            "http://cdn.example.com/pictures/134532/image/Representation1/10/1000.heif"
        );
        assert_eq!(ext.directory_id(149, 150).unwrap(), Some(24));
        // 151 tiles: the remainder goes to the last directory.
        assert_eq!(ext.directory_id(150, 151).unwrap(), Some(25));
        // 40 tiles: 2 per directory, the last one taking 10.
        assert_eq!(ext.directory_id(29, 40).unwrap(), Some(24));
        assert_eq!(ext.directory_id(30, 40).unwrap(), Some(25));
        assert_eq!(ext.directory_id(39, 40).unwrap(), Some(25));
        // Undefined splits.
        assert!(matches!(
            ext.directory_id(0, 14),
            Err(HeifError::Unsupported(_))
        ));
        let mut flat = ext.clone();
        flat.directory_ids = Some((5, 5));
        assert!(matches!(
            flat.directory_id(0, 14),
            Err(HeifError::Unsupported(_))
        ));
        flat.directory_ids = None;
        assert_eq!(
            flat.url(3, 4).unwrap(),
            "http://cdn.example.com/pictures/134532/image/Representation1/1003.heif"
        );
        flat.tile_id_start = u64::MAX;
        assert!(flat.url(1, 4).is_err(), "tile ID overflow");
        let r = ext.request(7, 13, 4, 150).unwrap();
        assert_eq!((r.column, r.row, r.tile_id), (1, 3, 1013));
        assert_eq!(r.directory_id, Some(11));
        assert!(ext.request(7, 150, 4, 150).is_err());
        // The serialised external entry parses back to the same fields.
        let (flags, payload) = DataEntryTiledItem::external_entry(150, ext).unwrap();
        let back = DataEntryTiledItem::parse(&entry(flags, payload)).unwrap();
        assert_eq!(back.external.as_ref(), Some(ext));
        assert_eq!(back.input_items, 150);
        assert!(back.offset_table.is_none());
    }
}
