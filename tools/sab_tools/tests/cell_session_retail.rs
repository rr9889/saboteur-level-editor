//! Terrain-cell editing through the placement Session (game-gated, `retail` feature, needs `.sab-local.toml`).
//! The headline check: the session-authored export of the E2b slab move is BYTE-IDENTICAL to the `sab_tools m0 stage-e2` pack that was seen working in game
//! (output/m0/e2b, `docs/level_editor/e2b_terrain_cell_result.md`), so the in-game evidence of that pack applies to the Workshop-authored one.
use std::path::{Path, PathBuf};

use sab_formats::editnodes::tags;
use sab_formats::game_paths::local_config_retail_dir;
use sab_tools::cellsession::{open_cell, stage_cell};

const CELL: u32 = 1_700_865;
const SLAB: u32 = 0x80a9_bb53;

fn game() -> PathBuf {
    local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"))
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sab_cellsession_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn slab_pos(s: &sab_tools::session::Session, c: u32, i: usize) -> [f32; 3] {
    s.entry(c).unwrap().objects()[i].position().unwrap()
}

#[test]
fn a_session_move_of_the_slab_exports_the_pack_that_worked_in_game() {
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, idx) = edit.find(SLAB).expect("the slab is in exactly one list of its cell");
    let c = edit.containers[li];
    let old = slab_pos(&s, c, idx);
    assert!((old[1] - 54.4976).abs() < 1e-3, "retail slab Y {old:?}");
    s.move_object(c, idx, [old[0], old[1] + 4.0, old[2]]).unwrap();
    assert_eq!(s.active().len(), 1, "one undoable Position edit");

    let out = tmp("e2b");
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass, "{}", o.report);
    assert!(o.report.contains(sab_tools::cellsession::COLLISION_CAVEAT), "every cell export report carries the collision caveat:
{}", o.report);
    assert_eq!(o.slot_name, "patchmega1.megapack");
    assert_eq!(o.moved, 1);

    let e2b = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("e2b").join("patchmega1.megapack");
    if e2b.is_file() {
        let want = std::fs::read(&e2b).unwrap();
        let got = std::fs::read(out.join("patchmega1.megapack")).unwrap();
        assert_eq!(sab_tools::stage::sha256(&got), sab_tools::stage::sha256(&want), "session-authored pack == the CLI-staged pack seen in game");
    } else {
        eprintln!("(output/m0/e2b not present: byte-identity to the in-game pack was not checked)");
    }
    assert!(out.join("install.ps1").is_file() && out.join("restore.ps1").is_file() && out.join("manifest.json").is_file());
    // undo returns the session to retail and the export then refuses (nothing to export)
    assert!(s.undo().unwrap());
    assert_eq!(slab_pos(&s, c, idx), old);
    assert!(stage_cell(&game, &edit, &s, &tmp("noop")).unwrap_err().contains("nothing to export"));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn rotation_is_exported_as_exactly_the_three_basis_leaves_and_class_edits_are_refused() {
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, idx) = edit.find(SLAB).unwrap();
    let c = edit.containers[li];
    let o0 = &s.entry(c).unwrap().objects()[idx];
    let axes0 = [o0.vec3_param(tags::X_AXIS).unwrap(), o0.vec3_param(tags::Y_AXIS).unwrap(), o0.vec3_param(tags::Z_AXIS).unwrap()];
    // intended values computed here, without the editor: a 45 degree turn about world X
    let (sn, cs) = (45f32.to_radians().sin(), 45f32.to_radians().cos());
    let rx = |v: [f32; 3]| [v[0], cs * v[1] - sn * v[2], sn * v[1] + cs * v[2]];
    let want = [rx(axes0[0]), rx(axes0[1]), rx(axes0[2])];
    s.rotate_objects(&[(c, idx)], [[1.0, 0.0, 0.0], [0.0, cs, -sn], [0.0, sn, cs]], None).unwrap();
    let out = tmp("rot");
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass, "{}", o.report);
    assert!(o.report.matches("leaf @").count() >= 1 && o.report.matches("leaf @").count() <= 3, "only basis leaves changed:\n{}", o.report);
    // decode the exported megapack and compare the slab's basis with the intended values; every other object byte-identical
    let mp = std::fs::read(out.join("patchmega1.megapack")).unwrap();
    let pk = sab_formats::megapack::parse(&mp).unwrap();
    let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
    let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
    let l = &cell.lists[li];
    let blob = &sub[l.loc.off..l.loc.off + l.loc.comp];
    let nodes = sab_formats::editnodes::parse_entry(blob).unwrap();
    for (k, tag) in [tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS].into_iter().enumerate() {
        let a = nodes[idx].vec3_param(tag).unwrap();
        assert!((0..3).all(|j| (a[j] - want[k][j]).abs() < 1e-5), "axis {k}: {a:?} vs {:?}", want[k]);
    }
    assert_eq!(nodes[idx].position().unwrap(), s.entry(c).unwrap().objects()[idx].position().unwrap(), "Position untouched by a rotation");
    let (_, orig) = open_cell(&game, CELL).unwrap();
    for i in (0..nodes.len()).filter(|&i| i != idx) {
        assert_eq!(nodes[i], orig.entry(c).unwrap().objects()[i], "neighbour {i} unchanged");
    }
    std::fs::remove_dir_all(&out).ok();

    // a class edit is still refused at export (removal is supported since the rebuild path: see the delete test)
    let (_, mut s2) = open_cell(&game, CELL).unwrap();
    s2.set_class_name(c, idx, "P_INT_RoundTable_Big").unwrap();
    assert!(stage_cell(&game, &edit, &s2, &tmp("cls")).unwrap_err().contains("duplicates and deletions only"));
}

#[test]
fn a_cell_held_by_the_retail_patch_pack_is_refused() {
    // cell 1831937 is one of the five cells patchmega.megapack holds (docs/formats/cell_placements.md): a new patch pack could not override it
    let e = open_cell(&game(), 1_831_937).err().expect("must refuse");
    assert!(e.contains("patch pack") || e.contains("several packs"), "{e}");
    // an id no pack holds
    assert!(open_cell(&game(), 99_999_999).err().unwrap().contains("no mounted pack"));
}

#[test]
fn the_slab_is_the_only_object_that_differs_in_the_exported_cell() {
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, idx) = edit.find(SLAB).unwrap();
    let c = edit.containers[li];
    let old = slab_pos(&s, c, idx);
    s.move_object(c, idx, [old[0] + 1.0, old[1], old[2]]).unwrap();
    let out = tmp("only");
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass);
    assert_eq!(o.report.matches("leaf @").count(), 1, "exactly one leaf changed:\n{}", o.report);
    let _ = tags::POSITION;
    std::fs::remove_dir_all(&out).ok();
}

/// The rebuild path serialises placement lists from parsed nodes. This sweep proves that is byte-exact for EVERY list of every base terrain cell (the same self-test `open_cell` runs).
#[test]
fn every_cell_placement_list_in_the_game_re_serializes_byte_exactly() {
    use sab_formats::{cellplace, editnodes, megapack, sbla};
    let game = game();
    let mounts = sab_tools::cells::france_mounts(&game).unwrap();
    let (mut cells, mut lists, mut objects, mut bad) = (0usize, 0usize, 0usize, Vec::new());
    for m in mounts.iter().filter(|m| m.rel.contains(r"\Mega")) {
        for e in &m.pack.entries {
            let sub = megapack::entry_slice(&m.map, e).unwrap();
            if sbla::descriptor_len(sub).unwrap() < 0xB4 {
                continue;
            }
            let Some(cell) = cellplace::read(sub).unwrap() else { continue };
            cells += 1;
            for (li, l) in cell.lists.iter().enumerate() {
                let blob = &sub[l.loc.off..l.loc.off + l.loc.comp];
                lists += 1;
                objects += l.objects.len();
                match editnodes::parse_entry(blob) {
                    Ok(nodes) if sab_tools::cellsession::list_body(&nodes) == blob => {}
                    _ => bad.push((e.index, li)),
                }
            }
        }
    }
    eprintln!("cells {cells}, lists {lists}, objects {objects}, lists that do not round-trip: {}", bad.len());
    assert!(bad.is_empty(), "first failures: {:?}", &bad[..bad.len().min(10)]);
}

/// Delete: the slab removed through the session exports a rebuilt cell: one object fewer in its list, the slab gone, every other object byte-identical and in order, every other blob identical.
#[test]
fn deleting_the_slab_exports_a_rebuilt_cell_without_it() {
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, idx) = edit.find(SLAB).unwrap();
    let c = edit.containers[li];
    let before: Vec<_> = s.entry(c).unwrap().objects().to_vec();
    s.remove_objects(&[(c, idx)]).unwrap();
    let out = tmp("del");
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass, "{}", o.report);
    assert!(o.report.contains("REBUILD") && o.report.contains(&format!("{} -> {} objects", before.len(), before.len() - 1)), "{}", o.report);
    let mp = std::fs::read(out.join("patchmega1.megapack")).unwrap();
    let pk = sab_formats::megapack::parse(&mp).unwrap();
    let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
    let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
    let l = &cell.lists[li];
    let nodes = sab_formats::editnodes::parse_entry(&sub[l.loc.off..l.loc.off + l.loc.comp]).unwrap();
    assert_eq!(nodes.len(), before.len() - 1);
    assert!(!nodes.iter().any(|n| n.tag() == SLAB), "the slab is gone");
    let want: Vec<_> = before.iter().filter(|n| n.tag() != SLAB).collect();
    assert!(nodes.iter().zip(want).all(|(a, b)| a == b), "every other object byte-identical, same order");
    std::fs::remove_dir_all(&out).ok();
}

/// Duplicate: a copy of the slab with a freshly minted tag exports a rebuilt cell: one object more, the original unchanged, the copy a distinct tag with the same class and the shifted Position.
#[test]
fn duplicating_the_slab_exports_a_rebuilt_cell_with_a_unique_copy() {
    use sab_formats::editnodes::Node;
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, idx) = edit.find(SLAB).unwrap();
    let c = edit.containers[li];
    let before: Vec<_> = s.entry(c).unwrap().objects().to_vec();
    let (new_tag, _) = s.mint_tag(c, SLAB);
    assert_ne!(new_tag, SLAB);
    let Node::Container { children, .. } = &before[idx] else { panic!("object is a container") };
    let copy = Node::Container { tag: new_tag, children: children.clone() };
    s.insert_objects(vec![(c, idx + 1, copy)], "Duplicate").unwrap();
    let old = slab_pos(&s, c, idx);
    s.move_object(c, idx + 1, [old[0] + 1.0, old[1], old[2]]).unwrap();
    let out = tmp("dup");
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass, "{}", o.report);
    assert!(o.report.contains("REBUILD") && o.report.contains("collide with none"), "{}", o.report);
    let mp = std::fs::read(out.join("patchmega1.megapack")).unwrap();
    let pk = sab_formats::megapack::parse(&mp).unwrap();
    let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
    let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
    let l = &cell.lists[li];
    let nodes = sab_formats::editnodes::parse_entry(&sub[l.loc.off..l.loc.off + l.loc.comp]).unwrap();
    assert_eq!(nodes.len(), before.len() + 1);
    assert_eq!(nodes[idx], before[idx], "the original slab is byte-identical");
    assert_eq!(nodes[idx + 1].tag(), new_tag);
    assert_eq!(nodes[idx + 1].class_name(), before[idx].class_name(), "same class");
    assert_eq!(nodes[idx + 1].position().unwrap(), [old[0] + 1.0, old[1], old[2]], "copy shifted 1 m on X");
    for (i, n) in before.iter().enumerate().filter(|(i, _)| *i != idx) {
        let j = if i > idx { i + 1 } else { i };
        assert_eq!(&nodes[j], n, "neighbour {i} unchanged");
    }
    std::fs::remove_dir_all(&out).ok();
}

/// Second cell, four operations on four distinct objects of different classes in ONE package (kept in output/m0/cell_multi_1963009 for the live A/B):
/// move a crate +3 m, rotate a fence 90 degrees about Y, delete a stacked crate, duplicate a small crate with the copy 2.5 m above it.
/// Every effect is decoded from the exported megapack and compared with values computed here; every other object must be byte-identical to retail.
#[test]
fn second_cell_four_operations_in_one_package_decode_exactly() {
    use sab_formats::editnodes::Node;
    const C2: u32 = 1_963_009;
    let (t_move, t_rot, t_del, t_dup) = (0x53e0_3abe_u32, 0x1ce7_b47f_u32, 0x9d96_f305_u32, 0xcd3f_e849_u32);
    let game = game();
    let (edit, mut s) = open_cell(&game, C2).unwrap();
    let find = |s: &sab_tools::session::Session, c: u32, tag: u32| s.entry(c).unwrap().objects().iter().position(|o| o.tag() == tag).expect("tag present");
    let (li, _) = edit.find(t_move).expect("crate in the cell");
    for t in [t_rot, t_del, t_dup] {
        assert_eq!(edit.find(t).map(|x| x.0), Some(li), "all four objects are in the same placement list");
    }
    let c = edit.containers[li];
    let before: Vec<Node> = s.entry(c).unwrap().objects().to_vec();
    let idx0 = |t: u32| before.iter().position(|o| o.tag() == t).unwrap();
    let pos_move = before[idx0(t_move)].position().unwrap();
    let axes = |n: &Node| [n.vec3_param(tags::X_AXIS).unwrap(), n.vec3_param(tags::Y_AXIS).unwrap(), n.vec3_param(tags::Z_AXIS).unwrap()];
    let ax_rot = axes(&before[idx0(t_rot)]);
    let pos_dup = before[idx0(t_dup)].position().unwrap();

    // 1 move
    s.move_object(c, find(&s, c, t_move), [pos_move[0], pos_move[1] + 3.0, pos_move[2]]).unwrap();
    // 2 rotate: 90 degrees about world Y. R = [[0,0,1],[0,1,0],[-1,0,0]] applied as R*v
    let ry = |v: [f32; 3]| [v[2], v[1], -v[0]];
    s.rotate_objects(&[(c, find(&s, c, t_rot))], [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]], None).unwrap();
    // 3 delete
    s.remove_objects(&[(c, find(&s, c, t_del))]).unwrap();
    // 4 duplicate, then lift the copy
    let (new_tag, _) = s.mint_tag(c, t_dup);
    let di = find(&s, c, t_dup);
    let Node::Container { children, .. } = &s.entry(c).unwrap().objects()[di].clone() else { panic!() };
    s.insert_objects(vec![(c, di + 1, Node::Container { tag: new_tag, children: children.clone() })], "Duplicate").unwrap();
    s.move_object(c, di + 1, [pos_dup[0], pos_dup[1] + 2.5, pos_dup[2]]).unwrap();

    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("cell_multi_1963009");
    let _ = std::fs::remove_dir_all(&out);
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass, "{}", o.report);
    assert!(o.report.contains("REBUILD"), "{}", o.report);
    let mp = std::fs::read(out.join("patchmega1.megapack")).unwrap();
    let pk = sab_formats::megapack::parse(&mp).unwrap();
    let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
    let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
    let l = &cell.lists[li];
    let nodes = sab_formats::editnodes::parse_entry(&sub[l.loc.off..l.loc.off + l.loc.comp]).unwrap();
    assert_eq!(nodes.len(), before.len(), "one deleted, one duplicated");
    let get = |t: u32| nodes.iter().find(|n| n.tag() == t);
    // move
    assert_eq!(get(t_move).unwrap().position().unwrap(), [pos_move[0], pos_move[1] + 3.0, pos_move[2]]);
    // rotate
    let a = axes(get(t_rot).unwrap());
    for (k, v) in ax_rot.iter().enumerate() {
        let w = ry(*v);
        assert!((0..3).all(|j| (a[k][j] - w[j]).abs() < 1e-5), "fence axis {k}: {:?} vs {w:?}", a[k]);
    }
    assert_eq!(get(t_rot).unwrap().position().unwrap(), before[idx0(t_rot)].position().unwrap(), "rotation leaves the fence's Position alone");
    // delete
    assert!(get(t_del).is_none(), "the stacked crate is gone");
    // duplicate
    assert_eq!(get(t_dup).unwrap(), &before[idx0(t_dup)], "the original small crate is byte-identical");
    let cp = get(new_tag).expect("the copy exists");
    assert_eq!(cp.class_name(), before[idx0(t_dup)].class_name());
    assert_eq!(cp.position().unwrap(), [pos_dup[0], pos_dup[1] + 2.5, pos_dup[2]]);
    // everything else byte-identical
    let touched = [t_move, t_rot, t_del, t_dup, new_tag];
    for b in before.iter().filter(|b| !touched.contains(&b.tag())) {
        assert_eq!(get(b.tag()).unwrap(), b, "untouched object {:08x} unchanged", b.tag());
    }
    eprintln!("{}", o.report);
}

/// Second cell, street furniture (open sidewalk, no railings): move a bench +3 m Y, rotate a refuse bin 90 degrees about X, delete a street tree, duplicate another tree with the copy 3 m south.
/// Kept in output/m0/cell_multi_street_1963009 for the live A/B; every effect is decoded from the export and every other object must be byte-identical to retail.
#[test]
fn second_cell_street_furniture_four_operations_decode_exactly() {
    use sab_formats::editnodes::Node;
    const C2: u32 = 1_963_009;
    let (t_move, t_rot, t_del, t_dup) = (0x906e_5a7c_u32, 0xc08e_0f41_u32, 0x7728_bc73_u32, 0xb4aa_354a_u32);
    let game = game();
    let (edit, mut s) = open_cell(&game, C2).unwrap();
    let (li, _) = edit.find(t_move).expect("bench in the cell");
    for t in [t_rot, t_del, t_dup] {
        assert_eq!(edit.find(t).map(|x| x.0), Some(li), "all four objects are in the same placement list");
    }
    let c = edit.containers[li];
    let find = |s: &sab_tools::session::Session, tag: u32| s.entry(c).unwrap().objects().iter().position(|o| o.tag() == tag).expect("tag present");
    let before: Vec<Node> = s.entry(c).unwrap().objects().to_vec();
    let idx0 = |t: u32| before.iter().position(|o| o.tag() == t).unwrap();
    let pos_move = before[idx0(t_move)].position().unwrap();
    let axes = |n: &Node| [n.vec3_param(tags::X_AXIS).unwrap(), n.vec3_param(tags::Y_AXIS).unwrap(), n.vec3_param(tags::Z_AXIS).unwrap()];
    let ax_rot = axes(&before[idx0(t_rot)]);
    let pos_dup = before[idx0(t_dup)].position().unwrap();

    s.move_object(c, find(&s, t_move), [pos_move[0], pos_move[1] + 3.0, pos_move[2]]).unwrap();
    // 90 degrees about world X: R = [[1,0,0],[0,0,-1],[0,1,0]] applied as R*v
    let rx = |v: [f32; 3]| [v[0], -v[2], v[1]];
    s.rotate_objects(&[(c, find(&s, t_rot))], [[1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]], None).unwrap();
    s.remove_objects(&[(c, find(&s, t_del))]).unwrap();
    let (new_tag, _) = s.mint_tag(c, t_dup);
    let di = find(&s, t_dup);
    let Node::Container { children, .. } = &s.entry(c).unwrap().objects()[di].clone() else { panic!() };
    s.insert_objects(vec![(c, di + 1, Node::Container { tag: new_tag, children: children.clone() })], "Duplicate").unwrap();
    s.move_object(c, di + 1, [pos_dup[0], pos_dup[1], pos_dup[2] - 3.0]).unwrap();

    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("cell_multi_street_1963009");
    let _ = std::fs::remove_dir_all(&out);
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass && o.report.contains("REBUILD"), "{}", o.report);
    let mp = std::fs::read(out.join("patchmega1.megapack")).unwrap();
    let pk = sab_formats::megapack::parse(&mp).unwrap();
    let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
    let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
    let l = &cell.lists[li];
    let nodes = sab_formats::editnodes::parse_entry(&sub[l.loc.off..l.loc.off + l.loc.comp]).unwrap();
    assert_eq!(nodes.len(), before.len());
    let get = |t: u32| nodes.iter().find(|n| n.tag() == t);
    assert_eq!(get(t_move).unwrap().position().unwrap(), [pos_move[0], pos_move[1] + 3.0, pos_move[2]]);
    let a = axes(get(t_rot).unwrap());
    for (k, v) in ax_rot.iter().enumerate() {
        let w = rx(*v);
        assert!((0..3).all(|j| (a[k][j] - w[j]).abs() < 1e-5), "bin axis {k}: {:?} vs {w:?}", a[k]);
    }
    assert!(get(t_del).is_none(), "the tree is gone");
    assert_eq!(get(t_dup).unwrap(), &before[idx0(t_dup)], "the original tree is byte-identical");
    let cp = get(new_tag).expect("the copy exists");
    assert_eq!(cp.class_name(), before[idx0(t_dup)].class_name());
    assert_eq!(cp.position().unwrap(), [pos_dup[0], pos_dup[1], pos_dup[2] - 3.0]);
    let touched = [t_move, t_rot, t_del, t_dup, new_tag];
    for b in before.iter().filter(|b| !touched.contains(&b.tag())) {
        assert_eq!(get(b.tag()).unwrap(), b, "untouched object {:08x} unchanged", b.tag());
    }
    eprintln!("moved bench from {pos_move:?}; bin at {:?}; deleted tree at {:?}; tree to copy at {pos_dup:?}", before[idx0(t_rot)].position().unwrap(), before[idx0(t_del)].position().unwrap());
}

/// Persistence of a terrain-cell editing project: author move + rotate + delete + duplicate on the hall's slabs (cell 1700865), save, reopen, and the reopened session must have the
/// same journal, the same undo behaviour and export the BYTE-IDENTICAL patch pack. A project whose recorded source pack hash differs is refused, and an ordinary load refuses a cell project.
#[test]
fn a_terrain_cell_project_saves_reopens_and_exports_the_same_pack() {
    use sab_formats::editnodes::Node;
    use sab_tools::cellsession::{cell_project_id, open_cell_project, save_cell_project};
    let (t_move, t_rot, t_del, t_dup) = (0x80a9_bb53_u32, 0x9c8b_4dc2_u32, 0x9879_2863_u32, 0xe97d_f01e_u32);
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, _) = edit.find(t_move).unwrap();
    let c = edit.containers[li];
    let find = |s: &sab_tools::session::Session, tag: u32| s.entry(c).unwrap().objects().iter().position(|o| o.tag() == tag).expect("tag present");
    let p0 = s.entry(c).unwrap().objects()[find(&s, t_move)].position().unwrap();
    s.move_object(c, find(&s, t_move), [p0[0], p0[1] + 4.0, p0[2]]).unwrap();
    s.rotate_objects(&[(c, find(&s, t_rot))], [[1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]], None).unwrap();
    s.remove_objects(&[(c, find(&s, t_del))]).unwrap();
    let (new_tag, _) = s.mint_tag(c, t_dup);
    let di = find(&s, t_dup);
    let Node::Container { children, .. } = &s.entry(c).unwrap().objects()[di].clone() else { panic!() };
    s.insert_objects(vec![(c, di + 1, Node::Container { tag: new_tag, children: children.clone() })], "Duplicate").unwrap();
    // leave one edit undone, so the undo cursor is not at the end
    s.move_object(c, di + 1, [p0[0], p0[1], p0[2] + 1.0]).unwrap();
    assert!(s.undo().unwrap());

    let dir = tmp("persist");
    std::fs::create_dir_all(&dir).unwrap();
    let proj = dir.join("cell.sabproject.json");
    save_cell_project(&edit, &s, &proj).unwrap();
    assert_eq!(cell_project_id(&proj), Some(CELL));
    assert!(sab_tools::session::Session::load(&proj).is_err(), "an ordinary load refuses a terrain-cell project");

    let (edit2, s2) = open_cell_project(&proj).unwrap();
    assert_eq!(edit2.cell, CELL);
    assert_eq!(s2.to_project_json()["edits"], s.to_project_json()["edits"], "same journal");
    assert_eq!(s2.to_project_json()["cursor"], s.to_project_json()["cursor"], "same undo cursor");
    assert_eq!(s2.active().len(), s.active().len());
    assert_eq!(s2.entry(c).unwrap().objects(), s.entry(c).unwrap().objects(), "same objects");
    let a = stage_cell(&game, &edit, &s, &dir.join("a")).unwrap();
    let b = stage_cell(&game, &edit2, &s2, &dir.join("b")).unwrap();
    assert!(a.all_pass && b.all_pass, "{}\n{}", a.report, b.report);
    assert_eq!(a.artifact_sha, b.artifact_sha, "the reopened project exports the byte-identical pack");
    // redo works on the reopened session (the undone edit is still in the journal)
    let mut s3 = open_cell_project(&proj).unwrap().1;
    assert!(s3.redo().unwrap(), "the undone edit comes back");

    // a changed source pack is refused
    let mut v = sab_tools::session::read_project(&proj).unwrap();
    v["terrain_cell"]["source_sha256"] = serde_json::json!("00".repeat(32));
    let bad = dir.join("bad.sabproject.json");
    std::fs::write(&bad, serde_json::to_string(&v).unwrap()).unwrap();
    assert!(open_cell_project(&bad).err().unwrap().contains("changed since this project was saved"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Hall props of other classes than the slab (panels `MN_LaVillette_panels01`, debris `pglobala_debris_wood_wb`), visible in the retail control frames of the slab runs
/// (the dark bench-like fixtures at the right of the frame). Rotate panel a7949f0d 90 degrees about X, move panel e577c516 +3 m Y, delete panel 9ff4daa4, duplicate debris 3d639467
/// with the copy 3 m up. Kept in output/m0/cell_multi_hall_1700865 for the live A/B; every effect decoded from the export, every other object byte-identical.
#[test]
fn hall_props_four_operations_decode_exactly() {
    use sab_formats::editnodes::Node;
    let (t_rot, t_move, t_del, t_dup) = (0xa794_9f0d_u32, 0xe577_c516_u32, 0x9ff4_daa4_u32, 0x3d63_9467_u32);
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, _) = edit.find(t_rot).expect("panel in the cell");
    for t in [t_move, t_del, t_dup] {
        assert_eq!(edit.find(t).map(|x| x.0), Some(li), "same placement list");
    }
    let c = edit.containers[li];
    let find = |s: &sab_tools::session::Session, tag: u32| s.entry(c).unwrap().objects().iter().position(|o| o.tag() == tag).expect("tag present");
    let before: Vec<Node> = s.entry(c).unwrap().objects().to_vec();
    let idx0 = |t: u32| before.iter().position(|o| o.tag() == t).unwrap();
    let pos_move = before[idx0(t_move)].position().unwrap();
    let axes = |n: &Node| [n.vec3_param(tags::X_AXIS).unwrap(), n.vec3_param(tags::Y_AXIS).unwrap(), n.vec3_param(tags::Z_AXIS).unwrap()];
    let ax_rot = axes(&before[idx0(t_rot)]);
    let pos_dup = before[idx0(t_dup)].position().unwrap();

    s.rotate_objects(&[(c, find(&s, t_rot))], [[1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]], None).unwrap();
    s.move_object(c, find(&s, t_move), [pos_move[0], pos_move[1] + 3.0, pos_move[2]]).unwrap();
    s.remove_objects(&[(c, find(&s, t_del))]).unwrap();
    let (new_tag, _) = s.mint_tag(c, t_dup);
    let di = find(&s, t_dup);
    let Node::Container { children, .. } = &s.entry(c).unwrap().objects()[di].clone() else { panic!() };
    s.insert_objects(vec![(c, di + 1, Node::Container { tag: new_tag, children: children.clone() })], "Duplicate").unwrap();
    s.move_object(c, di + 1, [pos_dup[0], pos_dup[1] + 3.0, pos_dup[2]]).unwrap();

    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("cell_multi_hall_1700865");
    let _ = std::fs::remove_dir_all(&out);
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass && o.report.contains("REBUILD"), "{}", o.report);
    let mp = std::fs::read(out.join("patchmega1.megapack")).unwrap();
    let pk = sab_formats::megapack::parse(&mp).unwrap();
    let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
    let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
    let l = &cell.lists[li];
    let nodes = sab_formats::editnodes::parse_entry(&sub[l.loc.off..l.loc.off + l.loc.comp]).unwrap();
    assert_eq!(nodes.len(), before.len());
    let get = |t: u32| nodes.iter().find(|n| n.tag() == t);
    let rx = |v: [f32; 3]| [v[0], -v[2], v[1]];
    let a = axes(get(t_rot).unwrap());
    for (k, v) in ax_rot.iter().enumerate() {
        let w = rx(*v);
        assert!((0..3).all(|j| (a[k][j] - w[j]).abs() < 1e-5), "panel axis {k}: {:?} vs {w:?}", a[k]);
    }
    assert_eq!(get(t_move).unwrap().position().unwrap(), [pos_move[0], pos_move[1] + 3.0, pos_move[2]]);
    assert!(get(t_del).is_none());
    assert_eq!(get(t_dup).unwrap(), &before[idx0(t_dup)]);
    let cp = get(new_tag).expect("copy exists");
    assert_eq!(cp.class_name(), before[idx0(t_dup)].class_name());
    assert_eq!(cp.position().unwrap(), [pos_dup[0], pos_dup[1] + 3.0, pos_dup[2]]);
    let touched = [t_rot, t_move, t_del, t_dup, new_tag];
    for b in before.iter().filter(|b| !touched.contains(&b.tag())) {
        assert_eq!(get(b.tag()).unwrap(), b, "untouched object {:08x} unchanged", b.tag());
    }
    eprintln!("panel(rot) {:?}; panel(move) {pos_move:?}; panel(del) {:?}; debris {pos_dup:?}", before[idx0(t_rot)].position().unwrap(), before[idx0(t_del)].position().unwrap());
}

/// Collision question: delete the five hall panels around the western walking path (9ff4daa4, 22895093, a7949f0d, e577c516, 4dbe000c) and keep 1c9623d1 (the internal control for the south-east walk).
/// Kept in output/m0/cell_multi_walk_1700865 for the live walk test; the export is decoded and every other object must be byte-identical.
#[test]
fn deleting_the_western_hall_panels_exports_exactly_those_deletions() {
    use sab_formats::editnodes::Node;
    let dels = [0x9ff4_daa4_u32, 0x2289_5093, 0xa794_9f0d, 0xe577_c516, 0x4dbe_000c];
    let keep = 0x1c96_23d1_u32;
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, _) = edit.find(dels[0]).unwrap();
    for t in dels.iter().chain([&keep]) {
        assert_eq!(edit.find(*t).map(|x| x.0), Some(li), "{t:08x} is in the same placement list");
    }
    let c = edit.containers[li];
    let before: Vec<Node> = s.entry(c).unwrap().objects().to_vec();
    for t in dels {
        let i = s.entry(c).unwrap().objects().iter().position(|o| o.tag() == t).unwrap();
        s.remove_objects(&[(c, i)]).unwrap();
    }
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("cell_multi_walk_1700865");
    let _ = std::fs::remove_dir_all(&out);
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass && o.report.contains("REBUILD"), "{}", o.report);
    let mp = std::fs::read(out.join("patchmega1.megapack")).unwrap();
    let pk = sab_formats::megapack::parse(&mp).unwrap();
    let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
    let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
    let l = &cell.lists[li];
    let nodes = sab_formats::editnodes::parse_entry(&sub[l.loc.off..l.loc.off + l.loc.comp]).unwrap();
    assert_eq!(nodes.len(), before.len() - 5);
    for t in dels {
        assert!(!nodes.iter().any(|n| n.tag() == t), "{t:08x} gone");
    }
    assert!(nodes.iter().any(|n| n.tag() == keep), "the control panel stays");
    let want: Vec<&Node> = before.iter().filter(|n| !dels.contains(&n.tag())).collect();
    assert!(nodes.iter().zip(want).all(|(a, b)| a == b), "every other object byte-identical, in order");
}
