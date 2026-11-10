//! Operation Quiet Village: scripted terrain-cell authoring through the ordinary placement `Session`
//! (the same journal, undo/redo, project file and export path Workshop uses), plus an export that can
//! carry a separately built and verified baked-collision payload in the same sub-pack.
//!
//! Spec (`author`): `{"cell": N, "ops": [ ... ]}`, applied in order, each op one undo step:
//! * `{"op":"move","tag":"hex","pos":[x,y,z],"yaw_deg":d?}` absolute position (and absolute yaw if given)
//! * `{"op":"copy","tag":"hex","pos":[..],"yaw_deg":d?,"name":"..."?}` duplicate an object of this cell
//! * `{"op":"insert_from","cell":M,"tag":"hex","pos":[..],"yaw_deg":d?,"name":".."?}` copy an object from
//!   another base cell into this cell's base-layer list (new tag; the object bytes are otherwise unchanged)
//! * `{"op":"delete","tag":"hex"}`, `{"op":"undo"}`, `{"op":"redo"}`
//! New tags are minted unique against every base-cell tag and are reported with the op's `name`.
use std::collections::HashSet;
use std::path::Path;

use sab_formats::editnodes::{tags, Node};
use serde_json::{json, Value};

use crate::cells::{france_mounts, scan_cells};
use crate::cellsession::{self, CellEdit};
use crate::session::Session;

fn hex(v: &Value, k: &str) -> Result<u32, String> {
    let s = v[k].as_str().ok_or_else(|| format!("op needs \"{k}\""))?;
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("{k}: {e}"))
}

fn vec3(v: &Value, k: &str) -> Result<[f32; 3], String> {
    let a = v[k].as_array().filter(|a| a.len() == 3).ok_or_else(|| format!("op needs \"{k}\": [x,y,z]"))?;
    let mut o = [0f32; 3];
    for i in 0..3 {
        o[i] = a[i].as_f64().ok_or("non-numeric coordinate")? as f32;
    }
    Ok(o)
}

fn yaw_axes(deg: f64) -> [[f32; 3]; 3] {
    let (s, c) = (deg.to_radians().sin() as f32, deg.to_radians().cos() as f32);
    [[c, 0.0, -s], [0.0, 1.0, 0.0], [s, 0.0, c]]
}

fn b3(v: [f32; 3]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn locate(edit: &CellEdit, s: &Session, tag: u32) -> Result<(u32, usize), String> {
    for c in &edit.containers {
        if let Some(e) = s.entry(*c) {
            let hits: Vec<usize> = e.objects().iter().enumerate().filter(|(_, o)| o.tag() == tag).map(|(i, _)| i).collect();
            match hits.len() {
                0 => continue,
                1 => return Ok((*c, hits[0])),
                _ => return Err(format!("tag {tag:08x} occurs {} times in one list", hits.len())),
            }
        }
    }
    Err(format!("tag {tag:08x} is not in cell {}", edit.cell))
}

/// The base-layer (f14 = 0) list with the most objects: where cross-cell inserts go.
fn base_list(edit: &CellEdit, s: &Session) -> Result<u32, String> {
    (0..edit.containers.len())
        .filter(|&li| edit.list_layer(li) == Some(0))
        .max_by_key(|&li| s.entry(edit.containers[li]).map_or(0, |e| e.objects().len()))
        .map(|li| edit.containers[li])
        .ok_or_else(|| "the cell has no base-layer placement list".into())
}

fn set_pose(s: &mut Session, c: u32, i: usize, node: &Node, pos: [f32; 3], yaw: Option<f64>) -> Result<(), String> {
    if node.param(tags::POSITION).is_none() {
        return Err(format!("object {:08x} has no Position", node.tag()));
    }
    s.set_param(c, i, tags::POSITION, 0, b3(pos))?;
    if let Some(d) = yaw {
        let ax = yaw_axes(d);
        for (t, a) in [(tags::X_AXIS, ax[0]), (tags::Y_AXIS, ax[1]), (tags::Z_AXIS, ax[2])] {
            if node.param(t).is_none() {
                return Err(format!("object {:08x} has no rotation basis", node.tag()));
            }
            s.set_param(c, i, t, 0, b3(a))?;
        }
    }
    Ok(())
}

fn posed(mut n: Node, tag: u32, pos: [f32; 3], yaw: Option<f64>) -> Result<Node, String> {
    n.set_tag(tag);
    if !n.set_position(pos)? {
        return Err("object has no Position".into());
    }
    if let Some(d) = yaw {
        let ax = yaw_axes(d);
        for (t, a) in [(tags::X_AXIS, ax[0]), (tags::Y_AXIS, ax[1]), (tags::Z_AXIS, ax[2])] {
            if !n.set_param(t, b3(a))? {
                return Err("object has no rotation basis".into());
            }
        }
    }
    Ok(n)
}

/// Apply `spec` to a fresh session of its cell and save the project. Returns a report.
pub fn author(game: &Path, spec: &Value, project: &Path) -> Result<Value, String> {
    let cell = spec["cell"].as_u64().ok_or("spec needs \"cell\"")? as u32;
    let (edit, mut s) = cellsession::open_cell(game, cell)?;
    let mounts = france_mounts(game)?;
    let (_, counts) = scan_cells(&mounts)?;
    let mut minted: Vec<u32> = Vec::new();
    let mut fresh = |seed: &str, used: &mut Vec<u32>| -> u32 {
        for n in 0u32.. {
            let t = sab_formats::pandemic_hash(&format!("quiet_village\\{seed}\\{n}"));
            if t != 0 && !counts.contains_key(&t) && !used.contains(&t) {
                used.push(t);
                return t;
            }
        }
        unreachable!()
    };
    let mut donors: std::collections::HashMap<u32, (CellEdit, Session)> = Default::default();
    let mut log = Vec::new();
    for (k, op) in spec["ops"].as_array().ok_or("spec needs \"ops\"")?.iter().enumerate() {
        let kind = op["op"].as_str().ok_or("op needs \"op\"")?;
        let name = op["name"].as_str().unwrap_or("").to_string();
        let yaw = op["yaw_deg"].as_f64();
        let entry = match kind {
            "move" => {
                let tag = hex(op, "tag")?;
                let (c, i) = locate(&edit, &s, tag)?;
                let node = s.entry(c).unwrap().objects()[i].clone();
                set_pose(&mut s, c, i, &node, vec3(op, "pos")?, yaw)?;
                json!({"op": k, "kind": kind, "tag": format!("{tag:08x}"), "class": node.class_name()})
            }
            "copy" | "insert_from" => {
                let src_tag = hex(op, "tag")?;
                let (node, container) = if kind == "copy" {
                    let (c, i) = locate(&edit, &s, src_tag)?;
                    (s.entry(c).unwrap().objects()[i].clone(), c)
                } else {
                    let dc = op["cell"].as_u64().ok_or("insert_from needs \"cell\"")? as u32;
                    if !donors.contains_key(&dc) {
                        donors.insert(dc, cellsession::open_cell(game, dc)?);
                    }
                    let (de, ds) = &donors[&dc];
                    let (c, i) = locate(de, ds, src_tag)?;
                    (ds.entry(c).unwrap().objects()[i].clone(), base_list(&edit, &s)?)
                };
                let new_tag = fresh(&format!("{k}\\{src_tag:08x}\\{name}"), &mut minted);
                let n = posed(node, new_tag, vec3(op, "pos")?, yaw)?;
                let class = n.class_name();
                let idx = s.entry(container).unwrap().objects().len();
                s.insert_objects(vec![(container, idx, n)], &format!("qv {kind} {name}"))?;
                if !name.is_empty() {
                    s.note_name(new_tag, &name);
                }
                json!({"op": k, "kind": kind, "source_tag": format!("{src_tag:08x}"), "new_tag": format!("{new_tag:08x}"), "class": class, "name": name})
            }
            "delete" => {
                let tag = hex(op, "tag")?;
                let (c, i) = locate(&edit, &s, tag)?;
                let class = s.entry(c).unwrap().objects()[i].class_name();
                s.remove_objects(&[(c, i)])?;
                json!({"op": k, "kind": kind, "tag": format!("{tag:08x}"), "class": class})
            }
            "undo" => json!({"op": k, "kind": kind, "did": s.undo()?}),
            "redo" => json!({"op": k, "kind": kind, "did": s.redo()?}),
            other => return Err(format!("unknown op {other}")),
        };
        log.push(entry);
    }
    cellsession::save_cell_project(&edit, &s, project)?;
    let (journal, cursor) = s.journal();
    Ok(json!({"cell": cell, "project": project.display().to_string(), "journal_edits": journal.len(), "undo_cursor": cursor, "revision": s.revision(), "ops": log}))
}

/// Reopen a saved project (fresh session, as Workshop's Open project does) and stage it, optionally
/// replacing type-2 records with verified payloads. Nothing is installed.
pub fn export(project: &Path, records: &[(usize, Vec<u8>)], out: &Path) -> Result<Value, String> {
    let (edit, s) = cellsession::open_cell_project(project)?;
    let game = s.game.clone();
    let mut objects = Vec::new();
    let mut seen = HashSet::new();
    for c in &edit.containers {
        for o in s.entry(*c).map(|e| e.objects().to_vec()).unwrap_or_default() {
            if seen.insert(o.tag()) {
                objects.push(json!({"tag": format!("{:08x}", o.tag()), "class": o.class_name(), "pos": o.position()}));
            }
        }
    }
    let outcome = cellsession::stage_cell_with_records(&game, &edit, &s, out, records)?;
    let (journal, cursor) = s.journal();
    Ok(json!({"cell": edit.cell, "journal_edits": journal.len(), "undo_cursor": cursor, "all_pass": outcome.all_pass,
              "artifact_sha256": outcome.artifact_sha, "slot": outcome.slot_name, "out": out.display().to_string(), "objects_after": objects.len()}))
}
