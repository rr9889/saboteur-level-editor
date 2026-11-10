//! Terrain-cell project persistence ACROSS PROCESSES (game-gated, `retail` feature, needs `.sab-local.toml`).
//! Run the two tests as two separate `cargo test` invocations, in order (they are `#[ignore]`d so a normal run never executes them):
//!   cargo test --test cell_project_process step_a_author_and_save -- --ignored      (process A: author four operations, save, record the export hash)
//!   cargo test --test cell_project_process step_b_reopen_extend_and_export -- --ignored   (process B: a fresh process reopens the file)
//! B checks: the same journal; the byte-identical export hash recorded by A; a further edit is possible, is exported together with A's edits, and undoing it
//! returns to A's exact export hash. This is the "close Workshop, reopen the project, keep editing" criterion at the library level (the GUI calls the same functions).
use std::path::{Path, PathBuf};

use sab_formats::editnodes::{tags, Node};
use sab_formats::game_paths::local_config_retail_dir;
use sab_tools::cellsession::{open_cell, open_cell_project, save_cell_project, stage_cell};

const CELL: u32 = 1_700_865;
// hall objects (cell 1700865): slab move, slab rotate, slab delete, slab duplicate
const T_MOVE: u32 = 0x80a9_bb53;
const T_ROT: u32 = 0x9c8b_4dc2;
const T_DEL: u32 = 0x9879_2863;
const T_DUP: u32 = 0xe97d_f01e;
// the further edit made in process B: a panel moved 1 m on X
const T_EXTRA: u32 = 0x1c96_23d1;

fn game() -> PathBuf {
    local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"))
}

fn dir() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("output").join("m0").join("cell_project_persist");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn find(s: &sab_tools::session::Session, c: u32, tag: u32) -> usize {
    s.entry(c).unwrap().objects().iter().position(|o| o.tag() == tag).expect("tag present")
}

#[test]
#[ignore = "process A of the cross-process persistence check; see the file header"]
fn step_a_author_and_save() {
    let game = game();
    let (edit, mut s) = open_cell(&game, CELL).unwrap();
    let (li, _) = edit.find(T_MOVE).unwrap();
    let c = edit.containers[li];
    let p0 = s.entry(c).unwrap().objects()[find(&s, c, T_MOVE)].position().unwrap();
    s.move_object(c, find(&s, c, T_MOVE), [p0[0], p0[1] + 4.0, p0[2]]).unwrap();
    s.rotate_objects(&[(c, find(&s, c, T_ROT))], [[1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]], None).unwrap();
    s.remove_objects(&[(c, find(&s, c, T_DEL))]).unwrap();
    let (new_tag, _) = s.mint_tag(c, T_DUP);
    let di = find(&s, c, T_DUP);
    let Node::Container { children, .. } = &s.entry(c).unwrap().objects()[di].clone() else { panic!() };
    s.insert_objects(vec![(c, di + 1, Node::Container { tag: new_tag, children: children.clone() })], "Duplicate").unwrap();
    let d = dir();
    let proj = d.join("cell.sabproject.json");
    save_cell_project(&edit, &s, &proj).unwrap();
    let out = d.join("export_a");
    let _ = std::fs::remove_dir_all(&out);
    let o = stage_cell(&game, &edit, &s, &out).unwrap();
    assert!(o.all_pass, "{}", o.report);
    std::fs::write(
        d.join("a_result.json"),
        serde_json::json!({ "pid": std::process::id(), "export_sha256": o.artifact_sha, "journal_len": s.to_project_json()["edits"].as_array().unwrap().len(), "new_tag": format!("{new_tag:08x}") }).to_string(),
    )
    .unwrap();
    eprintln!("process A (pid {}): saved {} ; export {}", std::process::id(), proj.display(), o.artifact_sha);
}

#[test]
#[ignore = "process B of the cross-process persistence check; run after step A in a NEW process"]
fn step_b_reopen_extend_and_export() {
    let game = game();
    let d = dir();
    let a: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join("a_result.json")).expect("run step A first")).unwrap();
    assert_ne!(a["pid"].as_u64().unwrap() as u32, std::process::id(), "step B must run in a different process than step A");
    let proj = d.join("cell.sabproject.json");
    let (edit, mut s) = open_cell_project(&proj).unwrap();
    assert_eq!(s.to_project_json()["edits"].as_array().unwrap().len() as u64, a["journal_len"].as_u64().unwrap(), "same journal length as saved");
    // 1. the reopened project exports exactly what process A exported
    let o1 = stage_cell(&game, &edit, &s, &d.join("export_b1")).unwrap();
    assert!(o1.all_pass, "{}", o1.report);
    assert_eq!(o1.artifact_sha, a["export_sha256"].as_str().unwrap(), "reopened in a new process: byte-identical export");
    // 2. a further edit is possible and is exported together with the earlier ones
    let li = edit.find(T_EXTRA).expect("panel in the cell").0;
    let c = edit.containers[li];
    let pe = s.entry(c).unwrap().objects()[find(&s, c, T_EXTRA)].position().unwrap();
    s.move_object(c, find(&s, c, T_EXTRA), [pe[0] + 1.0, pe[1], pe[2]]).unwrap();
    let o2 = stage_cell(&game, &edit, &s, &d.join("export_b2")).unwrap();
    assert!(o2.all_pass, "{}", o2.report);
    assert_ne!(o2.artifact_sha, o1.artifact_sha, "the extra edit changes the export");
    let mp = std::fs::read(d.join("export_b2").join("patchmega1.megapack")).unwrap();
    let pk = sab_formats::megapack::parse(&mp).unwrap();
    let sub = sab_formats::megapack::entry_slice(&mp, &pk.entries[0]).unwrap();
    let cell = sab_formats::cellplace::read(sub).unwrap().unwrap();
    let l = &cell.lists[li];
    let nodes = sab_formats::editnodes::parse_entry(&sub[l.loc.off..l.loc.off + l.loc.comp]).unwrap();
    let get = |t: u32| nodes.iter().find(|n| n.tag() == t);
    assert_eq!(get(T_EXTRA).unwrap().position().unwrap(), [pe[0] + 1.0, pe[1], pe[2]], "the new edit is in the export");
    assert!(get(T_DEL).is_none(), "A's deletion is still in the export");
    let new_tag = u32::from_str_radix(a["new_tag"].as_str().unwrap(), 16).unwrap();
    assert!(get(new_tag).is_some(), "A's duplicate is still in the export");
    assert!(get(T_MOVE).unwrap().position().unwrap()[1] > 58.0, "A's +4 m move is still in the export");
    let ax = |n: &Node| n.vec3_param(tags::Z_AXIS).unwrap();
    assert!(get(T_ROT).is_some() && ax(get(T_ROT).unwrap()) != [0.0, 0.0, 0.0], "A's rotation is present");
    // 3. undoing the new edit returns to process A's exact export
    assert!(s.undo().unwrap());
    let o3 = stage_cell(&game, &edit, &s, &d.join("export_b3")).unwrap();
    assert_eq!(o3.artifact_sha, a["export_sha256"].as_str().unwrap(), "undo of the extra edit returns to A's byte-identical export");
    // 4. and the extra edit can be redone
    assert!(s.redo().unwrap());
    assert_eq!(stage_cell(&game, &edit, &s, &d.join("export_b4")).unwrap().artifact_sha, o2.artifact_sha, "redo reproduces the extended export");
    eprintln!("process B (pid {}): reopened, same export, extended, undone, redone: all consistent", std::process::id());
}
