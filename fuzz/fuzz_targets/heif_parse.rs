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

fuzz_target!(|data: &[u8]| {
    let Ok(file) = HeifFile::parse(data) else {
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
        let _ = meta.thumbnails_of(it.id);
        let _ = meta.auxiliaries_of(it.id);
        let _ = meta.metadata_of(it.id);
    }
    let _ = file.primary_item();
    let _ = check(&file, MiafProfile::Miaf);
    let _ = check(&file, MiafProfile::HevcExtended);
});
