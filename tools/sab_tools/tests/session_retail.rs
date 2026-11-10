//! End-to-end placement-editing workflow on the retail install named by `.sab-local.toml`:
//! find, inspect, move, undo, redo, save, reopen and stage. The staged override must be byte-identical
//! to the M0 E1 artifact, which the game was observed to load (`docs/level_editor/m0_log.md` §5d).
//!
//! Game-gated: built by the `retail` feature, reads only the repo-root `.sab-local.toml`.

use std::path::Path;

use sab_formats::editnodes::tags;
use sab_formats::game_paths::local_config_retail_dir;
use sab_tools::session::Session;

/// `PARIS\area01\lavillette\interior\lavillette_ext\ext_blip_loc` and its `LOC_LV_Blip_Ext` locator.
const CONTAINER: u32 = 0x6d21_e53e;
const LOCATOR: u32 = 0xff31_06f1;
/// SHA-256 of the M0 E1 staged `EditNodes.pack` (the locator moved +3 m on X).
const E1_ARTIFACT: &str = "6E406467AE7A57A00053E8D28440CEE1541199E586289F5507F928BE9095A4F0";

#[test]
fn select_move_undo_save_reopen_and_stage_reproduce_the_in_game_e1_artifact() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mut s = Session::open(&game).unwrap();

    // find
    let c = s.containers().into_iter().find(|c| c.hash == CONTAINER).expect("container listed");
    assert_eq!(c.name, Some(r"France\EditNodes\PARIS\area01\lavillette\interior\lavillette_ext\ext_blip_loc.wsd"));
    assert_eq!(s.source_of(CONTAINER), Some("base (loosefiles)"));
    let idx = s.entry(CONTAINER).unwrap().objects().iter().position(|o| o.tag() == LOCATOR).expect("locator present");

    // inspect
    let o = &s.entry(CONTAINER).unwrap().objects()[idx];
    assert_eq!(o.type_name().as_deref(), Some("Point"));
    assert_eq!(o.class_name().as_deref(), Some("Locator"));
    let orig = o.position().unwrap();
    assert_eq!(o.params_with_tag(tags::POSITION).len(), 1);

    // move, undo, redo
    let moved = [orig[0] + 3.0, orig[1], orig[2]];
    s.move_object(CONTAINER, idx, moved).unwrap();
    assert_eq!(s.entry(CONTAINER).unwrap().objects()[idx].position(), Some(moved));
    s.undo().unwrap();
    assert_eq!(s.entry(CONTAINER).unwrap().objects()[idx].position(), Some(orig));
    s.redo().unwrap();
    assert_eq!(s.entry(CONTAINER).unwrap().objects()[idx].position(), Some(moved));

    // save and reopen
    let dir = std::env::temp_dir().join(format!("sab_session_retail_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let proj = dir.join("blip.sabproj.json");
    s.save(&proj).unwrap();
    let r = Session::load(&proj).unwrap();
    assert_eq!(r.journal().0, s.journal().0);
    assert_eq!(r.entry(CONTAINER).unwrap().objects()[idx].position(), Some(moved));

    // stage
    let out = r.stage(&dir.join("stage")).unwrap();
    assert!(out.all_pass, "{}", out.report);
    assert_eq!(out.artifact_sha, E1_ARTIFACT, "the editor's staged pack must equal the in-game-proven E1 pack");
    assert!(r.stage(&game.join("never_here")).is_err(), "staging inside the game folder is refused");
    std::fs::remove_dir_all(&dir).ok();
}

/// Go to Object on the retail install: the Gate A+ chair (tag 878ccdfa) is found by tag and by name, the hit carries the
/// position the game reported (-77.671, 263.2, -757.045, observed 2026-11-03), and searching changes nothing in the session.
#[test]
fn go_to_object_finds_the_gate_aplus_chair_by_tag_and_name_without_editing() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let s = Session::open(&game).unwrap();
    let rev = s.revision();
    let (hits, total) = sab_tools::areas::find_objects(&s, "878ccdfa", 10);
    assert!(total >= 1 && !hits.is_empty(), "the chair tag must be found");
    let chair = hits.iter().find(|h| h.tag == 0x878c_cdfa).expect("exact tag hit");
    let p = chair.pos.expect("the chair has a position");
    assert!((p[0] + 77.671).abs() < 0.01 && (p[1] - 263.2).abs() < 0.01 && (p[2] + 757.045).abs() < 0.01, "position {p:?}");
    let (by_ref, _) = sab_tools::areas::find_objects(&s, &format!("{:08x}#{}", chair.obj.0, chair.obj.1), 5);
    assert_eq!(by_ref.len(), 1, "container#index selects exactly that instance");
    assert_eq!(by_ref[0].obj, chair.obj);
    assert_eq!(s.revision(), rev, "navigation must not modify the session");
    let (none, n) = sab_tools::areas::find_objects(&s, "definitely_not_an_object_name_qq", 10);
    assert!(none.is_empty() && n == 0);
}
