//! The agent interface on the retail install named by `.sab-local.toml`: search, resolve (no change),
//! apply (atomic, idempotent by patch_id, revision-checked) and stage. The staged override must equal
//! the M0 E1 artifact the game was observed to load (`docs/level_editor/m0_log.md` §5d).
//!
//! Game-gated: built by the `retail` feature, reads only the repo-root `.sab-local.toml`.

use std::path::Path;

use sab_formats::game_paths::local_config_retail_dir;
use sab_tools::agent;
use sab_tools::session::Session;
use serde_json::json;

const E1_ARTIFACT: &str = "6E406467AE7A57A00053E8D28440CEE1541199E586289F5507F928BE9095A4F0";

#[test]
fn an_agent_patch_on_the_blip_locator_stages_the_in_game_e1_artifact() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    let mut s = Session::open(&game).unwrap();
    let hits = agent::search(&s, "LOC_LV_Blip_Ext", 5);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["container"], "6d21e53e");
    assert_eq!(hits[0]["tag"], "ff3106f1");

    let patch = json!({ "patch_id": "blip-east-3m", "expected_revision": 0,
                        "operations": [{ "action": "offset", "container": "6d21e53e", "tag": "ff3106f1", "delta": [3, 0, 0] }] });
    agent::resolve_patch(&mut s, &patch).unwrap();
    assert_eq!((s.revision(), s.active().len()), (0, 0), "resolve changes nothing");
    let r = agent::apply_patch(&mut s, &patch).unwrap();
    assert_eq!(r["revision_after"], 1);
    assert_eq!(agent::apply_patch(&mut s, &patch).unwrap()["replayed"], true);
    assert!(agent::apply_patch(&mut s, &json!({ "expected_revision": 0, "operations": [
        { "action": "offset", "container": "6d21e53e", "tag": "ff3106f1", "delta": [0, 1, 0] }] })).is_err(), "stale plan refused");

    let dir = std::env::temp_dir().join(format!("sab_agent_retail_{}", std::process::id()));
    let out = s.stage(&dir).unwrap();
    assert!(out.all_pass, "{}", out.report);
    assert_eq!(out.artifact_sha, E1_ARTIFACT);
    std::fs::remove_dir_all(&dir).ok();
}
