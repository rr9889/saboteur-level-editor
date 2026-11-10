//! `sab_headless` — thin JSON CLI over `sab_tools::headless::Editor`.
//!
//! ```text
//! sab_headless serve --game <install>          newline-delimited JSON requests on stdin/stdout
//! sab_headless run   --game <install> --script <file.json>
//! sab_headless exec  --game <install> --request '<json>'
//! ```
//!
//! One request/response per line:
//! `{"id":1,"cmd":"open_area","args":{"name":"Belle"}}` →
//! `{"id":1,"ok":true,"data":{...}}`. Errors are `{"id":1,"ok":false,"error":"..."}` and never abort
//! the server. The game folder is read-only here; staging writes only under `out`.
//!
//! Commands: status, areas, open_area, query, get, select, copy, cut, edit, plan, undo, redo, stage,
//! dump, assert, shutdown.

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

use sab_tools::headless::Editor;
use serde_json::{json, Value};

const HELP: &str = "sab_headless — headless Saboteur level-editing backend (JSON)

usage:
  sab_headless serve [--game <install>]
  sab_headless run   [--game <install>] --script <file.json> [--keep-going]
  sab_headless exec  [--game <install>] --request '<json>' | --request - [--stdin]
  sab_headless --help

Game folder: --game, else SAB_RETAIL_DIR / .sab-local.toml via sab_formats::game_paths.
Commands: status, areas, open_area, query, get, select, copy, cut, edit, plan, undo, redo,
          stage, dump, assert, shutdown.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return;
    }
    let r = run(&args);
    if let Err(e) = r {
        if !e.is_empty() {
            eprintln!("{e}");
        }
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mode = args.first().map(String::as_str).unwrap_or("");
    let game = flag(args, "--game");
    let game = match game {
        Some(g) => PathBuf::from(g),
        None => sab_formats::game_paths::retail_dir(&std::env::current_dir().map_err(|e| e.to_string())?).unwrap_or_else(|e| {
            eprintln!("no --game and no local game path: {e}");
            std::process::exit(2)
        }),
    };
    let mut editor = Editor::open(&game)?;
    match mode {
        "serve" => serve(&mut editor),
        "run" => {
            let path = flag(args, "--script").ok_or("run needs --script <file.json>")?;
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
            let script: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
            let list = script.get("commands").and_then(|c| c.as_array()).or_else(|| script.as_array()).ok_or("script must be an array or {\"commands\": [...]}")?;
            let keep_going = args.iter().any(|a| a == "--keep-going");
            let mut failed = 0;
            for (i, req) in list.iter().enumerate() {
                let resp = dispatch(&mut editor, req, i as u64 + 1);
                println!("{}", serde_json::to_string(&resp).unwrap_or_else(|_| "{\"ok\":false}".into()));
                if resp["ok"] != true {
                    failed += 1;
                    if !keep_going {
                        break;
                    }
                }
            }
            if failed > 0 {
                return Err(format!("{failed} command(s) failed"));
            }
            Ok(())
        }
        "exec" => {
            let text = match flag(args, "--request") {
                Some(r) if r == "-" => {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
                    s
                }
                Some(r) => r,
                None => {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
                    s
                }
            };
            let req: Value = serde_json::from_str(text.trim()).map_err(|e| format!("request is not JSON: {e}"))?;
            let resp = dispatch(&mut editor, &req, req["id"].as_u64().unwrap_or(1));
            println!("{}", serde_json::to_string(&resp).unwrap_or_else(|_| "{\"ok\":false}".into()));
            if resp["ok"] != true {
                std::process::exit(1);
            }
            Ok(())
        }
        other => Err(format!("unknown mode {other:?}\n{HELP}")),
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn serve(editor: &mut Editor) -> Result<(), String> {
    eprintln!("sab_headless: {} (json lines on stdin)", editor.game().display());
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                println!("{}", json!({ "id": Value::Null, "ok": false, "error": format!("request is not JSON: {e}") }));
                let _ = out.flush();
                continue;
            }
        };
        let id = req["id"].as_u64().unwrap_or(0);
        if req["cmd"].as_str() == Some("shutdown") {
            println!("{}", json!({ "id": id, "ok": true, "data": { "shutdown": true } }));
            break;
        }
        let resp = dispatch(editor, &req, id);
        println!("{}", serde_json::to_string(&resp).unwrap_or_else(|_| "{\"ok\":false}".into()));
        let _ = out.flush();
    }
    Ok(())
}

/// Execute one request against an open editor. Errors are returned inside the JSON envelope, never as a
/// process failure, so a server keeps running after a bad request.
fn dispatch(editor: &mut Editor, req: &Value, id: u64) -> Value {    let cmd = req["cmd"].as_str().unwrap_or("");
    let args = req.get("args").cloned().unwrap_or_else(|| json!({}));
    let r: Result<Value, String> = match cmd {
        "status" => Ok(editor.status()),
        "areas" => editor.areas().map(|a| json!({ "areas": a })),
        "open_area" => editor.open_area(&args),
        "query" => editor.query(&args),
        "get" => editor.get(&args),
        "select" => editor.select(&args),
        "copy" => editor.copy(&args),
        "cut" => editor.cut(&args),
        "edit" => editor.edit(&args, false).map_err(|e| e["error"].as_str().unwrap_or("edit failed").to_string()),
        "plan" => editor.edit(&args, true).map_err(|e| e["error"].as_str().unwrap_or("plan failed").to_string()),
        "undo" => editor.undo(),
        "redo" => editor.redo(),
        "stage" => editor.stage(&args),
        "dump" => editor.dump(&args),
        "assert" => editor.assert(&args),
        "shutdown" => Ok(json!({ "shutdown": true })),
        other => Err(format!("unknown cmd {other:?} (status, areas, open_area, query, get, select, copy, cut, edit, plan, undo, redo, stage, dump, assert)")),
    };
    match r {
        Ok(data) => json!({ "id": id, "ok": true, "data": data }),
        Err(error) => json!({ "id": id, "ok": false, "error": error, "revision": editor.session().revision() }),
    }
}

#[allow(dead_code)]
fn _path(p: &Path) -> String {
    p.display().to_string()
}
