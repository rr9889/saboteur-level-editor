//! Headless backend on the real install: open an area, query, edit atomically, undo, stage. No game
//! launch, no GUI, no GPU. Game-gated like the other `*_retail` tests.

use std::path::Path;

use sab_formats::game_paths::local_config_retail_dir;
use sab_tools::headless::{Editor, ObjRef};
use serde_json::json;

#[test]
fn headless_opens_belle_queries_edits_undoes_and_stages() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mut e = Editor::open(&game).unwrap();

    let area = e.open_area(&json!({ "name": "Belle" })).unwrap();
    let n = area["area"]["objects"].as_u64().unwrap();
    assert!(n > 100, "Belle has objects: {n}");

    let q = e.query(&json!({ "area": "Belle", "class_contains": "RoundTable", "limit": 10 })).unwrap();
    assert!(q["count"].as_u64().unwrap() >= 1, "{q}");
    let r = ObjRef::from_json(&q["objects"][0]["ref"]).unwrap();
    let before = e.get(&r.json()).unwrap()["position"].clone();
    e.select(&r.json()).unwrap();
    assert_eq!(e.status()["selection"].as_array().unwrap().len(), 1);

    // One atomic patch: a small move + a duplicate. Undo restores the original and removes the copy.
    let rev = e.session().revision();
    let patch = json!({ "patch_id": format!("headless-retail-{}", std::process::id()), "expected_revision": rev,
        "operations": [
            { "action": "offset", "refs": "selection", "delta": [0.25, 0.0, 0.25] },
            { "action": "duplicate", "refs": "selection", "count": 1, "offset": [0.0, 1.0, 0.0] }
        ] });
    let out = e.edit(&patch, false).unwrap();
    assert_eq!(out["applied"], true, "{out}");
    assert_eq!(e.session().revision(), rev + 1);
    let after = e.get(&r.json()).unwrap()["position"].clone();
    assert_ne!(before, after, "the move is visible");

    let dir = std::env::temp_dir().join(format!("sab_headless_stage_{}", std::process::id()));
    let staged = e.stage(&json!({ "out": dir.display().to_string() })).unwrap();
    assert_eq!(staged["all_static_checks_pass"], true, "{staged}");
    assert!(dir.join("EditNodes.pack").is_file() && dir.join("expect.json").is_file());
    assert!(staged["report"].as_str().unwrap().contains("ALL STATIC CHECKS PASS: true"));

    assert!(e.undo().unwrap()["undone"].as_bool().unwrap());
    let back = e.get(&r.json()).unwrap()["position"].clone();
    assert_eq!(before, back, "undo restores the position");
    std::fs::remove_dir_all(&dir).ok();
}
