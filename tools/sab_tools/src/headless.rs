//! Headless level-editing backend: a small, scriptable facade over the same `Session`, `areas`,
//! `agent` patch transaction and `editnodes_stage` staging the GUI uses. No egui, no GPU, no window,
//! no screenshots; every entry point takes and returns JSON-friendly values so the CLI (and an AI
//! agent) can drive it.
//!
//! The lifecycle is: `Editor::open` → `open_area`/`areas` → `query`/`get` → `edit` (one atomic,
//! revision-checked patch; `plan` validates without changing anything) → `undo`/`redo` → `stage` →
//! `assert`/`dump`. Object references are stable `(container, tag, optional index)` addresses; after an
//! edit, a reference without an index still resolves by tag.

use std::path::{Path, PathBuf};

use sab_formats::editnodes::{self, tags, LuaValue, Node};
use sab_formats::pandemic_hash;
use serde_json::{json, Value};

use crate::agent;
use crate::areas::{self, Area, Kind};
use crate::editnodes_stage::{node_json, unhex};
use crate::session::Session;

/// Largest absolute move accepted without `allow_far` (same guard as the agent protocol).
pub const MAX_MOVE: f32 = agent::MAX_MOVE;

/// A stable object address. `index` is optional: when absent, resolution requires a unique tag in the
/// container; when present it must still point at that tag (maps move indices).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjRef {
    pub container: u32,
    pub tag: u32,
    pub index: Option<usize>,
}

impl ObjRef {
    pub fn new(container: u32, tag: u32, index: Option<usize>) -> ObjRef {
        ObjRef { container, tag, index }
    }

    pub fn from_json(v: &Value) -> Result<ObjRef, String> {
        // Also accepts "90549f20:d9bc11a7[:17]" for shell convenience.
        if let Some(s) = v.as_str() {
            let mut parts = s.split(':');
            let c = hex(parts.next().ok_or("empty ref")?)?;
            let t = hex(parts.next().ok_or("ref needs <container>:<tag>")?)?;
            let i = match parts.next() {
                Some(i) => Some(i.parse::<usize>().map_err(|e| format!("ref index {i}: {e}"))?),
                None => None,
            };
            return Ok(ObjRef { container: c, tag: t, index: i });
        }
        let c = hex(v["container"].as_str().ok_or("ref needs \"container\" (hex)")?)?;
        let t = hex(v["tag"].as_str().ok_or("ref needs \"tag\" (hex)")?)?;
        let i = match v.get("index") {
            None | Some(Value::Null) => None,
            Some(i) => Some(i.as_u64().ok_or("ref \"index\" must be a number")? as usize),
        };
        Ok(ObjRef { container: c, tag: t, index: i })
    }

    pub fn resolve(&self, s: &Session) -> Result<usize, String> {
        let entry = s.entry(self.container).ok_or_else(|| format!("container {:08x} not found", self.container))?;
        let hits: Vec<usize> = entry.objects().iter().enumerate().filter(|(_, o)| o.tag() == self.tag).map(|(i, _)| i).collect();
        if let Some(i) = self.index {
            if hits.contains(&i) {
                return Ok(i);
            }
            return Err(format!("object {i} in container {:08x} does not carry tag {:08x}", self.container, self.tag));
        }
        match hits.as_slice() {
            [i] => Ok(*i),
            [] => Err(format!("no object with tag {:08x} in container {:08x}", self.tag, self.container)),
            many => Err(format!("tag {:08x} occurs {} times in container {:08x}; give \"index\"", self.tag, many.len(), self.container)),
        }
    }

    pub fn json(&self) -> Value {
        let mut v = json!({ "container": format!("{:08x}", self.container), "tag": format!("{:08x}", self.tag) });
        if let Some(i) = self.index {
            v["index"] = json!(i);
        }
        v
    }
}

fn hex(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).map_err(|e| format!("{s:?} is not a hex u32: {e}"))
}

fn f3(v: &Value, key: &str) -> Result<[f32; 3], String> {
    let a = v[key].as_array().filter(|a| a.len() == 3).ok_or_else(|| format!("\"{key}\" must be [x, y, z]"))?;
    let mut out = [0f32; 3];
    for (k, x) in a.iter().enumerate() {
        out[k] = x.as_f64().ok_or_else(|| format!("\"{key}\"[{k}] is not a number"))? as f32;
        if !out[k].is_finite() {
            return Err(format!("\"{key}\"[{k}] is not finite"));
        }
    }
    Ok(out)
}

fn short_name(tag: u32) -> String {
    editnodes::tag_name(tag).map(|n| n.rsplit('\\').next().unwrap_or(n).to_string()).unwrap_or_else(|| format!("0x{tag:08x}"))
}

fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::Interior => "interior",
        Kind::Exterior => "exterior",
        Kind::Technical => "technical",
    }
}

fn area_json(a: &Area) -> Value {
    json!({
        "name": a.name, "kind": kind_label(a.kind), "script": a.script,
        "anchor": a.anchor, "anchor_note": a.anchor_note,
        "primary": a.primary.as_ref().map(|m| json!({ "name": m.name, "hash": format!("{:08x}", m.hash), "centre": m.centre(), "a": m.a, "b": m.b })),
        "primary_note": a.primary_note,
        "map_objects": a.map_objects.iter().map(|m| json!({ "name": m.name, "hash": format!("{:08x}", m.hash) })).collect::<Vec<_>>(),
        "containers": a.containers().into_iter().map(|(c, n)| json!({ "hash": format!("{c:08x}"), "name": editnodes::tag_name(c), "objects": n })).collect::<Vec<_>>(),
        "objects": a.objects.len(), "availability": a.availability(),
        "openable": a.anchor.is_some() || !a.objects.is_empty(),
    })
}

/// The headless editor: one loaded install, its effective EditNodes session, area cache, selection and
/// clipboard. All edits go through `agent::apply_patch`, so they keep the existing atomicity,
/// `expected_revision` and `patch_id` replay semantics.
pub struct Editor {
    game: PathBuf,
    session: Session,
    areas: Option<Vec<Area>>,
    areas_rev: u64,
    area: Option<usize>,
    selection: Vec<ObjRef>,
    clipboard: Vec<(u32, Node)>,
    clip_cut: bool,
}

impl Editor {
    pub fn open(game: &Path) -> Result<Editor, String> {
        Ok(Editor::new(game, Session::open(game)?))
    }

    /// Test/embedded constructor: no pack I/O beyond what the caller already did.
    pub fn from_session(game: &Path, session: Session) -> Editor {
        Editor::new(game, session)
    }

    fn new(game: &Path, session: Session) -> Editor {
        Editor { game: game.to_path_buf(), session, areas: None, areas_rev: 0, area: None, selection: Vec::new(), clipboard: Vec::new(), clip_cut: false }
    }

    pub fn game(&self) -> &Path {
        &self.game
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn status(&self) -> Value {
        let mut v = agent::status(&self.session);
        v["area"] = self.area.and_then(|i| self.areas.as_ref()?.get(i)).map(|a| json!({ "name": a.name, "kind": kind_label(a.kind) })).unwrap_or(Value::Null);
        v["selection"] = json!(self.selection.iter().map(ObjRef::json).collect::<Vec<_>>());
        v["clipboard"] = json!({ "objects": self.clipboard.len(), "cut": self.clip_cut });
        v
    }

    fn ensure_areas(&mut self) -> Result<(), String> {
        if self.areas.is_none() || self.areas_rev != self.session.revision() {
            self.areas = Some(areas::build(&self.game, &self.session)?);
            self.areas_rev = self.session.revision();
        }
        Ok(())
    }

    pub fn areas(&mut self) -> Result<Vec<Value>, String> {
        self.ensure_areas()?;
        Ok(self.areas.as_ref().map(|l| l.iter().map(area_json).collect()).unwrap_or_default())
    }

    /// Open one area as the query/edit context. `name`, `name_contains` and `kind` select it; exactly
    /// one match is required. Headless "open" means the object set is the area's list, not a camera.
    pub fn open_area(&mut self, args: &Value) -> Result<Value, String> {
        self.ensure_areas()?;
        let list = self.areas.as_ref().unwrap();
        let name = args["name"].as_str().map(|s| s.to_ascii_lowercase());
        let contains = args["name_contains"].as_str().map(|s| s.to_ascii_lowercase());
        let kind = args["kind"].as_str().map(|s| s.to_ascii_lowercase());
        let matches: Vec<usize> = list
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                name.as_ref().map_or(true, |n| a.name.to_ascii_lowercase() == *n)
                    && contains.as_ref().map_or(true, |n| a.name.to_ascii_lowercase().contains(n))
                    && kind.as_ref().map_or(true, |k| kind_label(a.kind) == k)
            })
            .map(|(i, _)| i)
            .collect();
        let i = match matches.as_slice() {
            [i] => *i,
            [] => return Err(format!("no area matches {args}; use areas to list names")),
            many => {
                let names: Vec<&str> = many.iter().take(12).map(|i| list[*i].name.as_str()).collect();
                return Err(format!("{} areas match; narrow with name/kind: {names:?}", many.len()));
            }
        };
        let a = &list[i];
        if a.anchor.is_none() && a.objects.is_empty() {
            return Err(format!("{} cannot be opened: {}", a.name, a.anchor_note));
        }
        self.area = Some(i);
        Ok(json!({ "ok": true, "area": area_json(a), "revision": self.session.revision() }))
    }

    pub fn area(&self) -> Option<&Area> {
        self.area.and_then(|i| self.areas.as_ref()?.get(i))
    }

    /// Query objects. Filters are ANDed: `area`, `class`, `class_contains`, `name_contains`, `type`,
    /// `container`, `radius{center,m}`, `limit` (default 100), `sort` ("distance" when a radius is set).
    pub fn query(&mut self, args: &Value) -> Result<Value, String> {
        let area_name = args["area"].as_str().map(str::to_string);
        if let Some(n) = &area_name {
            // Resolve area by exact name; reuse open_area's selector rules without changing context.
            self.ensure_areas()?;
            let list = self.areas.as_ref().unwrap();
            let idx = list.iter().position(|a| a.name.eq_ignore_ascii_case(n)).ok_or_else(|| format!("no area named {n}"))?;
            self.area = Some(idx);
        } else if self.area.is_some() {
            self.ensure_areas()?;
        }
        let class = args["class"].as_str().map(|s| s.to_ascii_lowercase());
        let class_contains = args["class_contains"].as_str().map(|s| s.to_ascii_lowercase());
        let name_contains = args["name_contains"].as_str().map(|s| s.to_ascii_lowercase());
        let ty_filter = args["type"].as_str().map(|s| s.to_ascii_lowercase());
        let container = args["container"].as_str().map(hex).transpose()?;
        let radius = match args.get("radius") {
            None | Some(Value::Null) => None,
            Some(r) => {
                let m = r["m"].as_f64().ok_or("radius needs \"m\"")? as f32;
                Some((f3(r, "center")?, m))
            }
        };
        let limit = args["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize;
        let sort_distance = args["sort"].as_str() == Some("distance") || radius.is_some();

        let mut hits: Vec<(f32, ObjRef, Value)> = Vec::new();
        let mut push = |s: &Session, c: u32, i: usize, dist: f32| {
            let Some(o) = s.entry(c).and_then(|e| e.objects().get(i)) else { return };
            let name = editnodes::tag_name(o.tag());
            let (oc, type_name) = (o.class_name().unwrap_or_default(), o.type_name().unwrap_or_default());
            if let Some(want) = &class {
                if oc.to_ascii_lowercase() != *want {
                    return;
                }
            }
            if let Some(want) = &class_contains {
                if !oc.to_ascii_lowercase().contains(want) {
                    return;
                }
            }
            let full = name.unwrap_or("");
            if let Some(want) = &name_contains {
                if !full.to_ascii_lowercase().contains(want) {
                    return;
                }
            }
            if let Some(want) = &ty_filter {
                if type_name.to_ascii_lowercase() != *want {
                    return;
                }
            }
            let pos = o.position();
            if let Some((center, m)) = radius {
                let Some(p) = pos else { return };
                let d = ((p[0] - center[0]).powi(2) + (p[1] - center[1]).powi(2) + (p[2] - center[2]).powi(2)).sqrt();
                if d > m {
                    return;
                }
                hits.push((
                    d,
                    ObjRef { container: c, tag: o.tag(), index: Some(i) },
                    json!({ "ref": ObjRef { container: c, tag: o.tag(), index: Some(i) }.json(), "name": name,
                            "short_name": short_name(o.tag()), "class": oc, "type": type_name, "position": pos,
                            "container": format!("{c:08x}"), "container_name": editnodes::tag_name(c), "source": s.source_of(c) }),
                ));
                return;
            }
            hits.push((
                dist,
                ObjRef { container: c, tag: o.tag(), index: Some(i) },
                json!({ "ref": ObjRef { container: c, tag: o.tag(), index: Some(i) }.json(), "name": name,
                        "short_name": short_name(o.tag()), "class": oc, "type": type_name, "position": pos,
                        "container": format!("{c:08x}"), "container_name": editnodes::tag_name(c), "source": s.source_of(c) }),
            ));
        };

        if let Some(ai) = self.area {
            let area = self.areas.as_ref().and_then(|l| l.get(ai)).ok_or("area context is stale")?;
            for o in &area.objects {
                if container.map_or(false, |c| c != o.obj.0) {
                    continue;
                }
                push(&self.session, o.obj.0, o.obj.1, 0.0);
            }
        } else {
            for c in self.session.containers() {
                if container.map_or(false, |want| want != c.hash) {
                    continue;
                }
                let Some(e) = self.session.entry(c.hash) else { continue };
                for i in 0..e.objects().len() {
                    push(&self.session, c.hash, i, 0.0);
                }
            }
        }
        if sort_distance {
            hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        let objects: Vec<Value> = hits.into_iter().take(limit).map(|(_, _, v)| v).collect();
        Ok(json!({ "objects": objects, "count": objects.len(), "revision": self.session.revision(),
                   "area": self.area().map(|a| a.name.clone()) }))
    }

    pub fn get(&mut self, args: &Value) -> Result<Value, String> {
        let r = ObjRef::from_json(args.get("ref").unwrap_or(args))?;
        let i = r.resolve(&self.session)?;
        agent::inspect(&self.session, r.container, i)
    }

    pub fn select(&mut self, args: &Value) -> Result<Value, String> {
        let refs = self.refs(args)?;
        self.selection = refs;
        Ok(json!({ "ok": true, "selection": self.selection.iter().map(ObjRef::json).collect::<Vec<_>>() }))
    }

    fn refs(&self, args: &Value) -> Result<Vec<ObjRef>, String> {
        if args["refs"].as_str() == Some("selection") || args["selection"] == true {
            if self.selection.is_empty() {
                return Err("selection is empty; pass refs or use select first".into());
            }
            return Ok(self.selection.clone());
        }
        if let Some(a) = args["refs"].as_array() {
            return a.iter().map(ObjRef::from_json).collect();
        }
        if let Some(r) = args.get("ref") {
            return Ok(vec![ObjRef::from_json(r)?]);
        }
        if args.get("container").is_some() && args.get("tag").is_some() {
            return Ok(vec![ObjRef::from_json(args)?]);
        }
        if self.selection.is_empty() {
            return Err("no \"ref\"/\"refs\" given and the selection is empty".into());
        }
        Ok(self.selection.clone())
    }

    pub fn copy(&mut self, args: &Value) -> Result<Value, String> {
        let refs = self.refs(args)?;
        let mut nodes = Vec::new();
        for r in &refs {
            let i = r.resolve(&self.session)?;
            let o = self.session.entry(r.container).and_then(|e| e.objects().get(i)).ok_or_else(|| format!("object {} gone", i))?;
            nodes.push((r.container, o.clone()));
        }
        self.clipboard = nodes;
        self.clip_cut = false;
        Ok(json!({ "ok": true, "copied": self.clipboard.len() }))
    }

    /// Cut is a copy followed by one delete step; if the delete fails the clipboard is unchanged.
    pub fn cut(&mut self, args: &Value) -> Result<Value, String> {
        let refs = self.refs(args)?;
        let mut nodes = Vec::new();
        let mut pairs = Vec::new();
        for r in &refs {
            let i = r.resolve(&self.session)?;
            let o = self.session.entry(r.container).and_then(|e| e.objects().get(i)).cloned().ok_or_else(|| format!("object {} gone", i))?;
            nodes.push((r.container, o));
            pairs.push((r.container, i));
        }
        let n = self.session.remove_objects(&pairs)?;
        self.clipboard = nodes;
        self.clip_cut = true;
        Ok(json!({ "ok": true, "cut": n, "revision": self.session.revision() }))
    }

    pub fn undo(&mut self) -> Result<Value, String> {
        let any = self.session.undo()?;
        Ok(json!({ "ok": true, "undone": any, "revision": self.session.revision() }))
    }

    pub fn redo(&mut self) -> Result<Value, String> {
        let any = self.session.redo()?;
        Ok(json!({ "ok": true, "redone": any, "revision": self.session.revision() }))
    }

    /// Expand a high-level patch (which may use `ref`/`refs`, duplicate, paste, group actions and typed
    /// parameters) into the existing agent operation form, then run it atomically or resolve it.
    fn expand(&mut self, patch: &Value) -> Result<Expansion, String> {
        let ops = patch["operations"].as_array().ok_or("patch has no \"operations\" array")?;
        if ops.is_empty() {
            return Err("patch has no operations".into());
        }
        let mut out = Vec::new();
        let mut minted: Vec<(u32, String)> = Vec::new();
        let mut evidence = Vec::new();
        let mut used: Vec<u32> = Vec::new();
        for op in ops {
            let action = op["action"].as_str().ok_or("operation has no \"action\"")?;
            match action {
                "duplicate" => {
                    let refs = self.refs(op)?;
                    let count = op["count"].as_u64().unwrap_or(1).clamp(1, 100) as usize;
                    let offset = if op.get("offset").is_some() { f3(op, "offset")? } else { [0.0; 3] };
                    for r in &refs {
                        let i = r.resolve(&self.session)?;
                        let source = self.session.entry(r.container).and_then(|e| e.objects().get(i)).cloned().ok_or_else(|| format!("object {} gone", i))?;
                        for k in 1..=count {
                            let (tag, name) = self.session.mint_tag_excluding(r.container, source.tag(), &used);
                            used.push(tag);
                            if let Some(n) = &name {
                                minted.push((tag, n.clone()));
                            }
                            let mut node = source.clone();
                            node.set_tag(tag);
                            if let Some(p) = source.position() {
                                let _ = node.set_position([p[0] + offset[0] * k as f32, p[1] + offset[1] * k as f32, p[2] + offset[2] * k as f32]);
                            }
                            out.push(json!({ "action": "insert", "container": format!("{:08x}", r.container), "index": null, "node": node_json(&node) }));
                            evidence.push("kind demonstrated on locators (insert); this particular copy is not verified");
                        }
                    }
                }
                "paste" => {
                    if self.clipboard.is_empty() {
                        return Err("clipboard is empty; copy or cut first".into());
                    }
                    let mut offset = if op.get("offset").is_some() { f3(op, "offset")? } else { [0.0; 3] };
                    if op["in_place"] != true {
                        if op.get("target").is_some() {
                            let target = f3(op, "target")?;
                            let ps: Vec<[f32; 3]> = self.clipboard.iter().filter_map(|(_, n)| n.position()).collect();
                            let n = ps.len().max(1) as f32;
                            let c = ps.iter().fold([0.0; 3], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n, a[2] + p[2] / n]);
                            offset = [offset[0] + target[0] - c[0], offset[1] + target[1] - c[1], offset[2] + target[2] - c[2]];
                        }
                    }
                    let keep = op["keep_tags"].as_bool().unwrap_or(false) && self.clip_cut;
                    for (c, node0) in self.clipboard.clone() {
                        let mut node = node0.clone();
                        let tag_free = !self.session.entry(c).map_or(false, |e| e.objects().iter().any(|o| o.tag() == node.tag())) && !used.contains(&node.tag());
                        if !(keep && tag_free) {
                            let (tag, name) = self.session.mint_tag_excluding(c, node.tag(), &used);
                            used.push(tag);
                            if let Some(n) = &name {
                                minted.push((tag, n.clone()));
                            }
                            node.set_tag(tag);
                        }
                        if let Some(p) = node.position() {
                            let _ = node.set_position([p[0] + offset[0], p[1] + offset[1], p[2] + offset[2]]);
                        }
                        out.push(json!({ "action": "insert", "container": format!("{c:08x}"), "index": null, "node": node_json(&node) }));
                        evidence.push("kind demonstrated on locators (insert); this particular paste is not verified");
                    }
                }
                "rotate" => {
                    for r in self.refs(op)? {
                        out.push(self.with_ref(op, r, action)?);
                        evidence.push(if op["axis"].as_str().map_or(true, |a| a.eq_ignore_ascii_case("y")) { "kind demonstrated on locators (yaw); tilts not demonstrated" } else { "statically checked only (not a demonstrated rotation)" });
                    }
                }
                "scale" => {
                    for r in self.refs(op)? {
                        out.push(self.with_ref(op, r, action)?);
                        evidence.push("statically checked only (uniform basis scale; not verified in game)");
                    }
                }
                "delete" => {
                    for r in self.refs(op)? {
                        out.push(self.with_ref(op, r, action)?);
                        evidence.push("kind demonstrated on locators (delete); this particular delete is not verified");
                    }
                }
                "set_class" => {
                    for r in self.refs(op)? {
                        out.push(self.with_ref(op, r, action)?);
                        evidence.push("statically checked only (ClassName change not verified in game)");
                    }
                }
                "set_param" => {
                    let r = ObjRef::from_json(op.get("ref").ok_or("set_param needs \"ref\"")?)?;
                    out.push(self.param_op(op, r)?);
                    evidence.push(if op.get("param").and_then(|p| p.as_str()) == Some("Position") { "kind demonstrated on locators (Position); this particular edit is not verified" } else { "statically checked only (parameter not demonstrated in game)" });
                }
                "set_lua" => {
                    let r = ObjRef::from_json(op.get("ref").ok_or("set_lua needs \"ref\"")?)?;
                    out.push(self.lua_op(op, r)?);
                    evidence.push("statically checked only (LuaParam not verified in game)");
                }
                "move" | "offset" => {
                    for r in self.refs(op)? {
                        out.push(self.with_ref(op, r, action)?);
                        evidence.push("kind demonstrated on locators (Position); this particular move is not verified");
                    }
                }
                other => return Err(format!("unknown action {other:?}")),
            }
        }
        let mut normalized = json!({ "operations": out });
        for k in ["patch_id", "expected_revision"] {
            if patch.get(k).is_some() {
                normalized[k] = patch[k].clone();
            }
        }
        Ok(Expansion { patch: normalized, minted, evidence })
    }

    /// Copy a single-ref operation to the normalized agent form for one object.
    fn with_ref(&self, op: &Value, r: ObjRef, action: &str) -> Result<Value, String> {
        let mut v = op.clone();
        v["container"] = json!(format!("{:08x}", r.container));
        v["tag"] = json!(format!("{:08x}", r.tag));
        if let Some(i) = r.index {
            v["index"] = json!(i);
        } else {
            let i = r.resolve(&self.session)?;
            v["index"] = json!(i);
        }
        v["action"] = json!(action);
        v.as_object_mut().unwrap().remove("ref");
        v.as_object_mut().unwrap().remove("refs");
        Ok(v)
    }

    /// Convert a typed `set_param` value to the agent's `value_hex` form.
    fn param_op(&self, op: &Value, r: ObjRef) -> Result<Value, String> {
        let mut v = self.with_ref(op, r, "set_param")?;
        if v.get("value_hex").is_none() {
            op["param"].as_str().ok_or("set_param needs \"param\" (name or 0x tag)")?;
            let ty = op["type"].as_str().unwrap_or_else(|| {
                match op.get("value") {
                    Some(Value::Array(a)) if a.len() == 3 => "vec3",
                    Some(Value::String(_)) => "string",
                    Some(Value::Number(_)) => "f32",
                    _ => "f32",
                }
            });
            let value = op.get("value").ok_or("set_param needs \"value\" or \"value_hex\"")?;
            let bytes = encode_value(ty, value)?;
            v["value_hex"] = json!(crate::editnodes_stage::hex(&bytes));
        }
        Ok(v)
    }

    /// Convert a typed LuaParam change (`set_lua`) to a raw `set_param` on the decoded leaf.
    fn lua_op(&self, op: &Value, r: ObjRef) -> Result<Value, String> {
        let i = r.resolve(&self.session)?;
        let o = self.session.entry(r.container).and_then(|e| e.objects().get(i)).ok_or("object gone")?;
        let occurrence = op["occurrence"].as_u64().unwrap_or(0) as usize;
        let idx = *o.params_with_tag(tags::LUA_PARAM).get(occurrence).ok_or_else(|| format!("object has no LuaParam #{occurrence}"))?;
        let Node::Leaf { data, .. } = o.params().get(idx).ok_or("LuaParam is not a leaf")? else { return Err("LuaParam is not a leaf".into()) };
        let mut l = editnodes::lua_param(data)?;
        l.value = parse_lua_value(op.get("value").ok_or("set_lua needs \"value\"")?)?;
        let new = editnodes::lua_param_bytes(&l);
        let mut v = self.with_ref(op, r, "set_param")?;
        v["param"] = json!("LuaParam");
        v["occurrence"] = json!(occurrence);
        v["value_hex"] = json!(crate::editnodes_stage::hex(&new));
        Ok(v)
    }

    /// Apply a high-level patch atomically. `plan` runs it and rolls back, changing nothing.
    pub fn edit(&mut self, args: &Value, plan: bool) -> Result<Value, Value> {
        // Replaying a seen patch_id returns its receipt without re-expanding operations against a
        // changed session (the same guarantee as `agent::apply_patch`).
        if !plan {
            if let Some(id) = args["patch_id"].as_str() {
                if let Some((_, receipt)) = self.session.patches.iter().find(|(p, _)| p == id) {
                    let mut r = receipt.clone();
                    r["replayed"] = json!(true);
                    return Ok(r);
                }
            }
        }
        let exp = self.expand(args).map_err(|e| json!({ "ok": false, "error": e, "revision": self.session.revision() }))?;
        let before = self.session.revision();
        let res = if plan { agent::resolve_patch(&mut self.session, &exp.patch) } else { agent::apply_patch(&mut self.session, &exp.patch) };
        match res {
            Ok(mut v) => {
                if !plan {
                    for (tag, name) in &exp.minted {
                        self.session.note_name(*tag, name);
                    }
                }
                if let Some(results) = v["results"].as_array_mut() {
                    for (k, r) in results.iter_mut().enumerate() {
                        if let Some(e) = exp.evidence.get(k) {
                            r["evidence"] = json!(e);
                        }
                    }
                }
                v["revision_before"] = json!(before);
                v["revision_after"] = json!(self.session.revision());
                v["ok"] = json!(true);
                v["applied"] = json!(!plan);
                Ok(v)
            }
            Err(e) => Err(e),
        }
    }

    pub fn stage(&self, args: &Value) -> Result<Value, String> {
        let out = args["out"].as_str().or_else(|| args["out_dir"].as_str()).ok_or("stage needs \"out\"")?;
        let o = self.session.stage(Path::new(out))?;
        Ok(json!({ "ok": true, "out_dir": o.out_dir.display().to_string(), "all_static_checks_pass": o.all_pass,
                   "artifact_sha256": o.artifact_sha, "report": o.report }))
    }

    /// Dump state. `scope`: status|selection|journal|area|areas|clipboard|all.
    pub fn dump(&mut self, args: &Value) -> Result<Value, String> {
        let scope = args["scope"].as_str().unwrap_or("status");
        match scope {
            "status" => Ok(self.status()),
            "selection" => Ok(json!({ "selection": self.selection.iter().map(ObjRef::json).collect::<Vec<_>>() })),
            "journal" => Ok(agent::status(&self.session)),
            "clipboard" => Ok(json!({ "objects": self.clipboard.len(), "cut": self.clip_cut,
                                      "names": self.clipboard.iter().map(|(_, n)| short_name(n.tag())).collect::<Vec<_>>() })),
            "area" => Ok(self.area().map(area_json).unwrap_or(Value::Null)),
            "areas" => Ok(json!({ "areas": self.areas()? })),
            "all" => {
                let status = self.status();
                let areas = self.areas()?;
                Ok(json!({ "status": status, "areas": areas, "selection": self.selection.iter().map(ObjRef::json).collect::<Vec<_>>(),
                           "clipboard": { "objects": self.clipboard.len(), "cut": self.clip_cut } }))
            }
            other => Err(format!("unknown dump scope {other:?} (status, selection, journal, area, areas, clipboard, all)")),
        }
    }

    /// Simple assertions for verification scripts. Each check is `{ "kind": ... }`; the result has
    /// `ok` overall and one record per check.
    pub fn assert(&mut self, args: &Value) -> Result<Value, String> {
        let checks = args["checks"].as_array().ok_or("assert needs a \"checks\" array")?;
        let mut out = Vec::new();
        let mut ok = true;
        for c in checks {
            let kind = c["kind"].as_str().ok_or("each check needs \"kind\"")?;
            let (pass, detail) = match kind {
                "exists" | "absent" => {
                    let mut q = c.clone();
                    q.as_object_mut().unwrap().remove("kind");
                    let r = self.query(&q)?;
                    let n = r["count"].as_u64().unwrap_or(0);
                    let pass = if kind == "exists" { n > 0 } else { n == 0 };
                    (pass, json!({ "count": n }))
                }
                "position" => {
                    let r = ObjRef::from_json(c.get("ref").ok_or("position check needs \"ref\"")?)?;
                    let i = r.resolve(&self.session)?;
                    let got = self.session.entry(r.container).and_then(|e| e.objects().get(i)).and_then(|o| o.position());
                    let want = f3(c, "expect")?;
                    let tol = c["tolerance"].as_f64().unwrap_or(1e-3) as f32;
                    let d = got.map(|p| ((p[0] - want[0]).powi(2) + (p[1] - want[1]).powi(2) + (p[2] - want[2]).powi(2)).sqrt());
                    (d.map_or(false, |d| d <= tol), json!({ "got": got, "want": want, "distance": d, "tolerance": tol }))
                }
                "class" => {
                    let r = ObjRef::from_json(c.get("ref").ok_or("class check needs \"ref\"")?)?;
                    let i = r.resolve(&self.session)?;
                    let got = self.session.entry(r.container).and_then(|e| e.objects().get(i)).and_then(|o| o.class_name());
                    let want = c["expect"].as_str().map(str::to_string);
                    (got == want, json!({ "got": got, "want": want }))
                }
                "param" => {
                    let r = ObjRef::from_json(c.get("ref").ok_or("param check needs \"ref\"")?)?;
                    let i = r.resolve(&self.session)?;
                    let p = c["param"].as_str().ok_or("param check needs \"param\"")?;
                    let tag = if p.starts_with("0x") { hex(p)? } else { pandemic_hash(p) };
                    let occ = c["occurrence"].as_u64().unwrap_or(0) as usize;
                    let o = self.session.entry(r.container).and_then(|e| e.objects().get(i));
                    let idx = o.and_then(|o| o.params_with_tag(tag).get(occ).copied());
                    let got = idx.and_then(|k| match o?.params().get(k)? { Node::Leaf { data, .. } => Some(data.clone()), _ => None });
                    let want = c.get("expect_hex").map(|v| unhex(v.as_str().unwrap_or("")).unwrap_or_default());
                    let pass = match (&got, &want) { (Some(g), Some(w)) => g == w, _ => false };
                    (pass, json!({ "got_hex": got.as_ref().map(|g| crate::editnodes_stage::hex(g)), "want_hex": want.map(|w| crate::editnodes_stage::hex(&w)) }))
                }
                "revision" => {
                    let want = c["expect"].as_u64().ok_or("revision check needs numeric \"expect\"")?;
                    (self.session.revision() == want, json!({ "got": self.session.revision(), "want": want }))
                }
                "stage_report" => {
                    let dir = c["out_dir"].as_str().ok_or("stage_report needs \"out_dir\"")?;
                    let report = std::fs::read_to_string(Path::new(dir).join("report.txt")).unwrap_or_default();
                    let want = c["expect_contains"].as_str().unwrap_or("ALL STATIC CHECKS PASS: true");
                    (report.contains(want), json!({ "report_contains": want, "bytes": report.len() }))
                }
                other => return Err(format!("unknown check kind {other:?}")),
            };
            ok &= pass;
            out.push(json!({ "kind": kind, "ok": pass, "detail": detail }));
        }
        Ok(json!({ "ok": ok, "passed": out.iter().filter(|c| c["ok"] == true).count(), "failed": out.iter().filter(|c| c["ok"] == false).count(), "checks": out }))
    }
}

struct Expansion {
    patch: Value,
    minted: Vec<(u32, String)>,
    evidence: Vec<&'static str>,
}

fn encode_value(ty: &str, v: &Value) -> Result<Vec<u8>, String> {
    match ty {
        "vec3" => Ok(f3(v, "value")?.iter().flat_map(|x| x.to_le_bytes()).collect()),
        "f32" => {
            let x = v["value"].as_f64().ok_or("f32 value must be a number")? as f32;
            if !x.is_finite() {
                return Err("f32 value is not finite".into());
            }
            Ok(x.to_le_bytes().to_vec())
        }
        "u32" => {
            let x = v["value"].as_u64().ok_or("u32 value must be a non-negative integer")?;
            Ok((x as u32).to_le_bytes().to_vec())
        }
        "string" => {
            let s = v["value"].as_str().ok_or("string value must be a string")?;
            let mut b = s.as_bytes().to_vec();
            b.push(0);
            Ok(b)
        }
        other => Err(format!("unknown value type {other:?}")),
    }
}

fn parse_lua_value(v: &Value) -> Result<LuaValue, String> {
    if let Some(b) = v.as_bool() {
        return Ok(LuaValue::Bool(b as u8));
    }
    if let Some(i) = v.as_i64() {
        return Ok(LuaValue::Int(i as i32));
    }
    if let Some(f) = v.as_f64() {
        return Ok(LuaValue::Float(f as f32));
    }
    if let Some(s) = v.as_str() {
        if let Some(h) = s.strip_prefix("0x") {
            return Ok(LuaValue::Crc(hex(h)?));
        }
        return Ok(LuaValue::Str(s.to_string()));
    }
    if let Some(o) = v.as_object() {
        if let Some(h) = o.get("crc").and_then(|x| x.as_str()) {
            return Ok(LuaValue::Crc(hex(h)?));
        }
        if let Some(a) = o.get("crc_list").and_then(|x| x.as_array()) {
            let mut out = Vec::new();
            for x in a {
                out.push(x.as_str().ok_or("crc_list entries must be strings")?.to_string());
            }
            return Ok(LuaValue::CrcList(out));
        }
    }
    Err("set_lua value must be bool, integer, float, string, {\"crc\":\"0x..\"} or {\"crc_list\":[...]}".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage::EditNodesSources;
    use sab_formats::editnodes::{Entry, Pack, Root};

    fn object(tag: u32, class: &str, pos: [f32; 3]) -> Node {
        let mut n = Node::Container {
            tag,
            children: vec![
                Node::Leaf { tag: tags::CLASS_NAME, data: format!("{class}\0").into_bytes() },
                Node::Leaf { tag: tags::POSITION, data: pos.iter().flat_map(|v| v.to_le_bytes()).collect() },
                Node::Leaf { tag: tags::LUA_PARAM, data: b"a\0\x31\x4f\x44\x1b\x01".to_vec() },
            ],
        };
        n.set_yaw_degrees(0.0).unwrap();
        n
    }

    fn editor() -> Editor {
        let base = Pack {
            entries: vec![Entry {
                hash: 0x10,
                root: Root::EditNode { objects: vec![object(0xA, "Table", [0.0, 0.0, 0.0]), object(0xB, "Chair", [2.0, 0.0, 0.0]), object(0xC, "Chair", [4.0, 0.0, 0.0])] },
            }],
        };
        let s = Session::from_sources(Path::new("G"), EditNodesSources::from_bytes(base.write(), Pack { entries: vec![] }.write(), "LF".into()).unwrap());
        Editor::from_session(Path::new("G"), s)
    }

    fn selected(e: &Editor) -> Value {
        json!(e.selection.iter().map(ObjRef::json).collect::<Vec<_>>())
    }

    #[test]
    fn query_and_edit_cover_move_rotate_scale_class_param_and_undo() {
        let mut e = editor();
        let q = e.query(&json!({ "class": "Chair" })).unwrap();
        assert_eq!(q["count"], 2);
        let chair = ObjRef::from_json(&q["objects"][0]["ref"]).unwrap();
        e.select(&chair.json()).unwrap();
        assert_eq!(selected(&e).as_array().unwrap().len(), 1);

        // move + rotate + scale in one atomic patch
        let r = e.edit(&json!({ "patch_id": "p1", "expected_revision": 0, "operations": [
            { "action": "offset", "refs": "selection", "delta": [1, 2, 3] },
            { "action": "rotate", "refs": "selection", "axis": "y", "degrees": 90 },
            { "action": "scale", "refs": "selection", "factor": 2 }
        ]}), false).unwrap();
        assert_eq!(r["revision_after"], 1);
        assert_eq!(r["results"].as_array().unwrap().len(), 3);
        assert!(r["results"][2]["evidence"].as_str().unwrap().contains("statically checked"));
        let p = e.query(&json!({ "class": "Chair" })).unwrap()["objects"][0]["position"].clone();
        assert_eq!(p, json!([3.0, 2.0, 3.0]));

        // class + LuaParam
        e.edit(&json!({ "operations": [
            { "action": "set_class", "ref": chair.json(), "class": "Mug" },
            { "action": "set_lua", "ref": chair.json(), "occurrence": 0, "value": "hello" }
        ]}), false).unwrap();
        let got = e.get(&chair.json()).unwrap();
        assert_eq!(got["class"], "Mug");
        assert_eq!(got["params"][2]["name"], "LuaParam");

        assert!(e.undo().unwrap()["undone"].as_bool().unwrap());
        assert!(e.undo().unwrap()["undone"].as_bool().unwrap());
        assert_eq!(e.query(&json!({ "class": "Chair" })).unwrap()["count"], 2);
    }

    #[test]
    fn duplicate_and_paste_are_one_step_and_undo_leaves_nothing() {
        let mut e = editor();
        let q = e.query(&json!({ "class": "Chair" })).unwrap();
        let chair = ObjRef::from_json(&q["objects"][0]["ref"]).unwrap();
        e.edit(&json!({ "operations": [{ "action": "duplicate", "ref": chair.json(), "count": 2, "offset": [0, 1, 0] }] }), false).unwrap();
        assert_eq!(e.query(&json!({ "class": "Chair" })).unwrap()["count"], 4);
        e.undo().unwrap();
        assert_eq!(e.query(&json!({ "class": "Chair" })).unwrap()["count"], 2);

        e.select(&chair.json()).unwrap();
        e.copy(&json!({ "refs": "selection" })).unwrap();
        e.edit(&json!({ "operations": [{ "action": "paste", "in_place": true }] }), false).unwrap();
        assert_eq!(e.query(&json!({ "class": "Chair" })).unwrap()["count"], 3);
        assert_eq!(e.query(&json!({ "class": "Chair" })).unwrap()["objects"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_seen_patch_id_replays_its_receipt_without_reexpanding() {
        let mut e = editor();
        let q = e.query(&json!({ "class": "Table" })).unwrap();
        let table = ObjRef::from_json(&q["objects"][0]["ref"]).unwrap();
        let p = json!({ "patch_id": "replay-me", "operations": [{ "action": "offset", "ref": table.json(), "delta": [1, 0, 0] }] });
        let first = e.edit(&p, false).unwrap();
        assert_eq!(first["replayed"], Value::Null);
        let rev = e.session().revision();
        let second = e.edit(&p, false).unwrap();
        assert_eq!(second["replayed"], true, "{second}");
        assert_eq!(e.session().revision(), rev, "a replay changes nothing");
        assert_eq!(e.get(&table.json()).unwrap()["position"], json!([1.0, 0.0, 0.0]));
    }

    #[test]
    fn plan_changes_nothing_and_assertions_report_state() {
        let mut e = editor();
        let q = e.query(&json!({ "class": "Table" })).unwrap();
        let table = ObjRef::from_json(&q["objects"][0]["ref"]).unwrap();
        let plan = e.edit(&json!({ "operations": [{ "action": "offset", "ref": table.json(), "delta": [5, 0, 0] }] }), true).unwrap();
        assert_eq!(plan["applied"], false);
        let r = e.assert(&json!({ "checks": [
            { "kind": "position", "ref": table.json(), "expect": [0, 0, 0] },
            { "kind": "revision", "expect": 0 },
            { "kind": "exists", "class": "Table" },
            { "kind": "absent", "class": "Spore" }
        ]})).unwrap();
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["passed"], 4);
    }

    #[test]
    fn delete_is_one_step_and_cut_then_paste_restores_a_tag() {
        let mut e = editor();
        let q = e.query(&json!({ "class": "Table" })).unwrap();
        let table = ObjRef::from_json(&q["objects"][0]["ref"]).unwrap();
        e.edit(&json!({ "operations": [{ "action": "delete", "ref": table.json() }] }), false).unwrap();
        assert_eq!(e.query(&json!({ "class": "Table" })).unwrap()["count"], 0);
        e.undo().unwrap();
        assert_eq!(e.query(&json!({ "class": "Table" })).unwrap()["count"], 1);
        e.select(&table.json()).unwrap();
        e.cut(&json!({ "refs": "selection" })).unwrap();
        assert_eq!(e.query(&json!({ "class": "Table" })).unwrap()["count"], 0);
        e.edit(&json!({ "operations": [{ "action": "paste", "in_place": true, "keep_tags": true }] }), false).unwrap();
        assert_eq!(e.query(&json!({ "class": "Table" })).unwrap()["count"], 1);
    }
}
