#![no_main]

//! Arbitrary bytes through the whole container surface: box walk,
//! `ftyp`, the `meta` tree, item payload resolution (every
//! construction method, multi-extent, `iloc`-reference chains), typed
//! properties, derivation graphs (cycle / depth / fan-out bombs), MIAF
//! checks. The contract is "the call returns".

use libfuzzer_sys::fuzz_target;
use oxideav_heif::derived::build_graph;
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::{HeifFile, ItemProperties};

/// The id lookups of a tree (id → position tables on a large one)
/// against the plain walk over its vectors, for a bounded set of ids:
/// the ones the tree names, their neighbours, and ids it does not hold.
fn lookups_equal_the_walk(meta: &oxideav_heif::Meta) {
    let mut ids: Vec<u32> = meta.items.iter().take(96).map(|i| i.id).collect();
    ids.extend(meta.locations.iter().take(32).map(|l| l.item_id));
    ids.extend(meta.associations.iter().take(32).map(|a| a.item_id));
    for r in meta.references.iter().take(32) {
        ids.push(r.from_item_id);
        ids.extend(r.to_item_ids.iter().take(8));
    }
    for g in meta.entity_groups.iter().take(16) {
        ids.push(g.group_id);
        ids.extend(g.entity_ids.iter().take(8));
    }
    let near: Vec<u32> = ids.iter().take(16).map(|i| i.wrapping_add(1)).collect();
    ids.extend(near);
    ids.extend([0, 1, u32::MAX]);
    ids.sort_unstable();
    ids.dedup();
    let types: Vec<[u8; 4]> = {
        let mut t: Vec<[u8; 4]> = meta.references.iter().take(64).map(|r| r.reference_type).collect();
        t.push(*b"dimg");
        t.sort_unstable();
        t.dedup();
        t.truncate(8);
        t
    };
    let group_types: Vec<[u8; 4]> = {
        let mut t: Vec<[u8; 4]> = meta.entity_groups.iter().take(64).map(|g| g.grouping_type).collect();
        t.sort_unstable();
        t.dedup();
        t.truncate(8);
        t
    };
    for id in ids {
        assert_eq!(
            meta.item(id).map(|i| i as *const _),
            meta.items.iter().find(|i| i.id == id).map(|i| i as *const _),
            "item {id}"
        );
        assert_eq!(
            meta.location(id).map(|l| l as *const _),
            meta.locations.iter().find(|l| l.item_id == id).map(|l| l as *const _),
            "location {id}"
        );
        let rows: Vec<oxideav_heif::PropertyAssociation> = meta
            .associations
            .iter()
            .filter(|a| a.item_id == id)
            .flat_map(|a| a.entries.iter().copied())
            .collect();
        assert_eq!(meta.associations_of(id), rows, "ipma rows of {id}");
        for t in &types {
            let from: Vec<u32> = meta
                .references
                .iter()
                .filter(|r| r.from_item_id == id && &r.reference_type == t)
                .flat_map(|r| r.to_item_ids.iter().copied())
                .collect();
            assert_eq!(meta.references_from(id, t), from, "references from {id}");
            let to: Vec<u32> = meta
                .references
                .iter()
                .filter(|r| &r.reference_type == t && r.to_item_ids.contains(&id))
                .map(|r| r.from_item_id)
                .collect();
            assert_eq!(meta.references_to(id, t), to, "references to {id}");
        }
        for t in &group_types {
            let groups: Vec<*const oxideav_heif::EntityGroup> = meta
                .entity_groups
                .iter()
                .filter(|g| &g.grouping_type == t && g.entity_ids.contains(&id))
                .map(|g| g as *const _)
                .collect();
            let got: Vec<*const oxideav_heif::EntityGroup> = meta
                .groups_containing(id, t)
                .into_iter()
                .map(|g| g as *const _)
                .collect();
            assert_eq!(got, groups, "groups holding {id}");
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(file) = HeifFile::parse_borrowed(data) else {
        return;
    };
    let _ = file.box_walk();
    let _ = file.file_type.classify();
    let _ = file.file_type.to_box();
    let Some(meta) = &file.meta else {
        return;
    };
    // Bound the per-input work: a hostile iinf can declare many items.
    for it in meta.items.iter().take(256) {
        let _ = file.item_data(it.id);
        let _ = file.item_file_spans(it.id);
        let _ = file.item_extents(it.id);
        let _ = ItemProperties::resolve(meta, it.id).map(|p| {
            let _ = p.output_size((64, 64), false);
            let _ = p.unsupported_essential();
            let _ = p.colrs();
        });
        if it.is_image() {
            if let Ok(node) = build_graph(&file, it.id) {
                let _ = node.output_size();
                let _ = node.coded_items();
                let _ = node.chain_types();
            }
        }
        if it.item_type == *b"tmap" {
            if let Ok(body) = file.item_data(it.id) {
                let _ = oxideav_heif::gainmap::GainMapMetadata::parse_tmap_body(&body);
            }
        }
        let _ = meta.thumbnails_of(it.id);
        let _ = meta.auxiliaries_of(it.id);
        let _ = meta.metadata_of(it.id);
    }
    let _ = file.primary_item();
    let _ = meta.display_order();
    lookups_equal_the_walk(meta);
    let _ = file.minimized.as_ref().map(|m| m.to_box());
    for d in meta.data_references.iter().take(16) {
        if let Ok(deti) = oxideav_heif::tiled::DataEntryTiledItem::parse(d) {
            let _ = deti.tile_spans(data, 4);
        }
    }
    // Re-muxing: every item (tili ones with their offset table and tipa
    // properties) through the writer's import.
    for it in meta.items.iter().take(16) {
        let mut w = oxideav_heif::HeifWriter::new();
        if let Ok(id) = w.import_item(&file, it.id) {
            w.set_primary(id);
            let _ = w.write_to_vec();
        }
    }
    for g in meta.entity_groups.iter().take(64) {
        let _ = (
            g.version,
            g.flags,
            meta.groups_containing(g.group_id, &g.grouping_type).len(),
        );
        let _ = g.pyramid();
        let _ = g.region_partition_area();
        let _ = g.stereo_with_fallback();
    }
    let _ = check(&file, MiafProfile::Miaf);
    let _ = check(&file, MiafProfile::HevcExtended);
});
