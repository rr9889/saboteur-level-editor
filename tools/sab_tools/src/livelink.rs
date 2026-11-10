//! B1 live readback over the existing `sab_asi` file bridge.
//!
//! The running game polls `sab_cmd.lua` (next to `Saboteur.exe`) every 100 ms and runs it; `print`
//! inside that chunk appends to `sab_devlog.txt`. This module writes one request chunk, remembers the
//! devlog offset, tails the appended lines and parses the editor's own `SABREAD` records. It never
//! launches, injects into or pauses the game, and it backs up/restores `sab_cmd.lua` so the editor
//! cannot silently destroy a file another tool owns.
//!
//! Records are one line each: `SABREAD <id> <KIND> ...`. The request id is echoed in every line, so a
//! stale reply from an earlier request can never be mistaken for this one. The editor additionally
//! compares the session revision at request time with the revision when the reply arrives.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The bridge command file the game polls.
pub const CMD_FILE: &str = "sab_cmd.lua";
/// The log the ASI appends every `print` to.
pub const DEVLOG_FILE: &str = "sab_devlog.txt";
const MARKER: &str = "SABREAD";
/// How long to wait for a reply before declaring the game silent. The ASI drains `sab_cmd.lua` at the
/// next engine `lua_pcall`; in an idle interior that can take longer than a second, so this is generous
/// (the UI shows "waiting" rather than claiming failure early).
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// One selected object the readback asks about: its full Lua path (when recovered) and the editor's
/// stored position at request time.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryObject {
    pub path: String,
    pub editor_pos: [f32; 3],
}

/// One readback request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: u64,
    /// The session revision when the request was written, for stale-reply detection.
    pub revision: u64,
    /// The container's Lua node name (without `.wsd`), when recovered.
    pub node: Option<String>,
    pub objects: Vec<QueryObject>,
}

/// What the game reported for one request.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub id: u64,
    /// `Util.GetEditNodeContents(node)` count, or -1 if the call returned nothing.
    pub node_count: Option<i64>,
    /// Why the node lookup failed, when it did.
    pub node_error: Option<String>,
    /// One entry per request object, in request order: no line, a position, or an explicit error.
    pub readings: Vec<Option<Result<[f32; 3], String>>>,
    /// Errors from the chunk wrapper (`pcall` around the whole body).
    pub fatals: Vec<String>,
    pub ended: bool,
}

impl Response {
    fn new(id: u64, objects: usize) -> Response {
        Response { id, node_count: None, node_error: None, readings: vec![None; objects], fatals: Vec::new(), ended: false }
    }
}

/// The state shown in the editor's Live Readback panel.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Waiting,
    Ready(Response),
    TimedOut,
    Error(String),
}

/// The live readback client. One pending request at a time; `restore` (and `Drop`) put `sab_cmd.lua`
/// back exactly as it was before the first request.
pub struct LiveLink {
    game: Option<PathBuf>,
    request: Option<Request>,
    status: Status,
    offset: u64,
    buffer: String,
    started: Option<Instant>,
    /// `Some(original bytes)` when a file existed, `Some(None)` when it did not; `None` when untouched.
    backup: Option<Option<Vec<u8>>>,
    response: Option<Response>,
    last_id: u64,
}

impl Default for LiveLink {
    fn default() -> Self {
        LiveLink::new()
    }
}

impl LiveLink {
    pub fn new() -> LiveLink {
        LiveLink { game: None, request: None, status: Status::Idle, offset: 0, buffer: String::new(), started: None, backup: None, response: None, last_id: 0 }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn request(&self) -> Option<&Request> {
        self.request.as_ref()
    }

    /// Whether `sab_cmd.lua` is currently replaced by us (so a restore is meaningful).
    pub fn has_backup(&self) -> bool {
        self.backup.is_some()
    }

    /// Write one request chunk, remember the devlog offset and start waiting. The first call for a game
    /// folder backs up `sab_cmd.lua`; a later call preserves that original backup.
    pub fn ask(&mut self, game: &Path, revision: u64, node: Option<String>, objects: Vec<QueryObject>) -> Result<u64, String> {
        if objects.is_empty() {
            return Err("no selected object has a recovered Lua path; the readback can only address objects whose names are known".into());
        }
        if self.game.as_deref() != Some(game) {
            self.restore();
            self.game = Some(game.to_path_buf());
        }
        let cmd = game.join(CMD_FILE);
        let log = game.join(DEVLOG_FILE);
        if self.backup.is_none() {
            self.backup = Some(fs::read(&cmd).ok());
        }
        let id = self.next_id();
        let chunk = build_chunk(id, node.as_deref(), &objects);
        // Record the log offset BEFORE writing the chunk: the game may run it within one poll interval,
        // and a reply written between our write and our offset read would otherwise be missed.
        let start = fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
        fs::write(&cmd, chunk).map_err(|e| format!("could not write {}: {e}", cmd.display()))?;
        self.offset = start;
        self.buffer.clear();
        self.response = Some(Response::new(id, objects.len()));
        self.request = Some(Request { id, revision, node, objects });
        self.status = Status::Waiting;
        self.started = Some(Instant::now());
        Ok(id)
    }

    /// Read whatever the game appended since the last poll and parse our records. Returns true when the
    /// visible status changed.
    pub fn poll(&mut self) -> bool {
        if !matches!(self.status, Status::Waiting) {
            return false;
        }
        let (Some(game), Some(req)) = (self.game.clone(), self.request.as_ref()) else { return false };
        let req_id = req.id;
        let log = game.join(DEVLOG_FILE);
        let mut f = match fs::File::open(&log) {
            Ok(f) => f,
            Err(e) => {
                self.status = Status::Error(format!("could not read {}: {e}. Was the game started with sab_asi.asi?", log.display()));
                return true;
            }
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            self.status = Status::Error(format!("{} was truncated while waiting; request again", log.display()));
            return true;
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            self.status = Status::Error("could not seek the devlog".into());
            return true;
        }
        let mut bytes = Vec::new();
        if f.read_to_end(&mut bytes).is_err() {
            self.status = Status::Error("could not read the devlog tail".into());
            return true;
        }
        self.offset += bytes.len() as u64;
        self.buffer.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim_end_matches('\r').to_string();
            self.buffer.drain(..=pos);
            if let Some(resp) = self.response.as_mut() {
                apply_line(resp, req_id, &line);
            }
        }
        if self.response.as_ref().map_or(false, |r| r.ended) {
            self.status = Status::Ready(self.response.clone().unwrap_or_else(|| Response::new(req_id, 0)));
            return true;
        }
        if self.started.map_or(false, |t| t.elapsed() > TIMEOUT) {
            self.status = Status::TimedOut;
            return true;
        }
        false
    }

    /// Put `sab_cmd.lua` back and clear the request. Called on game switch, app close and the explicit
    /// Restore button.
    pub fn restore(&mut self) {
        if let Some(game) = self.game.clone() {
            match self.backup.take() {
                Some(Some(bytes)) => {
                    let _ = fs::write(game.join(CMD_FILE), bytes);
                }
                Some(None) => {
                    let _ = fs::remove_file(game.join(CMD_FILE));
                }
                None => {}
            }
        }
        self.request = None;
        self.response = None;
        self.buffer.clear();
        self.status = Status::Idle;
        self.started = None;
    }

    fn next_id(&mut self) -> u64 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let id = if now <= self.last_id { self.last_id + 1 } else { now };
        self.last_id = id;
        id
    }
}

impl Drop for LiveLink {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Apply one devlog line to a response when it is our request. Every other line in the log (the game,
/// other mods, other consoles) is ignored.
fn apply_line(resp: &mut Response, req_id: u64, line: &str) {
    let toks: Vec<&str> = line.split_whitespace().collect();
    // The ASI prefixes console output with `[lua] `, so find the marker rather than requiring column 0.
    let Some(base) = toks.iter().position(|t| *t == MARKER) else { return };
    if toks.len() < base + 3 {
        return;
    }
    let Ok(id) = toks[base + 1].parse::<u64>() else { return };
    if id != req_id || id != resp.id {
        return;
    }
    match toks[base + 2] {
        "BEGIN" => {}
        "NODE" => {
            if let Some(n) = toks.get(base + 3) {
                resp.node_count = n.parse().ok();
            }
        }
        "NODEERR" => resp.node_error = Some(toks[(base + 3).min(toks.len())..].join(" ")),
        "OBJ" => {
            let i = toks.get(base + 3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            if (1..=resp.readings.len()).contains(&i) {
                let mut p = [f32::NAN; 3];
                for (k, slot) in p.iter_mut().enumerate() {
                    *slot = toks.get(base + 4 + k).and_then(|s| s.parse().ok()).unwrap_or(f32::NAN);
                }
                resp.readings[i - 1] = Some(Ok(p));
            }
        }
        "NIL" => {
            let i = toks.get(base + 3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            if (1..=resp.readings.len()).contains(&i) {
                resp.readings[i - 1] = Some(Err("the game has no handle for this path (not loaded, or the name changed)".into()));
            }
        }
        "ERR" => {
            let i = toks.get(base + 3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
            if (1..=resp.readings.len()).contains(&i) {
                resp.readings[i - 1] = Some(Err(toks[(base + 4).min(toks.len())..].join(" ")));
            }
        }
        "FATAL" => resp.fatals.push(toks[(base + 3).min(toks.len())..].join(" ")),
        "END" => resp.ended = true,
        _ => {}
    }
}

/// The complete Lua chunk for one request. Paths are escaped as Lua strings; the whole body is wrapped
/// in `pcall` so a missing `Util`/`Object` binding is reported as one explicit `FATAL` line rather than
/// a bare console error.
fn build_chunk(id: u64, node: Option<&str>, objects: &[QueryObject]) -> String {
    let mut names = String::new();
    for (i, o) in objects.iter().enumerate() {
        names.push_str(&format!("\n  [{}] = {},", i + 1, lua_string(&o.path)));
    }
    let node = match node {
        Some(n) => lua_string(n),
        None => "nil".to_string(),
    };
    format!(
        "-- generated by sab_workshop live readback; id {id}\n\
local id = {id}\n\
local node = {node}\n\
local names = {{{names}\n}}\n\
local function report(...) print(\"{MARKER}\", id, ...) end\n\
local ok, err = pcall(function()\n\
  report(\"BEGIN\")\n\
  if node then\n\
    local okn, t = pcall(Util.GetEditNodeContents, node)\n\
    if okn then report(\"NODE\", (t and #t) or -1) else report(\"NODEERR\", tostring(t)) end\n\
  end\n\
  for i, name in ipairs(names) do\n\
    local okh, h = pcall(Util.GetHandleByName, name)\n\
    if not okh or not h then report(\"NIL\", i)\n\
    else\n\
      local okp, x, y, z = pcall(Object.GetPosition, h)\n\
      if okp and x then report(\"OBJ\", i, string.format(\"%.6f\", x), string.format(\"%.6f\", y), string.format(\"%.6f\", z))\n\
      else report(\"ERR\", i, tostring(x)) end\n\
    end\n\
  end\n\
end)\n\
if not ok then report(\"FATAL\", tostring(err)) end\n\
report(\"END\")\n"
    )
}

fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn the_chunk_marks_the_request_and_escapes_windows_paths() {
        let q = vec![
            QueryObject { path: r"PARIS\area01\belledenuit\interior\cinematic\Belle_Halo1(1)".into(), editor_pos: [1.0, 2.0, 3.0] },
            QueryObject { path: "a\"b".into(), editor_pos: [0.0; 3] },
        ];
        let c = build_chunk(42, Some(r"PARIS\area01\belledenuit\interior\cinematic"), &q);
        assert!(c.contains("local id = 42"));
        assert!(c.contains(r#"PARIS\\area01\\belledenuit\\interior\\cinematic"#), "{c}");
        assert!(c.contains(r#"Belle_Halo1(1)"#));
        assert!(c.contains(r#"a\"b"#));
        assert!(c.contains("Util.GetHandleByName"));
        assert!(c.contains("Object.GetPosition"));
        assert!(c.contains("SABREAD"));
    }

    #[test]
    fn only_lines_for_this_request_id_are_parsed() {
        let mut r = Response::new(7, 2);
        apply_line(&mut r, 7, "noise line");
        apply_line(&mut r, 7, "SABREAD 6 BEGIN");
        apply_line(&mut r, 7, "SABREAD 6 OBJ 1 9.0 9.0 9.0");
        apply_line(&mut r, 7, "SABREAD 7 BEGIN");
        apply_line(&mut r, 7, "SABREAD 7 NODE 12");
        apply_line(&mut r, 7, "[lua] SABREAD 7 OBJ 1 1.500000 2.000000 3.000000");
        apply_line(&mut r, 7, "SABREAD 7 NIL 2");
        apply_line(&mut r, 7, "SABREAD 7 FATAL Util nil");
        apply_line(&mut r, 7, "SABREAD 7 END");
        assert_eq!(r.node_count, Some(12));
        assert_eq!(r.readings[0], Some(Ok([1.5, 2.0, 3.0])));
        assert!(r.readings[1].as_ref().unwrap().is_err());
        assert_eq!(r.fatals, vec!["Util nil"]);
        assert!(r.ended);
    }

    #[test]
    fn a_request_backs_up_the_bridge_file_and_restore_puts_it_back() {
        let dir = std::env::temp_dir().join(format!("sab_live_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cmd = dir.join(CMD_FILE);
        let log = dir.join(DEVLOG_FILE);
        std::fs::write(&cmd, b"-- another tool's chunk\n").unwrap();
        std::fs::write(&log, b"old line\n").unwrap();
        let mut live = LiveLink::new();
        let id = live.ask(&dir, 3, Some("node".into()), vec![QueryObject { path: r"a\b".into(), editor_pos: [0.0; 3] }]).unwrap();
        assert!(live.has_backup());
        let written = std::fs::read_to_string(&cmd).unwrap();
        assert!(written.contains("SABREAD") && written.contains(&id.to_string()));
        assert_eq!(live.poll(), false, "no reply yet");
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "SABREAD {id} NODE 4").unwrap();
        writeln!(f, "SABREAD {id} OBJ 1 4.000000 5.000000 6.000000").unwrap();
        writeln!(f, "SABREAD {id} END").unwrap();
        assert!(live.poll());
        match live.status() {
            Status::Ready(r) => {
                assert_eq!(r.node_count, Some(4));
                assert_eq!(r.readings[0], Some(Ok([4.0, 5.0, 6.0])));
            }
            other => panic!("expected a ready reply, got {other:?}"),
        }
        live.restore();
        assert_eq!(std::fs::read_to_string(&cmd).unwrap(), "-- another tool's chunk\n");
        assert!(!live.has_backup());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_bridge_file_is_removed_again_on_restore() {
        let dir = std::env::temp_dir().join(format!("sab_live_rm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut live = LiveLink::new();
        live.ask(&dir, 0, None, vec![QueryObject { path: "x".into(), editor_pos: [0.0; 3] }]).unwrap();
        assert!(dir.join(CMD_FILE).is_file());
        live.restore();
        assert!(!dir.join(CMD_FILE).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Dropping the live link restores the bridge file too: closing the app must not leave our chunk
    /// in the game folder.
    #[test]
    fn dropping_the_link_restores_the_bridge_file() {
        let dir = std::env::temp_dir().join(format!("sab_live_drop_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cmd = dir.join(CMD_FILE);
        std::fs::write(&cmd, b"original\n").unwrap();
        {
            let mut live = LiveLink::new();
            live.ask(&dir, 1, None, vec![QueryObject { path: "x".into(), editor_pos: [0.0; 3] }]).unwrap();
            assert_ne!(std::fs::read(&cmd).unwrap(), b"original\n");
        }
        assert_eq!(std::fs::read_to_string(&cmd).unwrap(), "original\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}
