//! `sab_tools places`: the placement editor's agent interface on the command line. Every command
//! prints one JSON document to stdout; errors print `{"ok": false, ...}` and exit 1. The project file
//! (`sab_placement_project`) is the state between calls, so the same session can be driven from a
//! shell, a script or an AI agent, and then opened in the workshop's places page.
//!
//! ```text
//! sab_tools places new     <project.json>                 empty project on the configured install
//! sab_tools places status  <project.json>
//! sab_tools places search  <project.json> <text> [limit]  objects by path, name, Type or ClassName
//! sab_tools places inspect <project.json> <container> <tag | #index>
//! sab_tools places plan    <project.json> <patch>         validate a patch; changes nothing
//! sab_tools places apply   <project.json> <patch>         apply atomically and save the project
//! sab_tools places undo | redo <project.json>
//! sab_tools places stage   <project.json> <out_dir>       DLC/01 EditNodes override (never in the game folder)
//! ```
//! `<patch>` is a JSON file path, `-` for stdin, or inline JSON starting with `{`
//! (format: `sab_tools::agent`).

use std::io::Read as _;
use std::path::Path;

use sab_tools::agent::{self, parse_hex};
use sab_tools::session::Session;
use serde_json::{json, Value};

pub fn run(args: &[String]) -> Result<(), String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = match a.as_slice() {
        ["new", proj] => {
            let game = sab_formats::game_paths::retail_dir(&std::env::current_dir().map_err(|e| e.to_string())?)?;
            let s = Session::open(&game)?;
            s.save(Path::new(proj))?;
            json!({ "ok": true, "created": proj, "status": agent::status(&s) })
        }
        ["status", proj] => agent::status(&Session::load(Path::new(proj))?),
        ["search", proj, q] => json!({ "ok": true, "results": agent::search(&Session::load(Path::new(proj))?, q, 50) }),
        ["search", proj, q, n] => {
            let n = n.parse::<usize>().map_err(|e| format!("limit {n}: {e}"))?;
            json!({ "ok": true, "results": agent::search(&Session::load(Path::new(proj))?, q, n) })
        }
        ["inspect", proj, c, t] => {
            let s = Session::load(Path::new(proj))?;
            let c = parse_hex(c)?;
            let i = match t.strip_prefix('#') {
                Some(i) => i.parse::<usize>().map_err(|e| format!("index {i}: {e}"))?,
                None => agent::resolve_object(&s, c, parse_hex(t)?, None)?,
            };
            agent::inspect(&s, c, i)?
        }
        ["plan", proj, p] => {
            let mut s = Session::load(Path::new(proj))?;
            agent::resolve_patch(&mut s, &read_patch(p)?).unwrap_or_else(|e| e)
        }
        ["apply", proj, p] => {
            let mut s = Session::load(Path::new(proj))?;
            match agent::apply_patch(&mut s, &read_patch(p)?) {
                Ok(r) => {
                    s.save(Path::new(proj))?;
                    r
                }
                Err(e) => e,
            }
        }
        ["undo", proj] | ["redo", proj] => {
            let mut s = Session::load(Path::new(proj))?;
            let changed = if a[0] == "undo" { s.undo()? } else { s.redo()? };
            s.save(Path::new(proj))?;
            json!({ "ok": true, "changed": changed, "status": agent::status(&s) })
        }
        ["stage", proj, dir] => {
            let s = Session::load(Path::new(proj))?;
            let o = s.stage(Path::new(dir))?;
            json!({ "ok": o.all_pass, "out_dir": o.out_dir.display().to_string(), "artifact_sha256": o.artifact_sha,
                    "static_checks_pass": o.all_pass, "report": o.report,
                    "install": "close the game, then run install.ps1 in out_dir; restore.ps1 undoes it" })
        }
        _ => return Err(USAGE.to_string()),
    };
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    if out["ok"] == false {
        return Err(String::new());
    }
    Ok(())
}

fn read_patch(p: &str) -> Result<Value, String> {
    let text = if p.trim_start().starts_with('{') {
        p.to_string()
    } else if p == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
        s
    } else {
        std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?
    };
    serde_json::from_str(&text).map_err(|e| format!("patch is not JSON: {e}"))
}

pub const USAGE: &str = "  sab_tools places new|status <project.json>
  sab_tools places search  <project.json> <text> [limit]
  sab_tools places inspect <project.json> <container> <tag | #index>
  sab_tools places plan|apply <project.json> <patch.json | - | {inline json}>
  sab_tools places undo|redo <project.json>
  sab_tools places stage   <project.json> <out_dir>";
