//! Stage EditNodes parameter edits as a DLC-slot override pack.
//!
//! The engine searches the DLC slot's `EditNodes.pack` before the base pack, per entry hash, and a
//! shadowed entry is replaced wholesale (`docs/formats/editnodes.md`). The staged pack therefore holds
//! every original DLC/01 entry unchanged, plus the winning copy of each touched container with the
//! edits applied. This route was observed in game for a `Position` value edit
//! (`docs/level_editor/m0_log.md` §5d); added objects, renames and resized containers were not.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sab_formats::{cellplace, editnodes};
use serde_json::json;

use crate::stage::{self, Dest, EditNodesSources, DLC_EDITNODES, LOOSEFILES};

/// One parameter edit, addressed the way the data is laid out: the container (directory hash), the
/// object's index in that container plus its tag (checked), the parameter tag and its 0-based
/// occurrence among the object's leaves with that tag, and the expected old bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamEdit {
    pub container: u32,
    pub object_index: usize,
    pub object_tag: u32,
    pub param_tag: u32,
    pub occurrence: usize,
    pub old: Vec<u8>,
    pub new: Vec<u8>,
}

/// One journal edit: a parameter edit, or an object inserted into or removed from a container.
/// Structural edits (insert, remove) and size-changing parameter edits are statically checked only;
/// the in-game route was observed for same-size `Position` edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    Param(ParamEdit),
    /// Insert `node` so it becomes object `index` of `container`.
    Insert { container: u32, index: usize, node: editnodes::Node },
    /// Remove object `index` of `container`, which must currently equal `node`.
    Remove { container: u32, index: usize, node: editnodes::Node },
}

impl Edit {
    pub fn container(&self) -> u32 {
        match self {
            Edit::Param(p) => p.container,
            Edit::Insert { container, .. } | Edit::Remove { container, .. } => *container,
        }
    }

    pub fn object_index(&self) -> usize {
        match self {
            Edit::Param(p) => p.object_index,
            Edit::Insert { index, .. } | Edit::Remove { index, .. } => *index,
        }
    }

    pub fn object_tag(&self) -> u32 {
        match self {
            Edit::Param(p) => p.object_tag,
            Edit::Insert { node, .. } | Edit::Remove { node, .. } => node.tag(),
        }
    }

    /// A same-size parameter edit.
    pub fn is_same_size_param(&self) -> bool {
        matches!(self, Edit::Param(p) if p.old.len() == p.new.len())
    }

    /// Whether this edit is of a *kind* the game was shown to load, on always-loaded locators
    /// through the DLC/01 override (`docs/level_editor/m0_log.md` §5d, §5f):
    /// - a same-size Position edit (E1);
    /// - a yaw-only basis edit: an XAxis or ZAxis value staying in the horizontal plane, Y unchanged
    ///   (Gate A, one 90° yaw);
    /// - an inserted copy or a removed object (Gate A).
    ///
    /// This says the operation was demonstrated, not that this particular edit is verified: other
    /// object classes, containers and values were not tested. Any YAxis edit (a tilt), LuaParam
    /// edits, other leaves and resizes were not demonstrated.
    pub fn kind_demonstrated_on_locators(&self) -> bool {
        use sab_formats::editnodes::tags;
        let horizontal = |b: &[u8]| b.len() == 12 && f32::from_le_bytes([b[4], b[5], b[6], b[7]]) == 0.0;
        let unit = |b: &[u8]| {
            b.len() == 12
                && {
                    let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
                    let (x, y, z) = (f(0), f(4), f(8));
                    ((x * x + y * y + z * z).sqrt() - 1.0).abs() < 1e-3
                }
        };
        match self {
            Edit::Param(p) if p.old.len() != p.new.len() => false,
            Edit::Param(p) if p.param_tag == tags::POSITION => true,
            Edit::Param(p) if p.param_tag == tags::X_AXIS || p.param_tag == tags::Z_AXIS => horizontal(&p.old) && horizontal(&p.new) && unit(&p.old) && unit(&p.new),
            Edit::Param(_) => false,
            Edit::Insert { .. } | Edit::Remove { .. } => true,
        }
    }
}

/// Apply one journal edit to a container entry.
pub fn apply_edit(entry: &mut editnodes::Entry, e: &Edit) -> Result<(), String> {
    match e {
        Edit::Param(p) => apply(entry, p),
        Edit::Insert { container, index, node } => {
            let objects = entry.objects_mut().ok_or("container root is not an EditNode")?;
            if *index > objects.len() {
                return Err(format!("container {container:08x}: insert at {index} past the end ({})", objects.len()));
            }
            objects.insert(*index, node.clone());
            Ok(())
        }
        Edit::Remove { container, index, node } => {
            let objects = entry.objects_mut().ok_or("container root is not an EditNode")?;
            match objects.get(*index) {
                Some(o) if o == node => {
                    objects.remove(*index);
                    Ok(())
                }
                Some(_) => Err(format!("container {container:08x} object {index}: differs from the object expected for removal")),
                None => Err(format!("container {container:08x}: no object {index} to remove")),
            }
        }
    }
}

/// Independent check of a container rebuilt with structural or size-changing edits: replay the edits
/// on a list of slots (original object + leaf overrides, or an inserted object) without using
/// [`apply_edit`], then require the rebuilt objects to match slot for slot. Original objects not
/// edited must be unchanged; edited ones may differ only in their edited leaves.
pub fn verify_structure(src: &[editnodes::Node], out: &[editnodes::Node], edits: &[&Edit]) -> Result<(bool, String), String> {
    enum Slot {
        Orig(usize, Vec<(u32, usize, Vec<u8>)>),
        New(editnodes::Node),
    }
    let mut slots: Vec<Slot> = (0..src.len()).map(|i| Slot::Orig(i, Vec::new())).collect();
    for e in edits {
        match e {
            Edit::Param(p) => match slots.get_mut(p.object_index) {
                Some(Slot::Orig(_, ov)) => {
                    ov.retain(|(t, o, _)| !(*t == p.param_tag && *o == p.occurrence));
                    ov.push((p.param_tag, p.occurrence, p.new.clone()));
                }
                Some(Slot::New(n)) => n.set_param_at(p.param_tag, p.occurrence, &p.old, p.new.clone())?,
                None => return Err(format!("param edit addresses object {} past the end", p.object_index)),
            },
            Edit::Insert { index, node, .. } => {
                if *index > slots.len() {
                    return Err(format!("insert at {index} past the end"));
                }
                slots.insert(*index, Slot::New(node.clone()));
            }
            Edit::Remove { index, .. } => {
                if *index >= slots.len() {
                    return Err(format!("remove at {index} past the end"));
                }
                slots.remove(*index);
            }
        }
    }
    if slots.len() != out.len() {
        return Ok((false, format!("{} objects expected, {} rebuilt", slots.len(), out.len())));
    }
    let (mut unchanged, mut edited, mut inserted, mut ok) = (0, 0, 0, true);
    for (s, o) in slots.iter().zip(out) {
        match s {
            Slot::Orig(i, ov) if ov.is_empty() => {
                unchanged += 1;
                ok &= src[*i] == *o;
            }
            Slot::Orig(i, ov) => {
                edited += 1;
                let mut want = src[*i].clone();
                if let editnodes::Node::Container { children, .. } = &mut want {
                    for (tag, occ, bytes) in ov {
                        let leaf = children.iter_mut().filter(|c| matches!(c, editnodes::Node::Leaf { tag: t, .. } if t == tag)).nth(*occ);
                        match leaf {
                            Some(editnodes::Node::Leaf { data, .. }) => *data = bytes.clone(),
                            _ => return Err(format!("object {i}: no occurrence {occ} of {tag:08x}")),
                        }
                    }
                }
                ok &= want == *o;
            }
            Slot::New(n) => {
                inserted += 1;
                ok &= n == o;
            }
        }
    }
    let mut tags: Vec<u32> = out.iter().map(|o| o.tag()).collect();
    let before = tags.len();
    tags.sort_unstable();
    tags.dedup();
    let src_dupes = {
        let mut t: Vec<u32> = src.iter().map(|o| o.tag()).collect();
        let n = t.len();
        t.sort_unstable();
        t.dedup();
        n - t.len()
    };
    let new_dupes = (before - tags.len()).saturating_sub(src_dupes);
    ok &= new_dupes == 0;
    Ok((ok, format!("{unchanged} unchanged, {edited} edited, {inserted} inserted; slot-by-slot match and no new duplicate tags: {ok}")))
}

/// Apply one parameter edit to a container entry: the object at `object_index` must carry
/// `object_tag`, and the addressed leaf must currently hold `old`.
pub fn apply(entry: &mut editnodes::Entry, e: &ParamEdit) -> Result<(), String> {
    let objects = entry.objects_mut().ok_or("container root is not an EditNode")?;
    let o = objects
        .get_mut(e.object_index)
        .ok_or_else(|| format!("container {:08x}: no object {}", e.container, e.object_index))?;
    if o.tag() != e.object_tag {
        return Err(format!(
            "container {:08x} object {}: tag {:08x}, expected {:08x}",
            e.container,
            e.object_index,
            o.tag(),
            e.object_tag
        ));
    }
    o.set_param_at(e.param_tag, e.occurrence, &e.old, e.new.clone())
}

pub struct StageOutcome {
    pub out_dir: PathBuf,
    pub report: String,
    pub all_pass: bool,
    pub artifact_sha: String,
}

/// The payload range (body-relative) of the addressed leaf, from the source body's byte layout.
fn leaf_range(body: &[u8], e: &ParamEdit) -> Result<(usize, usize), String> {
    let locs = cellplace::index(body)?;
    let obj = locs.get(e.object_index).ok_or("object index out of range in the source body")?;
    let p = obj
        .params
        .iter()
        .filter(|p| p.tag == e.param_tag)
        .nth(e.occurrence)
        .ok_or("addressed leaf not found in the source body")?;
    Ok((p.data_off, p.len))
}

/// Stage `edits` (applied in order) as `out/EditNodes.pack` with `manifest.json`, `install.ps1`,
/// `restore.ps1` and `report.txt`. `label` names the package in the scripts and backups.
pub fn stage_editnodes(
    game: &Path,
    src: &EditNodesSources,
    edits: &[Edit],
    out: &Path,
    label: &str,
    extra: serde_json::Value,
) -> Result<StageOutcome, String> {
    if edits.is_empty() {
        return Err("nothing to stage: no edits".into());
    }
    stage::guard_write(out, game)?;
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut report = String::new();

    let mut touched: Vec<u32> = Vec::new();
    for e in edits {
        if !touched.contains(&e.container()) {
            touched.push(e.container());
        }
    }
    let mut dlc_new = editnodes::Pack::parse(&src.dlc_bytes)?;
    let mut from_base = 0usize;
    for &c in &touched {
        let (from, _) = src.winner(c).ok_or_else(|| format!("container {c:08x} is in neither pack"))?;
        let _ = writeln!(report, "container {c:08x} ({}) from {from}", editnodes::tag_name(c).unwrap_or("?"));
        if src.dlc.entry(c).is_none() {
            let mut extra_pack = src.base.override_pack(&[c])?;
            dlc_new.entries.push(extra_pack.entries.remove(0));
            from_base += 1;
        }
    }
    let noop = dlc_new.write();
    for e in edits {
        let entry = dlc_new.entry_mut(e.container()).expect("touched containers are in the staged pack");
        apply_edit(entry, e)?;
        let name = editnodes::tag_name(e.object_tag()).unwrap_or("?");
        let _ = match e {
            Edit::Param(p) => writeln!(
                report,
                "edit: object {} {:08x} ({name}) param {:08x} ({}) #{}: {} -> {} bytes",
                p.object_index,
                p.object_tag,
                p.param_tag,
                editnodes::tag_name(p.param_tag).unwrap_or("?"),
                p.occurrence,
                p.old.len(),
                p.new.len()
            ),
            Edit::Insert { index, node, .. } => writeln!(report, "insert: object {index} {:08x} ({name}), {} params", node.tag(), node.params().len()),
            Edit::Remove { index, node, .. } => writeln!(report, "remove: object {index} {:08x} ({name})", node.tag()),
        };
    }
    let edited = dlc_new.write();

    // (a) codec no-op reconstruction preserves the selected inputs.
    let dlc_rt = editnodes::Pack::parse(&src.dlc_bytes)?.write() == src.dlc_bytes;
    let noop_dir = editnodes::directory(&noop)?;
    let ed_dir = editnodes::directory(&edited)?;
    let body = |bytes: &[u8], dir: &[(u32, std::ops::Range<usize>)], c: u32| -> Vec<u8> {
        dir.iter().find(|(h, _)| *h == c).map(|(_, r)| bytes[r.clone()].to_vec()).unwrap_or_default()
    };
    let mut bodies_rt = true;
    let mut b_ok = true;
    let mut b_notes = Vec::new();
    for &c in &touched {
        let src_bytes = if src.dlc.entry(c).is_some() { &src.dlc_bytes } else { &src.base_bytes };
        let src_body = body(src_bytes, &editnodes::directory(src_bytes)?, c);
        bodies_rt &= body(&noop, &noop_dir, c) == src_body;
        // (b) same-size parameter edits: the edited body differs only inside the addressed leaves.
        // Structural or size-changing edits: an independent slot-by-slot object comparison.
        let ed_body = body(&edited, &ed_dir, c);
        let mine: Vec<&Edit> = edits.iter().filter(|e| e.container() == c).collect();
        if mine.iter().all(|e| e.is_same_size_param()) {
            let ranges: Vec<(usize, usize)> = mine
                .iter()
                .filter_map(|e| match e {
                    Edit::Param(p) => Some(leaf_range(&src_body, p)),
                    _ => None,
                })
                .collect::<Result<_, _>>()?;
            let diffs = stage::diff_positions(&src_body, &ed_body);
            let ok = ed_body.len() == src_body.len() && stage::ranges_cover(&diffs, &ranges);
            b_notes.push(format!("{c:08x}: same length {}, {} differing bytes, all inside the edited leaves: {ok}", ed_body.len() == src_body.len(), diffs.len()));
            b_ok &= ok;
        } else {
            let src_objs = editnodes::parse_entry(&src_body)?;
            let out_objs = editnodes::parse_entry(&ed_body)?;
            let (ok, note) = verify_structure(&src_objs, &out_objs, &mine)?;
            b_notes.push(format!("{c:08x}: structural or size-changing edits (not observed in game): {note}"));
            b_ok &= ok;
        }
    }
    let _ = writeln!(report, "(a) DLC/01 pack re-serializes byte-identically: {dlc_rt}; touched container bodies unchanged by a no-op rebuild: {bodies_rt}");
    let _ = writeln!(report, "(b) {}", b_notes.join("; "));
    // (c) container: expected directory, every untouched DLC/01 entry preserved, final values present.
    let orig_dir = editnodes::directory(&src.dlc_bytes)?;
    let mut c_ok = ed_dir.len() == orig_dir.len() + from_base;
    for (h, r) in &orig_dir {
        if touched.contains(h) {
            continue;
        }
        let new_r = ed_dir.iter().find(|(x, _)| x == h).map(|(_, r)| r.clone());
        c_ok &= new_r.map_or(false, |nr| edited[nr] == src.dlc_bytes[r.clone()]);
    }
    let reparsed = editnodes::Pack::parse(&edited)?;
    // Final leaf values, for containers without structural edits (indices there are stable; the
    // structural ones were compared object by object in (b)).
    let structural: Vec<u32> = edits.iter().filter(|e| !matches!(e, Edit::Param(_))).map(|e| e.container()).collect();
    let mut last: BTreeMap<(u32, usize, u32, usize), &[u8]> = BTreeMap::new();
    for e in edits {
        if let Edit::Param(p) = e {
            if !structural.contains(&p.container) {
                last.insert((p.container, p.object_index, p.param_tag, p.occurrence), &p.new);
            }
        }
    }
    for ((c, oi, pt, occ), want) in &last {
        let got = reparsed.entry(*c).and_then(|en| en.objects().get(*oi)).and_then(|o| {
            o.params_with_tag(*pt).get(*occ).and_then(|&i| match &o.params()[i] {
                editnodes::Node::Leaf { data, .. } => Some(data.clone()),
                editnodes::Node::Container { .. } => None,
            })
        });
        c_ok &= got.as_deref() == Some(*want);
    }
    let _ = writeln!(report, "(c) directory {} -> {} entries; every untouched DLC/01 entry body preserved and every edited leaf re-parses to its new bytes: {c_ok}", orig_dir.len(), ed_dir.len());
    let _ = writeln!(report, "(d) sab_validator: not applicable (it validates megapacks, not EditNodes packs)");
    let all = dlc_rt && bodies_rt && b_ok && c_ok;

    let staged = "EditNodes.pack";
    stage::write(out, staged, &edited)?;
    let dest = Dest {
        rel: DLC_EDITNODES.into(),
        existed: true,
        original_sha: Some(src.dlc_sha()),
        staged_name: staged.into(),
        installed_sha: stage::sha256(&edited),
    };
    let _ = writeln!(report, "(e) sha256: source DLC/01 pack {}; staged {staged} {}; loosefiles_BinPC.pack {}", src.dlc_sha(), dest.installed_sha, src.loosefiles_sha);
    let _ = writeln!(report, "ALL STATIC CHECKS PASS: {all}");
    let edits_json: Vec<_> = edits.iter().map(edit_json).collect();
    let prov = json!({ "edits": edits_json, "loosefiles_sha256": src.loosefiles_sha, "dlc_editnodes_sha256": src.dlc_sha(),
                       "static_checks_pass": all, "extra": extra });
    let artifact_sha = dest.installed_sha.clone();
    let pre = [(LOOSEFILES.to_string(), src.loosefiles_sha.clone())];
    stage::write_scripts(out, game, label, &[dest], &pre, prov)?;
    stage::write(out, "report.txt", report.as_bytes())?;
    Ok(StageOutcome { out_dir: out.to_path_buf(), report, all_pass: all, artifact_sha })
}

/// A node as JSON: `{tag, data}` for a leaf (hex bytes), `{tag, children}` for a container.
pub fn node_json(n: &editnodes::Node) -> serde_json::Value {
    match n {
        editnodes::Node::Leaf { tag, data } => json!({ "tag": format!("{tag:08x}"), "data": hex(data) }),
        editnodes::Node::Container { tag, children } => json!({ "tag": format!("{tag:08x}"), "children": children.iter().map(node_json).collect::<Vec<_>>() }),
    }
}

pub fn node_from_json(v: &serde_json::Value) -> Result<editnodes::Node, String> {
    let tag = u32::from_str_radix(v["tag"].as_str().ok_or("node without tag")?, 16).map_err(|e| format!("node tag: {e}"))?;
    if let Some(d) = v["data"].as_str() {
        return Ok(editnodes::Node::Leaf { tag, data: unhex(d)? });
    }
    let children = v["children"].as_array().ok_or("node has neither data nor children")?.iter().map(node_from_json).collect::<Result<_, _>>()?;
    Ok(editnodes::Node::Container { tag, children })
}

/// A journal edit as JSON (project files and manifests). Parameter edits keep their original
/// shape, without a `kind` field, so earlier project files still read.
pub fn edit_json(e: &Edit) -> serde_json::Value {
    match e {
        Edit::Param(p) => json!({ "container": format!("{:08x}", p.container), "object_index": p.object_index,
                                 "object_tag": format!("{:08x}", p.object_tag), "param_tag": format!("{:08x}", p.param_tag),
                                 "param_name": editnodes::tag_name(p.param_tag), "occurrence": p.occurrence,
                                 "old": hex(&p.old), "new": hex(&p.new) }),
        Edit::Insert { container, index, node } => json!({ "kind": "insert", "container": format!("{container:08x}"), "object_index": index, "node": node_json(node) }),
        Edit::Remove { container, index, node } => json!({ "kind": "remove", "container": format!("{container:08x}"), "object_index": index, "node": node_json(node) }),
    }
}

pub fn edit_from_json(x: &serde_json::Value) -> Result<Edit, String> {
    let h = |k: &str| -> Result<u32, String> { u32::from_str_radix(x[k].as_str().ok_or_else(|| format!("edit field {k} missing"))?, 16).map_err(|e| format!("{k}: {e}")) };
    let index = x["object_index"].as_u64().ok_or("object_index")? as usize;
    match x["kind"].as_str() {
        None | Some("param") => Ok(Edit::Param(ParamEdit {
            container: h("container")?,
            object_index: index,
            object_tag: h("object_tag")?,
            param_tag: h("param_tag")?,
            occurrence: x["occurrence"].as_u64().ok_or("occurrence")? as usize,
            old: unhex(x["old"].as_str().ok_or("old")?)?,
            new: unhex(x["new"].as_str().ok_or("new")?)?,
        })),
        Some("insert") => Ok(Edit::Insert { container: h("container")?, index, node: node_from_json(&x["node"])? }),
        Some("remove") => Ok(Edit::Remove { container: h("container")?, index, node: node_from_json(&x["node"])? }),
        Some(k) => Err(format!("unknown edit kind {k:?}")),
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("odd-length hex {s:?}"));
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("hex {s:?}: {e}"))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sab_formats::editnodes::{tags, Node};

    fn obj(tag: u32, x: f32) -> Node {
        Node::Container { tag, children: vec![Node::Leaf { tag: tags::POSITION, data: [x, 0.0, 0.0].iter().flat_map(|v| v.to_le_bytes()).collect() }] }
    }

    /// The structural check accepts exactly the expected rebuild and rejects a changed bystander, a
    /// missing insert and a duplicated tag.
    #[test]
    fn verify_structure_rejects_anything_but_the_expected_rebuild() {
        let src = vec![obj(1, 1.0), obj(2, 2.0)];
        let ins = Edit::Insert { container: 9, index: 1, node: obj(3, 3.0) };
        let rem = Edit::Remove { container: 9, index: 0, node: obj(1, 1.0) };
        let edits = [&ins, &rem];
        assert!(verify_structure(&src, &[obj(3, 3.0), obj(2, 2.0)], &edits).unwrap().0);
        assert!(!verify_structure(&src, &[obj(3, 3.0), obj(2, 2.5)], &edits).unwrap().0, "a changed bystander");
        assert!(!verify_structure(&src, &[obj(2, 2.0)], &edits).unwrap().0, "a missing insert");
        let dup = Edit::Insert { container: 9, index: 0, node: obj(2, 9.0) };
        assert!(!verify_structure(&src, &[obj(2, 9.0), obj(1, 1.0), obj(2, 2.0)], &[&dup]).unwrap().0, "a new duplicate tag");
        // Only the kinds Gate A and E1 demonstrated count: a yaw (X/Z staying horizontal), a
        // same-size Position edit, an insert or a removal. Tilts, LuaParams and resizes do not.
        let v = |x: f32, y: f32, z: f32| [x, y, z].iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
        let param = |tag: u32, old: Vec<u8>, new: Vec<u8>| Edit::Param(ParamEdit { container: 9, object_index: 0, object_tag: 1, param_tag: tag, occurrence: 0, old, new });
        assert!(param(tags::X_AXIS, v(1.0, 0.0, 0.0), v(0.0, 0.0, -1.0)).kind_demonstrated_on_locators(), "a yaw");
        assert!(!param(tags::X_AXIS, v(1.0, 0.0, 0.0), v(0.7, 0.7, 0.0)).kind_demonstrated_on_locators(), "a tilt of X");
        assert!(!param(tags::Y_AXIS, v(0.0, 1.0, 0.0), v(0.0, 0.7, 0.7)).kind_demonstrated_on_locators(), "any Y change is a tilt");
        assert!(param(tags::POSITION, v(1.0, 2.0, 3.0), v(4.0, 2.0, 3.0)).kind_demonstrated_on_locators());
        assert!(!param(tags::LUA_PARAM, b"a\0".to_vec(), b"b\0".to_vec()).kind_demonstrated_on_locators(), "a LuaParam");
        assert!(!param(tags::POSITION, v(1.0, 2.0, 3.0), vec![0; 8]).kind_demonstrated_on_locators(), "a resize");
        assert!(ins.kind_demonstrated_on_locators() && rem.kind_demonstrated_on_locators());
        // A JSON round trip keeps every kind of edit.
        for e in [&ins, &rem] {
            assert_eq!(&edit_from_json(&edit_json(e)).unwrap(), e);
        }
    }
}
