//! Read-only probe (retail-gated, `retail` feature, needs `.sab-local.toml`): where, if anywhere, does a placed prop's collision live in its terrain cell?
//! Background: deleting five hall panels from the placement lists of cell 1700865 did not move the obstruction Sean walks into (docs/level_editor/collision_after_edit.md).
//! For those panels, a kept control panel and the slab (no collision, comparison), search EVERY record of the cell's sub-pack (zlib records inflated first) for each object's
//! tag bytes and its exact 12-byte Position. A hit outside the type-8 placement lists (and their type-4 mirrors) is evidence of a link, not proof of what the record does.
//! Run: cargo test --features retail --test collision_probe_retail -- --nocapture
use std::path::Path;

use sab_formats::game_paths::local_config_retail_dir;
use sab_formats::{cellplace, megapack, sbla, zlib};
use sab_tools::cells;

const CELL: u32 = 1_700_865;

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return Vec::new();
    }
    (0..=hay.len() - needle.len()).filter(|&i| &hay[i..i + needle.len()] == needle).collect()
}

#[test]
fn where_do_the_hall_props_appear_outside_the_placement_lists() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mounts = cells::france_mounts(&game).unwrap();
    let crc = cells::cell_crc(CELL);
    let m = mounts.iter().find(|m| m.find(crc).is_some()).expect("a mount holds the cell");
    let e = *m.find(crc).unwrap();
    let sub = megapack::entry_slice(&m.map, &e).unwrap();
    let sp = sbla::parse(sub, None).unwrap();
    let cell = cellplace::read(sub).unwrap().unwrap();
    let list_records: Vec<usize> = cell.lists.iter().map(|l| l.loc.record).collect();
    let mirror_records: Vec<usize> = cell.prefixes.iter().map(|p| p.loc.record).collect();

    // every record's bytes, inflated when stored compressed (zlib unless comp == uncomp)
    let mut recs: Vec<(usize, usize, Vec<u8>, bool)> = Vec::new(); // (index, kind, bytes, was_compressed)
    let mut inflate_failures = 0usize;
    let mut failed: std::collections::BTreeMap<(usize, bool), (usize, u64, u64)> = std::collections::BTreeMap::new(); // (kind, stored_empty) -> (count, comp, uncomp)
    for (i, (r, b)) in sp.records.iter().zip(sp.blobs.iter()).enumerate() {
        if r.comp == r.uncomp {
            recs.push((i, r.kind, b.to_vec(), false));
        } else {
            match zlib::inflate_exact(b, r.uncomp as usize, "record") {
                Ok(v) => recs.push((i, r.kind, v, true)),
                Err(_) => {
                    inflate_failures += 1;
                    let f = failed.entry((r.kind, r.comp == 0)).or_insert((0, 0, 0));
                    f.0 += 1;
                    f.1 += r.comp as u64;
                    f.2 += r.uncomp as u64;
                }
            }
        }
    }
    eprintln!("not searched (could not inflate), keyed (type, stored-with-no-bytes) -> (count, stored bytes, declared bytes): {failed:?}");
    let mut by_kind = std::collections::BTreeMap::new();
    for (_, k, b, c) in &recs {
        let e = by_kind.entry(*k).or_insert((0usize, 0usize, 0usize));
        e.0 += 1;
        e.1 += b.len();
        if *c {
            e.2 += 1;
        }
    }
    eprintln!("cell {CELL} (mount {}): {} records, inflate failures {inflate_failures}; per type (count, bytes after inflate, compressed): {by_kind:?}", m.rel, sp.records.len());
    eprintln!("placement-list records {list_records:?}; type-4 mirror records {mirror_records:?}");

    // the objects to look for, with their stored Position bytes taken from the placement list itself
    let targets: [(u32, &str); 7] = [
        (0x9ff4_daa4, "panel, DELETED (walk stop 1.05 m away)"),
        (0x2289_5093, "panel, DELETED"),
        (0xa794_9f0d, "panel, DELETED"),
        (0xe577_c516, "panel, DELETED"),
        (0x4dbe_000c, "panel, DELETED"),
        (0x1c96_23d1, "panel, KEPT (blocks the south-east walk)"),
        (0x80a9_bb53, "meat slab (no collision expected; comparison)"),
    ];
    for (tag, what) in targets {
        let (li, oi) = cell
            .lists
            .iter()
            .enumerate()
            .find_map(|(li, l)| l.objects.iter().position(|o| o.tag == tag).map(|oi| (li, oi)))
            .expect("tag in a placement list");
        let l = &cell.lists[li];
        let o = &l.objects[oi];
        let pos = o.params.iter().find(|p| p.tag == sab_formats::editnodes::tags::POSITION).expect("position");
        let pos_bytes = &sub[l.loc.off + pos.data_off..l.loc.off + pos.data_off + 12];
        let tag_bytes = tag.to_le_bytes();
        let mut tag_hits = Vec::new();
        let mut pos_hits = Vec::new();
        for (i, k, b, c) in &recs {
            if list_records.contains(i) || mirror_records.contains(i) {
                continue;
            }
            for off in find_all(b, &tag_bytes) {
                tag_hits.push(format!("rec {i} type {k}{} @0x{off:x}", if *c { " (inflated)" } else { "" }));
            }
            for off in find_all(b, pos_bytes) {
                pos_hits.push(format!("rec {i} type {k}{} @0x{off:x}", if *c { " (inflated)" } else { "" }));
            }
        }
        let p = sab_tools::cells::vec3(pos_bytes).unwrap();
        eprintln!(
            "{tag:08x} {what}: Position ({:.2}, {:.2}, {:.2}); OUTSIDE the placement lists: tag x{} {:?}; exact Position x{} {:?}",
            p[0], p[1], p[2], tag_hits.len(), &tag_hits[..tag_hits.len().min(6)], pos_hits.len(), &pos_hits[..pos_hits.len().min(6)]
        );
    }
}
