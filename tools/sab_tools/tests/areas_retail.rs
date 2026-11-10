//! The area browser's data on the retail install named by `.sab-local.toml`: the InteriorList paths
//! resolve, Belle de Nuit and La Villette land on their room map objects, and building the list
//! never touches the editing session. Game-gated like `session_retail.rs`.

use std::path::Path;

use sab_formats::game_paths::local_config_retail_dir;
use sab_tools::areas::{self, Kind};
use sab_tools::session::Session;

#[test]
fn interiors_resolve_and_belle_and_la_villette_find_their_rooms() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let s = Session::open(&game).unwrap();
    let (rev, journal) = (s.revision(), s.journal().0.len());
    let started = std::time::Instant::now();
    let list = areas::build(&game, &s).unwrap();
    println!("area list built in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
    assert_eq!((s.revision(), s.journal().0.len()), (rev, journal), "building the list must not edit anything");

    for a in &list {
        println!("{:9} {:28} {}", a.kind.label(), a.name, a.availability());
    }
    let unresolved: Vec<_> = list.iter().filter(|a| a.kind == Kind::Interior && a.anchor.is_none()).map(|a| (&a.name, &a.anchor_note)).collect();
    println!("interiors without an anchor: {unresolved:?}");
    // Every interior either opens or says exactly why: no silent empty entry.
    for a in list.iter().filter(|a| a.kind == Kind::Interior) {
        assert!(a.anchor.is_some() || !a.anchor_note.trim().is_empty(), "{} has no anchor and no reason", a.name);
    }
    // Exterior bookmarks are only listed when their locator resolved, so all of them are openable.
    for a in list.iter().filter(|a| a.kind == Kind::Exterior) {
        assert!(a.anchor.is_some(), "{} has no anchor: {}", a.name, a.anchor_note);
    }

    // Selection identity: an area object's index must still address its tag, and the tag survives the
    // list rebuild that an edit triggers.
    for a in &list {
        for o in &a.objects {
            let live = s.entry(o.obj.0).and_then(|e| e.objects().get(o.obj.1)).expect("area object resolves");
            assert_eq!(live.tag(), o.tag, "{} #{} identity", a.name, o.obj.1);
        }
    }
    assert!(list.iter().any(|a| a.kind == Kind::Interior && a.objects.iter().any(|o| o.tag != 0)), "objects carry their stable tag");

    let get = |n: &str| list.iter().find(|a| a.name == n).unwrap_or_else(|| panic!("{n} missing"));
    let belle = get("Belle");
    assert!(belle.anchor.is_some(), "{}", belle.anchor_note);
    assert_eq!(belle.primary.as_ref().map(|m| m.name.to_lowercase()), Some("hq_int".into()), "{}", belle.primary_note);
    assert!(!belle.objects.is_empty());
    let lv = get("LaVillette");
    assert_eq!(lv.primary.as_ref().map(|m| m.name.to_lowercase()), Some("lavillette_int".into()), "{}", lv.primary_note);
    assert!(list.iter().any(|a| a.kind == Kind::Exterior), "at least one exterior bookmark");
}
