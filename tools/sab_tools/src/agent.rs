//! Agent interface over a placement [`Session`]: JSON queries and declarative, revision-checked
//! patches, so a program (or an AI agent) can search the world data, inspect objects, preview a set
//! of edits and apply exactly that set.
//!
//! The lifecycle follows the agent protocol audited in Ariane (`docs/level_editor/_audit/group_C_ariane.md`):
//! a patch carries an optional `patch_id` (re-applying the same id returns the stored receipt and
//! changes nothing) and an optional `expected_revision` (refused when the session has changed since
//! the caller looked). [`resolve_patch`] validates without changing anything. [`apply_patch`] is
//! all-or-nothing: if any operation fails, every edit it made is rolled back.
//!
//! Patch:
//! ```json
//! { "patch_id": "lift-blip-1", "expected_revision": 0,
//!   "operations": [
//!     { "action": "move",   "container": "6d21e53e", "tag": "ff3106f1", "position": [779.83, 52.75, -718.41] },
//!     { "action": "offset", "container": "6d21e53e", "tag": "ff3106f1", "delta": [0, 1, 0] },
//!     { "action": "set_param", "container": "...", "tag": "...", "param": "Position", "occurrence": 0, "value_hex": "..." }
//!   ] }
//! ```
//! An object is addressed by container hash plus object tag; `index` disambiguates the rare tags
//! repeated inside one container (retail ships 13). A move of more than 1,000 m is refused unless the
//! operation sets `"allow_far": true`, a guard against swapped or mistyped coordinates.

use sab_formats::editnodes::{self, tags, Node};
use sab_formats::pandemic_hash;
use serde_json::{json, Value};

use crate::editnodes_stage::{hex, unhex, Edit};
use crate::session::Session;

/// Largest move accepted without `allow_far`, in metres.
pub const MAX_MOVE: f32 = 1000.0;

pub fn parse_hex(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).map_err(|e| format!("{s:?} is not a hex u32: {e}"))
}

fn vec3(d: &[u8]) -> Option<[f32; 3]> {
    (d.len() == 12).then(|| {
        let g = |o: usize| f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
        [g(0), g(4), g(8)]
    })
}

fn short_name(hash: u32) -> String {
    editnodes::tag_name(hash).map(|n| n.rsplit('\\').next().unwrap_or(n).to_string()).unwrap_or_else(|| format!("0x{hash:08x}"))
}

/// The object index in `container` carrying `tag`. A tag repeated in the container needs `index`, which
/// must then point at an object with that tag.
pub fn resolve_object(s: &Session, container: u32, tag: u32, index: Option<usize>) -> Result<usize, String> {
    let entry = s.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
    let hits: Vec<usize> = entry.objects().iter().enumerate().filter(|(_, o)| o.tag() == tag).map(|(i, _)| i).collect();
    match (index, hits.as_slice()) {
        (Some(i), _) if hits.contains(&i) => Ok(i),
        (Some(i), _) => Err(format!("object {i} in container {container:08x} does not carry tag {tag:08x}")),
        (None, [i]) => Ok(*i),
        (None, []) => Err(format!("no object with tag {tag:08x} in container {container:08x}")),
        (None, many) => Err(format!("tag {tag:08x} occurs {} times in container {container:08x} (objects {many:?}); give \"index\"", many.len())),
    }
}

/// Objects whose container path, object name, `Type` or `ClassName` contains `query`
/// (case-insensitive), in container order, at most `limit`.
pub fn search(s: &Session, query: &str, limit: usize) -> Vec<Value> {
    let q = query.to_ascii_lowercase();
    let mut out = Vec::new();
    for c in s.containers() {
        let Some(entry) = s.entry(c.hash) else { continue };
        let cname = c.name.unwrap_or("").to_ascii_lowercase();
        for (i, o) in entry.objects().iter().enumerate() {
            let name = editnodes::tag_name(o.tag()).unwrap_or("");
            let (ty, class) = (o.type_name().unwrap_or_default(), o.class_name().unwrap_or_default());
            let hay = [cname.as_str(), &name.to_ascii_lowercase(), &ty.to_ascii_lowercase(), &class.to_ascii_lowercase()];
            if q.is_empty() || hay.iter().any(|h| h.contains(&q)) {
                out.push(json!({ "container": format!("{:08x}", c.hash), "container_name": c.name, "source": s.source_of(c.hash),
                                 "index": i, "tag": format!("{:08x}", o.tag()), "name": editnodes::tag_name(o.tag()),
                                 "type": ty, "class": class, "position": o.position() }));
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// One object with every parameter in stored order: tag, recovered name, occurrence among same-tag
/// leaves, size, raw hex (up to 64 bytes) and a decoded rendering.
pub fn inspect(s: &Session, container: u32, index: usize) -> Result<Value, String> {
    let o = s.entry(container).and_then(|e| e.objects().get(index)).ok_or_else(|| format!("no object {index} in container {container:08x}"))?;
    let mut seen = std::collections::HashMap::new();
    let params: Vec<Value> = o
        .params()
        .iter()
        .map(|p| {
            let n = seen.entry(p.tag()).or_insert(0usize);
            let occ = *n;
            *n += 1;
            let (len, raw) = match p {
                Node::Leaf { data, .. } => (data.len(), (data.len() <= 64).then(|| hex(data))),
                Node::Container { children, .. } => (children.len(), None),
            };
            json!({ "tag": format!("{:08x}", p.tag()), "name": editnodes::tag_name(p.tag()), "occurrence": occ,
                    "len": len, "hex": raw, "value": format!("{:?}", p.value()) })
        })
        .collect();
    Ok(json!({ "container": format!("{container:08x}"), "container_name": editnodes::tag_name(container), "source": s.source_of(container),
               "index": index, "tag": format!("{:08x}", o.tag()), "name": editnodes::tag_name(o.tag()),
               "type": o.type_name(), "class": o.class_name(), "position": o.position(), "params": params, "revision": s.revision() }))
}

fn err(msg: impl Into<String>, op: Option<usize>, s: &Session) -> Value {
    json!({ "ok": false, "error": msg.into(), "op_index": op, "current_revision": s.revision() })
}

fn f3(v: &Value, key: &str) -> Result<[f32; 3], String> {
    let a = v[key].as_array().filter(|a| a.len() == 3).ok_or_else(|| format!("\"{key}\" must be [x, y, z]"))?;
    let mut out = [0f32; 3];
    for (k, x) in a.iter().enumerate() {
        let f = x.as_f64().ok_or_else(|| format!("\"{key}\"[{k}] is not a number"))? as f32;
        if !f.is_finite() {
            return Err(format!("\"{key}\"[{k}] is not finite"));
        }
        out[k] = f;
    }
    Ok(out)
}

/// Rotation matrix from an axis-angle in degrees (Rodrigues); the session expects a row-major 3×3
/// applied as `r · v`.
fn axis_angle_matrix(axis: [f32; 3], degrees: f32) -> Result<[[f32; 3]; 3], String> {
    let len = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
    if !len.is_finite() || len < 1e-6 {
        return Err("rotation axis is zero".into());
    }
    let (x, y, z) = (axis[0] / len, axis[1] / len, axis[2] / len);
    let (s, c) = degrees.to_radians().sin_cos();
    let t = 1.0 - c;
    Ok([
        [t * x * x + c, t * x * y - s * z, t * x * z + s * y],
        [t * x * y + s * z, t * y * y + c, t * y * z - s * x],
        [t * x * z - s * y, t * y * z + s * x, t * z * z + c],
    ])
}

fn axis_of(op: &Value) -> Result<[f32; 3], String> {
    match &op["axis"] {
        Value::Array(a) if a.len() == 3 => {
            let mut out = [0f32; 3];
            for (k, v) in a.iter().enumerate() {
                out[k] = v.as_f64().ok_or_else(|| format!("axis[{k}] is not a number"))? as f32;
            }
            Ok(out)
        }
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "x" => Ok([1.0, 0.0, 0.0]),
            "y" => Ok([0.0, 1.0, 0.0]),
            "z" => Ok([0.0, 0.0, 1.0]),
            _ => Err(format!("axis {s:?} is not x, y, z or [x, y, z]")),
        },
        Value::Null => Ok([0.0, 1.0, 0.0]),
        _ => Err("\"axis\" must be \"x\"/\"y\"/\"z\" or [x, y, z]".into()),
    }
}

/// Apply one operation to the session; returns its result record.
fn run_op(s: &mut Session, op: &Value) -> Result<Value, String> {
    let action = op["action"].as_str().ok_or("operation has no \"action\"")?;
    // Paste/duplicate expand to an explicit node insert, which needs no existing tag.
    if action == "insert" {
        let container = parse_hex(op["container"].as_str().ok_or("insert needs \"container\"")?)?;
        let index = match op.get("index") {
            None | Some(Value::Null) => s.entry(container).map(|e| e.objects().len()).ok_or_else(|| format!("container {container:08x} not found"))?,
            Some(v) => v.as_u64().ok_or("insert \"index\" must be a number or null")? as usize,
        };
        let node = crate::editnodes_stage::node_from_json(op.get("node").ok_or("insert needs \"node\"")?)?;
        let placed = s.insert_objects(vec![(container, index, node.clone())], "Insert")?;
        let (c, i) = placed[0];
        return Ok(json!({ "action": "insert", "container": format!("{c:08x}"), "index": i,
                           "tag": format!("{:08x}", node.tag()), "name": short_name(node.tag()) }));
    }
    let container = parse_hex(op["container"].as_str().ok_or("operation has no \"container\"")?)?;
    let tag = parse_hex(op["tag"].as_str().ok_or("operation has no \"tag\"")?)?;
    let index = resolve_object(s, container, tag, op["index"].as_u64().map(|i| i as usize))?;
    let o = &s.entry(container).unwrap().objects()[index];
    let before = o.position();
    match action {
        "move" | "offset" => {
            let n = o.params_with_tag(tags::POSITION).len();
            if n != 1 {
                return Err(format!("object has {n} Position params; exactly one is required to move it"));
            }
            let old = before.ok_or("Position is not a 3-float vector")?;
            let new = if action == "move" {
                f3(op, "position")?
            } else {
                let d = f3(op, "delta")?;
                [old[0] + d[0], old[1] + d[1], old[2] + d[2]]
            };
            let dist = ((new[0] - old[0]).powi(2) + (new[1] - old[1]).powi(2) + (new[2] - old[2]).powi(2)).sqrt();
            if dist > MAX_MOVE && op["allow_far"] != true {
                return Err(format!("move of {dist:.1} m exceeds {MAX_MOVE} m; set \"allow_far\": true if intended"));
            }
            s.move_object(container, index, new)?;
            Ok(json!({ "action": action, "container": format!("{container:08x}"), "index": index, "tag": format!("{tag:08x}"),
                       "name": short_name(tag), "old_position": old, "new_position": new, "distance_m": dist }))
        }
        "set_param" => {
            let p = op["param"].as_str().ok_or("set_param needs \"param\" (name or hex tag)")?;
            let param_tag = if p.starts_with("0x") { parse_hex(p)? } else { pandemic_hash(p) };
            let occurrence = op["occurrence"].as_u64().unwrap_or(0) as usize;
            let new = unhex(op["value_hex"].as_str().ok_or("set_param needs \"value_hex\"")?)?;
            if param_tag == tags::POSITION {
                if let (Some(old), Some(n)) = (before, vec3(&new)) {
                    let dist = ((n[0] - old[0]).powi(2) + (n[1] - old[1]).powi(2) + (n[2] - old[2]).powi(2)).sqrt();
                    if !(n.iter().all(|x| x.is_finite())) || (dist > MAX_MOVE && op["allow_far"] != true) {
                        return Err(format!("Position change of {dist:.1} m refused (non-finite or over {MAX_MOVE} m without allow_far)"));
                    }
                }
            }
            s.set_param(container, index, param_tag, occurrence, new.clone())?;
            Ok(json!({ "action": action, "container": format!("{container:08x}"), "index": index, "tag": format!("{tag:08x}"),
                       "name": short_name(tag), "param": format!("{param_tag:08x}"), "param_name": editnodes::tag_name(param_tag),
                       "occurrence": occurrence, "new_hex": hex(&new) }))
        }
        "delete" => {
            s.remove_objects(&[(container, index)])?;
            Ok(json!({ "action": "delete", "container": format!("{container:08x}"), "index": index, "tag": format!("{tag:08x}"),
                       "name": short_name(tag) }))
        }
        "set_class" => {
            let class = op["class"].as_str().ok_or("set_class needs \"class\"")?;
            s.set_class_name(container, index, class)?;
            Ok(json!({ "action": "set_class", "container": format!("{container:08x}"), "index": index, "tag": format!("{tag:08x}"),
                       "name": short_name(tag), "class": class }))
        }
        "rotate" => {
            let axis = axis_of(op)?;
            let degrees = op["degrees"].as_f64().ok_or("rotate needs \"degrees\"")? as f32;
            if !degrees.is_finite() {
                return Err("rotation degrees are not finite".into());
            }
            let pivot = op.get("pivot").filter(|v| !v.is_null()).map(|_| f3(op, "pivot")).transpose()?;
            let m = axis_angle_matrix(axis, degrees)?;
            s.rotate_objects(&[(container, index)], m, pivot)?;
            Ok(json!({ "action": "rotate", "container": format!("{container:08x}"), "index": index, "tag": format!("{tag:08x}"),
                       "name": short_name(tag), "axis": axis, "degrees": degrees, "pivot": pivot }))
        }
        "scale" => {
            let factor = op["factor"].as_f64().ok_or("scale needs \"factor\"")? as f32;
            if !factor.is_finite() || factor <= 0.0 {
                return Err(format!("scale factor {factor} must be finite and > 0"));
            }
            s.scale_objects(&[(container, index)], factor)?;
            Ok(json!({ "action": "scale", "container": format!("{container:08x}"), "index": index, "tag": format!("{tag:08x}"),
                       "name": short_name(tag), "factor": factor }))
        }
        other => Err(format!("unknown action {other:?} (move, offset, set_param, delete, set_class, rotate, scale, insert)")),
    }
}

struct Snapshot {
    journal: Vec<Edit>,
    groups: Vec<u64>,
    group_names: std::collections::BTreeMap<u64, String>,
    cursor: usize,
    revision: u64,
}

fn snapshot(s: &Session) -> Snapshot {
    Snapshot { journal: s.journal.clone(), groups: s.groups.clone(), group_names: s.group_names.clone(), cursor: s.cursor, revision: s.revision }
}

fn restore(s: &mut Session, snap: Snapshot) -> Result<(), String> {
    s.journal = snap.journal;
    s.groups = snap.groups;
    s.group_names = snap.group_names;
    s.cursor = snap.cursor;
    s.revision = snap.revision;
    s.rebuild()
}

/// Run every operation, rolling all of them back on the first failure.
fn run_all(s: &mut Session, patch: &Value) -> Result<Vec<Value>, Value> {
    let ops = patch["operations"].as_array().ok_or_else(|| err("patch has no \"operations\" array", None, s))?;
    if ops.is_empty() {
        return Err(err("patch has no operations", None, s));
    }
    let snap = snapshot(s);
    let mut results = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        match run_op(s, op) {
            Ok(r) => results.push(r),
            Err(e) => {
                let rev = snap.revision;
                restore(s, snap).map_err(|re| err(format!("{e}; and rollback failed: {re}"), Some(i), s))?;
                return Err(json!({ "ok": false, "error": e, "op_index": i, "current_revision": rev, "rolled_back": true }));
            }
        }
    }
    Ok(results)
}

fn check_revision(s: &Session, patch: &Value) -> Result<(), Value> {
    match patch["expected_revision"].as_u64() {
        Some(want) if want != s.revision() => Err(json!({ "ok": false, "error": "stale plan: the session changed since expected_revision",
                                                          "expected_revision": want, "current_revision": s.revision() })),
        _ => Ok(()),
    }
}

/// Validate a patch without changing the session: run it, record the results, roll it back.
pub fn resolve_patch(s: &mut Session, patch: &Value) -> Result<Value, Value> {
    check_revision(s, patch)?;
    let snap = snapshot(s);
    let rev = snap.revision;
    let results = run_all(s, patch)?;
    restore(s, snap).map_err(|e| err(format!("rollback after resolve failed: {e}"), None, s))?;
    Ok(json!({ "ok": true, "resolved": true, "applied": false, "revision": rev, "results": results }))
}

/// Apply a patch atomically. A `patch_id` seen before returns its stored receipt with
/// `"replayed": true` and changes nothing.
pub fn apply_patch(s: &mut Session, patch: &Value) -> Result<Value, Value> {
    let id = patch["patch_id"].as_str().map(str::to_string);
    if let Some(id) = &id {
        if let Some((_, r)) = s.patches.iter().find(|(p, _)| p == id) {
            let mut r = r.clone();
            r["replayed"] = Value::Bool(true);
            return Ok(r);
        }
    }
    check_revision(s, patch)?;
    let before = s.revision();
    let first = s.cursor;
    let results = run_all(s, patch)?;
    // The whole patch is one undo step, named after it.
    if s.cursor > first {
        let g = s.groups[first];
        for k in first..s.cursor {
            let old = s.groups[k];
            if old != g {
                s.group_names.remove(&old);
            }
            s.groups[k] = g;
        }
        s.group_names.insert(g, format!("Agent patch {}", id.as_deref().unwrap_or("(no id)")));
    }
    s.revision = before + 1;
    let receipt = json!({ "ok": true, "applied": true, "patch_id": id, "revision_before": before, "revision_after": s.revision(),
                          "results": results, "active_edits": s.active().len() });
    if let Some(id) = id {
        s.patches.push((id, receipt.clone()));
    }
    Ok(receipt)
}

/// A summary of the session for an agent.
pub fn status(s: &Session) -> Value {
    let (j, c) = s.journal();
    let edits: Vec<Value> = j
        .iter()
        .enumerate()
        .map(|(k, e)| {
            let mut v = json!({ "active": k < c, "group": s.groups()[k], "step": s.group_name(s.groups()[k]),
                                "container": format!("{:08x}", e.container()), "index": e.object_index(),
                                "tag": format!("{:08x}", e.object_tag()), "name": short_name(e.object_tag()) });
            match e {
                Edit::Param(p) => {
                    v["kind"] = json!("param");
                    v["param"] = json!(editnodes::tag_name(p.param_tag));
                    v["occurrence"] = json!(p.occurrence);
                    v["old"] = vec3(&p.old).map(|x| json!(x)).unwrap_or(json!(hex(&p.old)));
                    v["new"] = vec3(&p.new).map(|x| json!(x)).unwrap_or(json!(hex(&p.new)));
                }
                Edit::Insert { .. } => v["kind"] = json!("insert"),
                Edit::Remove { .. } => v["kind"] = json!("remove"),
            }
            v
        })
        .collect();
    json!({ "game_dir": s.game.display().to_string(), "revision": s.revision(), "active_edits": c, "journal": j.len(),
            "can_undo": s.can_undo(), "can_redo": s.can_redo(), "edits": edits,
            "applied_patches": s.patches.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
            "terrain_cell_editing": "disabled: the patch-megapack route is unverified in game (M0 E2)" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage::EditNodesSources;
    use editnodes::{Entry, Pack, Root};
    use std::path::Path;

    fn obj(tag: u32, pos: [f32; 3]) -> Node {
        Node::Container {
            tag,
            children: vec![
                Node::Leaf { tag: tags::CLASS_NAME, data: b"Locator\0".to_vec() },
                Node::Leaf { tag: tags::POSITION, data: pos.iter().flat_map(|v| v.to_le_bytes()).collect() },
            ],
        }
    }

    fn session() -> Session {
        let base = Pack {
            entries: vec![Entry {
                hash: 0x10,
                root: Root::EditNode { objects: vec![obj(0xA, [1.0, 2.0, 3.0]), obj(0xB, [4.0, 5.0, 6.0]), obj(0xB, [7.0, 8.0, 9.0])] },
            }],
        };
        let dlc = Pack { entries: vec![] };
        Session::from_sources(Path::new("G"), EditNodesSources::from_bytes(base.write(), dlc.write(), "LF".into()).unwrap())
    }

    fn pos(s: &Session, i: usize) -> [f32; 3] {
        s.entry(0x10).unwrap().objects()[i].position().unwrap()
    }

    #[test]
    fn search_finds_by_class_and_inspect_lists_ordered_params() {
        let s = session();
        assert_eq!(search(&s, "locator", 10).len(), 3);
        assert_eq!(search(&s, "locator", 2).len(), 2);
        let v = inspect(&s, 0x10, 1).unwrap();
        assert_eq!(v["params"].as_array().unwrap().len(), 2);
        assert_eq!(v["params"][1]["name"], "Position");
    }

    #[test]
    fn resolve_changes_nothing_and_apply_is_atomic_and_idempotent() {
        let mut s = session();
        let p = json!({ "patch_id": "p1", "expected_revision": 0, "operations": [
            { "action": "offset", "container": "10", "tag": "a", "delta": [1, 0, 0] },
            { "action": "move", "container": "0x10", "tag": "b", "index": 2, "position": [70, 8, 9] } ] });
        let r = resolve_patch(&mut s, &p).unwrap();
        assert_eq!(r["results"].as_array().unwrap().len(), 2);
        assert_eq!((pos(&s, 0), s.revision(), s.active().len()), ([1.0, 2.0, 3.0], 0, 0), "resolve leaves no trace");

        let a = apply_patch(&mut s, &p).unwrap();
        assert_eq!((pos(&s, 0), pos(&s, 2), s.revision()), ([2.0, 2.0, 3.0], [70.0, 8.0, 9.0], 1));
        assert_eq!(a["revision_after"], 1);
        let again = apply_patch(&mut s, &p).unwrap();
        assert_eq!(again["replayed"], true);
        assert_eq!((pos(&s, 0), s.revision(), s.active().len()), ([2.0, 2.0, 3.0], 1, 2), "a replayed patch changes nothing");
    }

    #[test]
    fn a_failing_operation_rolls_the_whole_patch_back() {
        let mut s = session();
        let p = json!({ "operations": [
            { "action": "offset", "container": "10", "tag": "a", "delta": [1, 0, 0] },
            { "action": "move", "container": "10", "tag": "b", "position": [1, 1, 1] } ] });
        let e = apply_patch(&mut s, &p).unwrap_err();
        assert_eq!(e["op_index"], 1, "{e}");
        assert!(e["error"].as_str().unwrap().contains("give \"index\""), "a repeated tag needs an index");
        assert_eq!((pos(&s, 0), s.revision(), s.active().len()), ([1.0, 2.0, 3.0], 0, 0));
    }

    #[test]
    fn stale_revisions_far_moves_and_bad_numbers_are_refused() {
        let mut s = session();
        s.move_object(0x10, 0, [1.5, 2.0, 3.0]).unwrap();
        let stale = json!({ "expected_revision": 0, "operations": [{ "action": "offset", "container": "10", "tag": "a", "delta": [1, 0, 0] }] });
        assert_eq!(apply_patch(&mut s, &stale).unwrap_err()["current_revision"], 1);
        let far = json!({ "operations": [{ "action": "offset", "container": "10", "tag": "a", "delta": [5000, 0, 0] }] });
        assert!(apply_patch(&mut s, &far).is_err());
        let far_ok = json!({ "operations": [{ "action": "offset", "container": "10", "tag": "a", "delta": [5000, 0, 0], "allow_far": true }] });
        assert!(apply_patch(&mut s, &far_ok).is_ok());
        let nan = json!({ "operations": [{ "action": "move", "container": "10", "tag": "a", "position": [1, "x", 3] }] });
        assert!(apply_patch(&mut s, &nan).is_err());
    }

    #[test]
    fn receipts_and_revision_survive_save_and_reopen() {
        let mut s = session();
        apply_patch(&mut s, &json!({ "patch_id": "keep", "operations": [{ "action": "offset", "container": "10", "tag": "a", "delta": [0, 1, 0] }] })).unwrap();
        let v = s.to_project_json();
        let base = Pack {
            entries: vec![Entry {
                hash: 0x10,
                root: Root::EditNode { objects: vec![obj(0xA, [1.0, 2.0, 3.0]), obj(0xB, [4.0, 5.0, 6.0]), obj(0xB, [7.0, 8.0, 9.0])] },
            }],
        };
        let src = EditNodesSources::from_bytes(base.write(), Pack { entries: vec![] }.write(), "LF".into()).unwrap();
        let mut r = Session::load_with(src, Path::new("G"), &v).unwrap();
        assert_eq!(r.revision(), 1);
        let again = apply_patch(&mut r, &json!({ "patch_id": "keep", "operations": [] })).unwrap();
        assert_eq!(again["replayed"], true);
    }
}
