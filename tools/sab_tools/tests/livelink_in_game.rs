//! B1 live readback on a running game. Ignored by default: it needs The Saboteur running with a Belle
//! save, the dev sab_asi.asi, and a named EditNodes object loaded. Run with:
//!
//! ```text
//! cargo test --release --target-dir ..\..\target_demo --test livelink_in_game -- --ignored --nocapture
//! ```
//!
//! It writes one `sab_cmd.lua` request through the production `LiveLink` client, tails the devlog,
//! compares the live position with the editor's stored position, and restores the bridge file.

use std::path::Path;
use std::time::{Duration, Instant};

use sab_formats::game_paths::local_config_retail_dir;
use sab_tools::livelink::{LiveLink, QueryObject, Status};
use sab_tools::session::Session;

fn lua_node_name(container: u32) -> Option<String> {
    let n = sab_formats::editnodes::tag_name(container)?;
    let n = n.strip_prefix(r"France\EditNodes\").unwrap_or(n);
    let n = n.strip_suffix(".wsd").unwrap_or(n);
    (!n.is_empty()).then(|| n.to_string())
}

#[test]
#[ignore = "needs The Saboteur running with a Belle save and the dev sab_asi.asi"]
fn live_readback_returns_the_game_position_for_the_belle_round_table() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
    println!("B1 game {}", game.display());
    let s = Session::open(&game).unwrap();
    let container = s
        .containers()
        .into_iter()
        .find(|c| c.name.map_or(false, |n| n.ends_with(r"belledenuit\interior\civs.wsd")))
        .unwrap_or_else(|| panic!("no Belle civs container in {}", game.display()));
    let entry = s.entry(container.hash).unwrap();
    let (index, name, pos) = entry
        .objects()
        .iter()
        .enumerate()
        .find_map(|(i, o)| {
            let name = s.object_name(o.tag())?;
            (o.class_name().is_some() && o.position().is_some()).then_some((i, name, o.position()?))
        })
        .unwrap_or_else(|| panic!("no named positioned object in container {:08x}", container.hash));
    let node = lua_node_name(container.hash).expect("container node name");
    println!("B1 object {name} (#{index}, container {:08x}) editor position {pos:?}", container.hash);
    println!("B1 node {node}");

    let cmd = game.join("sab_cmd.lua");
    let log = game.join("sab_devlog.txt");
    let log_from = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    println!("B1 bridge {}", cmd.display());
    let before = std::fs::read(&cmd).ok();
    let mut live = LiveLink::new();
    let id = live.ask(&game, s.revision(), Some(node), vec![QueryObject { path: name.clone(), editor_pos: pos }]).expect("write request");
    println!("B1 request id {id} written={} bytes={:?}", cmd.exists(), std::fs::metadata(&cmd).map(|m| m.len()).ok());

    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if live.poll() {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let status = live.status().clone();
    live.restore();
    let after = std::fs::read(&cmd).ok();
    assert_eq!(before, after, "sab_cmd.lua restored exactly");
    // What the game actually appended, for diagnosing a body/parser mismatch.
    let appended = {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&log).unwrap();
        let _ = f.seek(SeekFrom::Start(log_from));
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        s
    };
    println!("B1 appended log lines:\n{}", appended.lines().filter(|l| l.contains("SABREAD") || l.contains("sab_cmd") || l.contains("console")).collect::<Vec<_>>().join("\n"));

    match status {
        Status::Ready(r) => {
            println!("B1 reply node_count={:?} ended={} fatals={:?} readings={:?}", r.node_count, r.ended, r.fatals, r.readings);
            assert!(r.fatals.is_empty(), "chunk FATAL: {:?}", r.fatals);
            let got = r.readings.first().and_then(|x| x.as_ref()).unwrap_or_else(|| panic!("no reading for the table: {r:?}"));
            let got = got.as_ref().unwrap_or_else(|e| panic!("game refused the object: {e}"));
            let d = ((got[0] - pos[0]).powi(2) + (got[1] - pos[1]).powi(2) + (got[2] - pos[2]).powi(2)).sqrt();
            println!("B1 game position {got:?} delta {d:.6} m");
            assert!(d < 0.01, "game vs editor delta {d} m (a staged override may be installed)");
        }
        other => panic!("no live reply: {other:?}. Is the game in gameplay and sab_asi attached?"),
    }
}
