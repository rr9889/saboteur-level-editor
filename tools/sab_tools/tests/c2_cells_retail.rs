//! C2: terrain-cell identity and world mapping, checked on the retail install (game-gated, `retail` feature).
//!
//! Findings asserted here (evidence: asset bytes, this run):
//! * every terrain cell that carries placements has exactly one `France.map` tile whose `hash` equals the cell id
//!   (`Tile.hash == streamblock id`, 7669 of 7669);
//! * tile boxes are axis-aligned 60 m x 60 m squares on X/Z with a degenerate Y (0..0), aligned to multiples of 60;
//! * a placement lies inside its own cell's X/Z square (within a tolerance) except for a measured set of exceptions, which
//!   the test lists, so "cell ownership == position inside the box" is a rule with documented exceptions, not a law.
use std::collections::HashMap;
use std::path::Path;

use sab_formats::game_paths::local_config_retail_dir;
use sab_formats::map6;
use sab_tools::cells;

fn xz_inside(t: &map6::Tile, p: &[f32; 3], tol: f32) -> bool {
    [0usize, 2].iter().all(|&k| p[k] >= t.a[k].min(t.b[k]) - tol && p[k] <= t.a[k].max(t.b[k]) + tol)
}

#[test]
fn c2_cell_id_is_the_tile_hash_and_placements_fall_in_the_60m_square() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let lf = std::fs::read(game.join("France").join("loosefiles_BinPC.pack")).unwrap();
    let es = sab_formats::loosefiles::entries(&lf).unwrap();
    let fm = sab_formats::loosefiles::find(&es, "France.map").unwrap();
    let rm = map6::parse_region_main(&lf[fm.data.clone()]).unwrap();
    let tiles: HashMap<u32, &map6::Tile> = rm.tiles.iter().map(|t| (t.hash, t)).collect();
    assert_eq!(tiles.len(), rm.tiles.len(), "tile hashes are unique");

    let mut squares: HashMap<(i32, i32), Vec<(u32, i16)>> = HashMap::new();
    for t in &rm.tiles { squares.entry(((t.a[0].min(t.b[0]) / 60.0).round() as i32, (t.a[2].min(t.b[2]) / 60.0).round() as i32)).or_default().push((t.hash, t.sel)); }
    let multi = squares.values().filter(|v| v.len() > 1).count();
    eprintln!("tiles {}; distinct 60 m squares {}; squares holding more than one tile: {multi}; max tiles in one square: {}", rm.tiles.len(), squares.len(), squares.values().map(|v| v.len()).max().unwrap());
    if let Some(v) = squares.values().find(|v| v.len() > 1) { eprintln!("  example shared square: {v:?}"); }
    let mounts = cells::france_mounts(&game).unwrap();
    let (pl, _) = cells::scan_cells(&mounts).unwrap();
    let mut cells_seen: HashMap<u32, usize> = HashMap::new();
    for p in &pl {
        *cells_seen.entry(p.cell).or_default() += 1;
    }
    assert!(cells_seen.keys().all(|c| tiles.contains_key(c)), "every cell with placements has a tile whose hash is the cell id");

    // geometry of the boxes
    let mut non60 = 0;
    let mut off_grid = 0;
    for c in cells_seen.keys() {
        let t = tiles[c];
        let (dx, dz) = ((t.b[0] - t.a[0]).abs(), (t.b[2] - t.a[2]).abs());
        if (dx - 60.0).abs() > 1e-3 || (dz - 60.0).abs() > 1e-3 { non60 += 1; }
        if t.a[0].rem_euclid(60.0) > 1e-3 || t.a[2].rem_euclid(60.0) > 1e-3 { off_grid += 1; }
    }
    eprintln!("cells with placements {}; boxes not 60x60: {non60}; min corner not a multiple of 60: {off_grid}", cells_seen.len());

    // ownership: how many placements are inside their own cell's square, how far outside the rest are
    let (mut inside, mut outside, mut origin_like) = (0usize, 0usize, 0usize);
    let mut far: Vec<(f32, u32, [f32; 3])> = Vec::new();
    for p in &pl {
        let t = tiles[&p.cell];
        if xz_inside(t, &p.pos, 0.5) { inside += 1; continue; }
        outside += 1;
        if p.pos[1] == 0.0 && (p.pos[0].rem_euclid(60.0) < 1e-3 || p.pos[0] == 0.0) { origin_like += 1; }
        let d = [0usize, 2].iter().map(|&k| (t.a[k].min(t.b[k]) - p.pos[k]).max(p.pos[k] - t.a[k].max(t.b[k])).max(0.0)).fold(0.0f32, f32::max);
        far.push((d, p.cell, p.pos));
    }
    far.sort_by(|a, b| b.0.total_cmp(&a.0));
    eprintln!("placements {}: inside own 60 m square {inside}; outside {outside} (of which on the 60 m grid or at the origin, y == 0: {origin_like})", pl.len());
    for (d, c, p) in far.iter().take(5) { eprintln!("  farthest outside: {d:.1} m beyond cell {c} at {p:?}"); }
    let within_5 = far.iter().filter(|f| f.0 <= 5.0).count();
    eprintln!("outside but within 5 m of the square: {within_5}");
    // the lookup agrees with ownership: for placements inside their own square the cell is among cells_at(pos)
    let grid = cells::CellGrid::from_tiles(&rm.tiles);
    let (mut agree, mut checked) = (0usize, 0usize);
    for p in &pl {
        let t = tiles[&p.cell];
        if !xz_inside(t, &p.pos, 0.0) { continue; }
        checked += 1;
        if grid.cells_at(p.pos[0], p.pos[2]).contains(&p.cell) { agree += 1; }
    }
    eprintln!("CellGrid::cells_at contains the owning cell for {agree} of {checked} placements inside their square; {} squares", grid.square_count());
    assert!(agree * 1000 >= checked * 999, "lookup must agree with ownership for >= 99.9% (got {agree}/{checked})");
    assert!(inside * 100 / pl.len() >= 90, "the box rule must explain at least 90% of placements (measured: {inside} of {})", pl.len());
}

/// E2's cell: which France mounts hold its crc, and at what priority? (Override boundary: a higher-priority holder would shadow a patchmega1 edit.)
#[test]
fn e2_cell_1700865_is_held_by_which_mounts() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mounts = cells::france_mounts(&game).unwrap();
    let crc = cells::cell_crc(1700865);
    assert_eq!(crc, 0xc5ae_a62c, "the cell crc used by the staged E2 pack");
    let holders: Vec<(String, Option<u32>)> = mounts.iter().filter(|m| m.find(crc).is_some()).map(|m| (m.rel.clone(), m.priority)).collect();
    eprintln!("mounts: {:?}", mounts.iter().map(|m| (m.rel.clone(), m.priority, m.pack.entries.len())).collect::<Vec<_>>());
    eprintln!("holders of crc {crc:08x}: {holders:?}");
    assert!(holders.iter().any(|(r, _)| r.ends_with("Mega1.megapack")), "Mega1 holds the cell");
}

/// Streaming boundary for E2: the tiles sharing the slab's 60 m square, their `sel`, whether each has a pack entry, and how many placements each holds.
#[test]
fn e2_square_tiles_and_their_placements() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let lf = std::fs::read(game.join("France").join("loosefiles_BinPC.pack")).unwrap();
    let es = sab_formats::loosefiles::entries(&lf).unwrap();
    let fm = sab_formats::loosefiles::find(&es, "France.map").unwrap();
    let rm = map6::parse_region_main(&lf[fm.data.clone()]).unwrap();
    let mounts = cells::france_mounts(&game).unwrap();
    let (pl, _) = cells::scan_cells(&mounts).unwrap();
    let sq = cells::square_of(774.27, -683.04);
    eprintln!("slab square {sq:?} = x {}..{}, z {}..{}", sq.0 as f32 * 60.0, (sq.0 + 1) as f32 * 60.0, sq.1 as f32 * 60.0, (sq.1 + 1) as f32 * 60.0);
    for t in &rm.tiles {
        if cells::square_of(t.a[0].min(t.b[0]) + 0.01, t.a[2].min(t.b[2]) + 0.01) != sq { continue; }
        let id = t.hash;
        let held: Vec<String> = mounts.iter().filter(|m| m.find(cells::cell_crc(id)).is_some()).map(|m| m.rel.clone()).collect();
        let n = pl.iter().filter(|p| p.cell == id).count();
        let slab = pl.iter().filter(|p| p.cell == id && p.tag == 0x80a9_bb53).count();
        eprintln!("  tile {id}: sel {} held_by {held:?} placements {n} (slab tag 80a9bb53: {slab})", t.sel);
    }
}

/// Which cell-resident placements near the E2 area have a resolvable Lua name (editnodes::tag_name)? Those could be read back numerically in game.
#[test]
fn cell_placements_with_known_names_near_la_villette() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mounts = cells::france_mounts(&game).unwrap();
    let (pl, _) = cells::scan_cells(&mounts).unwrap();
    let near = |p: &cells::Placement| ((p.pos[0] - 776.0).powi(2) + (p.pos[2] + 686.0).powi(2)).sqrt() < 120.0 && p.pos[1] != 0.0;
    let all: Vec<&cells::Placement> = pl.iter().filter(|p| near(p)).collect();
    let named: Vec<&&cells::Placement> = all.iter().filter(|p| sab_formats::editnodes::tag_name(p.tag).is_some()).collect();
    let total_named = pl.iter().filter(|p| sab_formats::editnodes::tag_name(p.tag).is_some()).count();
    eprintln!("placements within 120 m: {}; with a resolvable name: {}; named across ALL cells: {} of {}", all.len(), named.len(), total_named, pl.len());
    let mut classes: HashMap<String, usize> = HashMap::new();
    for p in &all { *classes.entry(p.class_name.clone()).or_default() += 1; }
    let mut cv: Vec<_> = classes.into_iter().collect(); cv.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!("classes near: {:?}", &cv[..cv.len().min(10)]);
    for p in named.iter().take(15) { eprintln!("  cell {} {:?} class {} name {}", p.cell, p.pos, p.class_name, sab_formats::editnodes::tag_name(p.tag).unwrap()); }
}

/// Candidate cells for more object-editing A/B runs (selection aid, not a census tool): near La Villette, not held by another patch pack, lists unmirrored,
/// with several placements of one visible static-prop class that carry all three basis leaves, close to the ground.
#[test]
fn candidate_cells_for_object_editing_runs() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mounts = cells::france_mounts(&game).unwrap();
    let (pl, _) = cells::scan_cells(&mounts).unwrap();
    let held_by_patch: std::collections::HashSet<u32> = mounts.iter().filter(|m| m.rel.contains("patchmega")).flat_map(|m| m.pack.entries.iter().map(|e| e.crc)).collect();
    let mut by_cell_class: HashMap<(u32, String), Vec<&cells::Placement>> = HashMap::new();
    for p in &pl {
        let d = ((p.pos[0] - 776.0).powi(2) + (p.pos[2] + 686.0).powi(2)).sqrt();
        if d < 140.0 && p.pos[1] > 40.0 && p.pos[1] < 70.0 && !p.class_name.starts_with("occ") && p.class_name != "Locator" && p.class_name != "Occluder" {
            by_cell_class.entry((p.cell, p.class_name.clone())).or_default().push(p);
        }
    }
    let mut rows: Vec<_> = by_cell_class.into_iter().filter(|(_, v)| v.len() >= 6).collect();
    rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    for ((cell, class), v) in rows.iter().take(14) {
        let patch = held_by_patch.contains(&cells::cell_crc(*cell));
        let c = v[0].pos;
        eprintln!("cell {cell} class {class}: {} placements, patch-held {patch}, first at ({:.1},{:.1},{:.1}), lists {}", v.len(), c[0], c[1], c[2], v[0].lists_in_cell);
    }
}

#[test]
fn cell_1963009_object_listing() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mounts = cells::france_mounts(&game).unwrap();
    let (pl, counts) = cells::scan_cells(&mounts).unwrap();
    let t = mounts.iter().flat_map(|m| m.pack.entries.iter()).count();
    eprintln!("(mount entries {t})");
    let mut v: Vec<&cells::Placement> = pl.iter().filter(|p| p.cell == 1963009 && !p.class_name.starts_with("occ") && p.class_name != "Occluder" && p.class_name != "Locator").collect();
    v.sort_by(|a, b| a.pos[0].total_cmp(&b.pos[0]));
    for p in v.iter().take(60) { eprintln!("  list {} tag {:08x} {:<34} ({:.1},{:.1},{:.1}) occ {}", p.list, p.tag, p.class_name, p.pos[0], p.pos[1], p.pos[2], counts.get(&p.tag).copied().unwrap_or(0)); }
    eprintln!("total non-occluder placements in the cell: {}", v.len());
}

#[test]
fn hall_objects_near_the_slab_stand() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mounts = cells::france_mounts(&game).unwrap();
    let (pl, _) = cells::scan_cells(&mounts).unwrap();
    let mut v: Vec<&cells::Placement> = pl.iter().filter(|p| p.cell == 1700865 && ((p.pos[0] - 776.78).powi(2) + (p.pos[2] + 686.09).powi(2)).sqrt() < 14.0 && p.pos[1] > 50.0 && p.pos[1] < 66.0 && p.class_name != "Locator" && p.class_name != "Occluder" && !p.class_name.starts_with("decal") && !p.class_name.to_ascii_lowercase().starts_with("occ")).collect();
    v.sort_by(|a, b| a.class_name.cmp(&b.class_name));
    for p in &v { eprintln!("  list {} tag {:08x} {:<40} ({:.1},{:.1},{:.1})", p.list, p.tag, p.class_name, p.pos[0], p.pos[1], p.pos[2]); }
    eprintln!("count {}", v.len());
}

#[test]
fn placements_near_the_west_walk_stop_point() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mounts = cells::france_mounts(&game).unwrap();
    let (pl, _) = cells::scan_cells(&mounts).unwrap();
    let deleted: [u32; 5] = [0x9ff4_daa4, 0x2289_5093, 0xa794_9f0d, 0xe577_c516, 0x4dbe_000c];
    let mut v: Vec<(f32, &cells::Placement)> = pl.iter().filter(|p| p.pos[1] > 50.0 && p.pos[1] < 62.0).map(|p| ((((p.pos[0] - 770.39).powi(2) + (p.pos[2] + 686.55).powi(2)) as f32).sqrt(), p)).filter(|(d, _)| *d < 2.5).collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (d, p) in &v { eprintln!("  {:.2} m  cell {} {:<36} tag {:08x} ({:.2},{:.2},{:.2}){}", d, p.cell, p.class_name, p.tag, p.pos[0], p.pos[1], p.pos[2], if deleted.contains(&p.tag) { "  [DELETED in the walk package]" } else { "" }); }
    eprintln!("count {}", v.len());
}
