//! A placement-editing session over the effective EditNodes data (DLC slot first, then base).
//!
//! The retail packs stay untouched in memory. Edits form a journal of [`Edit`]s (parameter edits,
//! object inserts and removals) in named undo groups with an undo cursor; the current state of a
//! touched container is rebuilt by replaying the active edits onto a fresh copy of its winning
//! entry, so undo never depends on reversing a mutation. Every edit carries what it expects to
//! replace (old bytes, or the whole object for a removal), and replay refuses on any mismatch. A
//! project file stores the journal plus the hashes of the inputs it was made against; reopening
//! refuses changed inputs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sab_formats::editnodes::{self, tags, Entry, Node};
use serde_json::{json, Value};

use crate::editnodes_stage::{self, Edit, ParamEdit, StageOutcome};
use crate::stage::EditNodesSources;

/// The project file format tag and version.
pub const PROJECT_FORMAT: &str = "sab_placement_project";
/// v2 adds `revision` and applied-patch receipts; v1 files load with revision 0 and no receipts.
pub const PROJECT_VERSION: u64 = 2;

/// How many rotating backups a project save keeps.
pub const BACKUPS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// One validation finding; `at` is the object to go to, if any.
#[derive(Clone, Debug)]
pub struct Issue {
    pub severity: Severity,
    pub at: Option<(u32, usize)>,
    pub message: String,
}

/// A container as the browser lists it.
#[derive(Clone, Debug)]
pub struct ContainerInfo {
    pub hash: u32,
    pub name: Option<&'static str>,
    pub objects: usize,
    /// The DLC slot holds (and so wins for) this container.
    pub in_dlc: bool,
}

pub struct Session {
    pub game: PathBuf,
    src: EditNodesSources,
    pub(crate) journal: Vec<Edit>,
    /// The undo group of each journal entry (same length as `journal`). Undo and redo step over a
    /// whole group, so a multi-object move is one step.
    pub(crate) groups: Vec<u64>,
    /// A name per undo group ("Move 3 objects", "Rotate", ...).
    pub(crate) group_names: BTreeMap<u64, String>,
    /// Names of tags this session minted for copies (`path\name_N`), which no name table knows.
    pub(crate) minted_names: BTreeMap<u32, String>,
    pub(crate) cursor: usize,
    work: BTreeMap<u32, Entry>,
    /// Increments on every change (edit, undo, redo, applied patch). A patch carrying
    /// `expected_revision` is refused when it differs, so a plan made against an older state can't
    /// land on a newer one.
    pub(crate) revision: u64,
    /// Receipts of applied patches by `patch_id`: re-applying the same id returns the stored receipt
    /// and changes nothing.
    pub(crate) patches: Vec<(String, Value)>,
}

fn leaf(o: &Node, index: usize) -> Option<&[u8]> {
    match o.params().get(index)? {
        Node::Leaf { data, .. } => Some(data),
        Node::Container { .. } => None,
    }
}

impl Session {
    pub fn open(game: &Path) -> Result<Session, String> {
        Ok(Session::from_sources(game, EditNodesSources::open(game)?))
    }

    pub fn from_sources(game: &Path, src: EditNodesSources) -> Session {
        Session {
            game: game.to_path_buf(),
            src,
            journal: Vec::new(),
            groups: Vec::new(),
            group_names: BTreeMap::new(),
            minted_names: BTreeMap::new(),
            cursor: 0,
            work: BTreeMap::new(),
            revision: 0,
            patches: Vec::new(),
        }
    }

    fn next_group(&self) -> u64 {
        self.groups.iter().max().map_or(0, |g| g + 1)
    }

    pub fn sources(&self) -> &EditNodesSources {
        &self.src
    }

    /// Every container in either pack, base order first, then DLC-only entries.
    pub fn containers(&self) -> Vec<ContainerInfo> {
        let mut v: Vec<ContainerInfo> = Vec::new();
        for e in &self.src.base.entries {
            let in_dlc = self.src.dlc.entry(e.hash).is_some();
            let n = if in_dlc { self.src.dlc.entry(e.hash).unwrap().objects().len() } else { e.objects().len() };
            v.push(ContainerInfo { hash: e.hash, name: editnodes::tag_name(e.hash), objects: n, in_dlc });
        }
        for e in &self.src.dlc.entries {
            if self.src.base.entry(e.hash).is_none() {
                v.push(ContainerInfo { hash: e.hash, name: editnodes::tag_name(e.hash), objects: e.objects().len(), in_dlc: true });
            }
        }
        v
    }

    /// Where a container's winning copy comes from.
    pub fn source_of(&self, container: u32) -> Option<&'static str> {
        self.src.winner(container).map(|(s, _)| s)
    }

    /// The container's current state: its edited copy if any active edit touches it, else the
    /// winning retail entry.
    pub fn entry(&self, container: u32) -> Option<&Entry> {
        self.work.get(&container).or_else(|| self.src.winner(container).map(|(_, e)| e))
    }

    fn winner_copy(&self, container: u32) -> Result<Entry, String> {
        let pack = if self.src.dlc.entry(container).is_some() { &self.src.dlc } else { &self.src.base };
        Ok(pack.override_pack(&[container])?.entries.remove(0))
    }

    pub(crate) fn rebuild(&mut self) -> Result<(), String> {
        let mut work: BTreeMap<u32, Entry> = BTreeMap::new();
        for e in &self.journal[..self.cursor] {
            let c = e.container();
            if !work.contains_key(&c) {
                work.insert(c, self.winner_copy(c)?);
            }
            editnodes_stage::apply_edit(work.get_mut(&c).unwrap(), e)?;
        }
        self.work = work;
        Ok(())
    }

    /// Replace the bytes of one leaf (the `occurrence`-th leaf with `param_tag` on the object at
    /// `object_index`). Drops any redo history. A no-op change is not recorded.
    pub fn set_param(&mut self, container: u32, object_index: usize, param_tag: u32, occurrence: usize, new: Vec<u8>) -> Result<(), String> {
        let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
        let o = entry.objects().get(object_index).ok_or_else(|| format!("no object {object_index}"))?;
        let idx = *o
            .params_with_tag(param_tag)
            .get(occurrence)
            .ok_or_else(|| format!("object {object_index} has no occurrence {occurrence} of param {param_tag:08x}"))?;
        let old = leaf(o, idx).ok_or("addressed param is not a leaf")?.to_vec();
        if old == new {
            return Ok(());
        }
        let name = format!("Set {}", editnodes::tag_name(param_tag).unwrap_or("param"));
        let e = ParamEdit { container, object_index, object_tag: o.tag(), param_tag, occurrence, old, new };
        self.push_group(vec![Edit::Param(e)], &name)
    }

    /// Change an object's `ClassName` (its template/archetype) as one undo step. The name is stored
    /// as a NUL-terminated string, so a longer or shorter name resizes the leaf; the format permits
    /// that (`set_param` records the old bytes and rebuild fails closed). Refused when the object has
    /// zero or several `ClassName` leaves, when the name is empty, contains NUL, or is absurdly long.
    pub fn set_class_name(&mut self, container: u32, object_index: usize, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("ClassName cannot be empty".into());
        }
        if name.as_bytes().contains(&0) {
            return Err("ClassName cannot contain a NUL byte".into());
        }
        if name.len() > 255 {
            return Err(format!("ClassName is {} bytes; 255 is the limit here", name.len()));
        }
        let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
        let o = entry.objects().get(object_index).ok_or_else(|| format!("no object {object_index}"))?;
        let n = o.params_with_tag(tags::CLASS_NAME).len();
        if n != 1 {
            return Err(format!("object {object_index} has {n} ClassName params; exactly one is required to change it"));
        }
        let mut new = name.as_bytes().to_vec();
        new.push(0);
        self.set_param(container, object_index, tags::CLASS_NAME, 0, new)
    }

    /// Append edits as one named undo group (dropping any redo history) and rebuild; all or nothing.
    pub(crate) fn push_group(&mut self, edits: Vec<Edit>, name: &str) -> Result<(), String> {
        if edits.is_empty() {
            return Ok(());
        }
        let g = self.next_group();
        self.group_names.insert(g, name.to_string());
        self.journal.truncate(self.cursor);
        self.groups.truncate(self.cursor);
        let n = edits.len();
        self.journal.extend(edits);
        self.groups.extend(std::iter::repeat(g).take(n));
        self.cursor += n;
        if let Err(err) = self.rebuild() {
            self.cursor -= n;
            self.journal.truncate(self.cursor);
            self.groups.truncate(self.cursor);
            self.rebuild()?;
            return Err(err);
        }
        self.revision += 1;
        Ok(())
    }

    /// Move several objects as one undo step. Every object must have exactly one `Position` leaf;
    /// any refusal leaves the session unchanged. Unchanged positions are skipped.
    pub fn move_objects(&mut self, moves: &[(u32, usize, [f32; 3])]) -> Result<usize, String> {
        let mut edits = Vec::new();
        for &(container, object_index, pos) in moves {
            let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
            let o = entry.objects().get(object_index).ok_or_else(|| format!("no object {object_index} in {container:08x}"))?;
            let idx = o.params_with_tag(tags::POSITION);
            if idx.len() != 1 {
                return Err(format!("object {object_index} of {container:08x} has {} Position params; exactly one is required to move it", idx.len()));
            }
            let old = leaf(o, idx[0]).ok_or("Position is not a leaf")?.to_vec();
            let new: Vec<u8> = pos.iter().flat_map(|v| v.to_le_bytes()).collect();
            if old != new {
                edits.push(Edit::Param(ParamEdit { container, object_index, object_tag: o.tag(), param_tag: tags::POSITION, occurrence: 0, old, new }));
            }
        }
        let n = edits.len();
        let name = if n == 1 { "Move".to_string() } else { format!("Move {n} objects") };
        self.push_group(edits, &name)?;
        Ok(n)
    }

    /// Rotate objects by `r` (a 3×3 rotation, row-major, applied as `r · v`) as one undo step:
    /// each object's XAxis/YAxis/ZAxis basis is replaced by `r` times it, and with a `pivot` its
    /// Position turns about the pivot too. The stored basis is the rotation's only representation:
    /// nothing is converted to or from Euler angles (summing Euler angles is a known source of drift
    /// in other editors). A rotated basis is re-orthonormalised only if it drifted past 1e-5,
    /// keeping its handedness, so an identity rotation writes nothing. Every object must carry one
    /// 12-byte leaf of each axis; any refusal leaves the session unchanged.
    pub fn rotate_objects(&mut self, objs: &[(u32, usize)], r: [[f32; 3]; 3], pivot: Option<[f32; 3]>) -> Result<usize, String> {
        let mul = |v: [f32; 3]| -> [f32; 3] {
            [
                r[0][0] * v[0] + r[0][1] * v[1] + r[0][2] * v[2],
                r[1][0] * v[0] + r[1][1] * v[1] + r[1][2] * v[2],
                r[2][0] * v[0] + r[2][1] * v[1] + r[2][2] * v[2],
            ]
        };
        let bytes = |v: [f32; 3]| -> Vec<u8> { v.iter().flat_map(|x| x.to_le_bytes()).collect() };
        let mut edits = Vec::new();
        for &(container, object_index) in objs {
            let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
            let o = entry.objects().get(object_index).ok_or_else(|| format!("no object {object_index} in {container:08x}"))?;
            let mut axes = [[0f32; 3]; 3];
            for (k, tag) in [tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS].into_iter().enumerate() {
                let idx = o.params_with_tag(tag);
                if idx.len() != 1 {
                    return Err(format!("object {object_index} of {container:08x} has {} {} params; exactly one of each axis is required to rotate it", idx.len(), editnodes::tag_name(tag).unwrap_or("axis")));
                }
                axes[k] = o.vec3_param(tag).ok_or("axis is not 12 bytes")?;
            }
            let new_axes = orthonormal_keep_handedness([mul(axes[0]), mul(axes[1]), mul(axes[2])]);
            for (k, tag) in [tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS].into_iter().enumerate() {
                let (old, new) = (bytes(axes[k]), bytes(new_axes[k]));
                if old != new {
                    edits.push(Edit::Param(ParamEdit { container, object_index, object_tag: o.tag(), param_tag: tag, occurrence: 0, old, new }));
                }
            }
            if let (Some(pv), Some(p)) = (pivot, o.position()) {
                if o.params_with_tag(tags::POSITION).len() == 1 {
                    let d = mul([p[0] - pv[0], p[1] - pv[1], p[2] - pv[2]]);
                    let np = [pv[0] + d[0], pv[1] + d[1], pv[2] + d[2]];
                    let (old, new) = (bytes(p), bytes(np));
                    if old != new {
                        edits.push(Edit::Param(ParamEdit { container, object_index, object_tag: o.tag(), param_tag: tags::POSITION, occurrence: 0, old, new }));
                    }
                }
            }
        }
        let n = objs.len();
        let name = if n == 1 { "Rotate".to_string() } else { format!("Rotate {n} objects") };
        self.push_group(edits, &name)?;
        Ok(n)
    }

    /// Uniformly scale the stored XAxis/YAxis/ZAxis basis of objects (all three axes × `factor`) as one
    /// undo step. Uniform scaling keeps the basis right-handed and orthonormal; it is the only scale
    /// representation EditNodes has (there is no scale field). Non-uniform scaling would skew the
    /// placement and is refused here. Every object must carry exactly one 12-byte leaf of each axis and
    /// the factor must be finite and positive. **Not verified in game**: statically checked only until
    /// a Gate A+ run covers it.
    pub fn scale_objects(&mut self, objs: &[(u32, usize)], factor: f32) -> Result<usize, String> {
        if !factor.is_finite() || factor <= 0.0 {
            return Err(format!("scale factor {factor} must be finite and > 0"));
        }
        let bytes = |v: [f32; 3]| -> Vec<u8> { v.iter().flat_map(|x| x.to_le_bytes()).collect() };
        let neg = |v: [f32; 3]| -> [f32; 3] { v.map(|x| (x * factor) as f32) };
        let mut edits = Vec::new();
        for &(container, object_index) in objs {
            let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
            let o = entry.objects().get(object_index).ok_or_else(|| format!("no object {object_index} in {container:08x}"))?;
            for tag in [tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS] {
                let idx = o.params_with_tag(tag);
                if idx.len() != 1 {
                    return Err(format!(
                        "object {object_index} of {container:08x} has {} {} params; exactly one of each axis is required to scale it",
                        idx.len(),
                        editnodes::tag_name(tag).unwrap_or("axis")
                    ));
                }
                let old = leaf(o, idx[0]).ok_or("axis is not a leaf")?.to_vec();
                let v = o.vec3_param(tag).ok_or("axis is not 12 bytes")?;
                let new = bytes(neg(v));
                if old != new {
                    edits.push(Edit::Param(ParamEdit { container, object_index, object_tag: o.tag(), param_tag: tag, occurrence: 0, old, new }));
                }
            }
        }
        let n = objs.len();
        let name = if n == 1 { "Scale".to_string() } else { format!("Scale {n} objects") };
        self.push_group(edits, &name)?;
        Ok(n)
    }

    /// Insert objects as one undo step: each `(container, index, node)` is inserted in the order
    /// given (indices refer to the state after the previous inserts). Refuses an object whose tag
    /// already exists in its container. Returns the inserted objects' addresses.
    pub fn insert_objects(&mut self, items: Vec<(u32, usize, Node)>, name: &str) -> Result<Vec<(u32, usize)>, String> {
        let mut edits = Vec::new();
        let mut placed = Vec::new();
        let mut added: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (container, index, node) in items {
            let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
            let tag = node.tag();
            if entry.objects().iter().any(|o| o.tag() == tag) || added.get(&container).map_or(false, |v| v.contains(&tag)) {
                return Err(format!("tag {tag:08x} already exists in container {container:08x}"));
            }
            added.entry(container).or_default().push(tag);
            for p in placed.iter_mut() {
                let (c, i): &mut (u32, usize) = p;
                if *c == container && *i >= index {
                    *i += 1;
                }
            }
            placed.push((container, index));
            edits.push(Edit::Insert { container, index, node });
        }
        self.push_group(edits, name)?;
        Ok(placed)
    }

    /// Remove objects as one undo step. Indices refer to the current state; they are removed from
    /// the highest index down so the others stay valid.
    pub fn remove_objects(&mut self, objs: &[(u32, usize)]) -> Result<usize, String> {
        let mut sorted: Vec<(u32, usize)> = objs.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        sorted.dedup();
        let mut edits = Vec::new();
        for &(container, index) in &sorted {
            let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
            let node = entry.objects().get(index).ok_or_else(|| format!("no object {index} in {container:08x}"))?.clone();
            edits.push(Edit::Remove { container, index, node });
        }
        let n = edits.len();
        let name = if n == 1 { "Delete".to_string() } else { format!("Delete {n} objects") };
        self.push_group(edits, &name)?;
        Ok(n)
    }

    /// Set the `occurrence`-th LuaParam leaf of an object to `value`, keeping its name. Refuses a
    /// leaf that does not decode as a LuaParam. A change of the leaf's size is allowed by the codec
    /// (framing is positional) but is not observed in game; [`Edit::is_same_size_param`] tells.
    pub fn set_lua_param(&mut self, container: u32, object_index: usize, occurrence: usize, value: editnodes::LuaValue) -> Result<(), String> {
        let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
        let o = entry.objects().get(object_index).ok_or_else(|| format!("no object {object_index}"))?;
        let idx = *o.params_with_tag(tags::LUA_PARAM).get(occurrence).ok_or_else(|| format!("object {object_index} has no LuaParam #{occurrence}"))?;
        let old = leaf(o, idx).ok_or("LuaParam is not a leaf")?;
        let mut p = editnodes::lua_param(old)?;
        p.value = value;
        let new = editnodes::lua_param_bytes(&p);
        self.set_param(container, object_index, tags::LUA_PARAM, occurrence, new)?;
        if let Some(g) = self.groups.last() {
            self.group_names.insert(*g, format!("Set {}", p.name));
        }
        Ok(())
    }

    /// A new object tag for a copy of `source_tag` placed in `container`, unique across every
    /// container (Lua resolves names globally). With the container's path and the source's name
    /// known, the tag is `object_tag(path, "<name>_<N>")`, so Lua can address the copy by that
    /// name; otherwise it is a hash of a synthetic path and the copy is not addressable by name.
    pub fn mint_tag(&self, container: u32, source_tag: u32) -> (u32, Option<String>) {
        self.mint_tag_excluding(container, source_tag, &[])
    }

    /// [`mint_tag`] that also treats `extra` tags as taken. A batch copy mints several tags before any
    /// of them is inserted, so the caller must pass the ones already chosen.
    pub fn mint_tag_excluding(&self, container: u32, source_tag: u32, extra: &[u32]) -> (u32, Option<String>) {
        let taken = |t: u32| extra.contains(&t) || self.containers().iter().any(|c| self.entry(c.hash).map_or(false, |e| e.objects().iter().any(|o| o.tag() == t)));
        let container_path = editnodes::tag_name(container).map(|p| p.trim_start_matches(r"France\EditNodes\").trim_end_matches(".wsd").to_string());
        let base = editnodes::tag_name(source_tag).map(|n| n.rsplit('\\').next().unwrap_or(n).to_string());
        if let (Some(path), Some(base)) = (container_path, base) {
            let stem = base.trim_end_matches(|c: char| c.is_ascii_digit()).trim_end_matches('_').to_string();
            for n in 1..10_000 {
                let name = format!("{stem}_{n}");
                let t = editnodes::object_tag(&path, &name);
                if !taken(t) {
                    return (t, Some(format!("{path}\\{name}")));
                }
            }
        }
        for n in 1..u32::MAX {
            let t = sab_formats::pandemic_hash(&format!("sab_places_copy\\{container:08x}\\{source_tag:08x}\\{n}"));
            if !taken(t) {
                return (t, None);
            }
        }
        unreachable!("tag space exhausted")
    }

    /// Remember the name behind a minted tag, for display (saved with the project).
    pub fn note_name(&mut self, tag: u32, name: &str) {
        self.minted_names.insert(tag, name.to_string());
    }

    /// An object's full name: the name table's, else one this session minted.
    pub fn object_name(&self, tag: u32) -> Option<String> {
        editnodes::tag_name(tag).map(str::to_string).or_else(|| self.minted_names.get(&tag).cloned())
    }

    /// The name of an undo group.
    pub fn group_name(&self, g: u64) -> &str {
        self.group_names.get(&g).map(String::as_str).unwrap_or("Edit")
    }

    /// Move an object: rewrite its single `Position` leaf. Objects with no or several `Position`
    /// leaves are refused rather than guessed at.
    pub fn move_object(&mut self, container: u32, object_index: usize, pos: [f32; 3]) -> Result<(), String> {
        let entry = self.entry(container).ok_or_else(|| format!("container {container:08x} not found"))?;
        let o = entry.objects().get(object_index).ok_or_else(|| format!("no object {object_index}"))?;
        let n = o.params_with_tag(tags::POSITION).len();
        if n != 1 {
            return Err(format!("object has {n} Position params; exactly one is required to move it"));
        }
        let bytes: Vec<u8> = pos.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.set_param(container, object_index, tags::POSITION, 0, bytes)
    }

    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_redo(&self) -> bool {
        self.cursor < self.journal.len()
    }

    /// Undo the last group of edits.
    pub fn undo(&mut self) -> Result<bool, String> {
        if !self.can_undo() {
            return Ok(false);
        }
        let g = self.groups[self.cursor - 1];
        while self.cursor > 0 && self.groups[self.cursor - 1] == g {
            self.cursor -= 1;
        }
        self.rebuild()?;
        self.revision += 1;
        Ok(true)
    }

    /// Redo the next group of edits.
    pub fn redo(&mut self) -> Result<bool, String> {
        if !self.can_redo() {
            return Ok(false);
        }
        let g = self.groups[self.cursor];
        while self.cursor < self.journal.len() && self.groups[self.cursor] == g {
            self.cursor += 1;
        }
        self.rebuild()?;
        self.revision += 1;
        Ok(true)
    }

    /// The undo group of each journal entry.
    pub fn groups(&self) -> &[u64] {
        &self.groups
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The whole journal and the undo cursor (edits before it are active).
    pub fn journal(&self) -> (&[Edit], usize) {
        (&self.journal, self.cursor)
    }

    /// The active edits, in order.
    pub fn active(&self) -> &[Edit] {
        &self.journal[..self.cursor]
    }

    pub fn to_project_json(&self) -> Value {
        let edits: Vec<Value> = self
            .journal
            .iter()
            .zip(&self.groups)
            .map(|(e, g)| {
                let mut v = editnodes_stage::edit_json(e);
                v["group"] = json!(g);
                v
            })
            .collect();
        let names: serde_json::Map<String, Value> = self.group_names.iter().map(|(g, n)| (g.to_string(), json!(n))).collect();
        let minted: serde_json::Map<String, Value> = self.minted_names.iter().map(|(t, n)| (format!("{t:08x}"), json!(n))).collect();
        let patches: Vec<Value> = self.patches.iter().map(|(id, r)| json!({ "patch_id": id, "receipt": r })).collect();
        json!({ "format": PROJECT_FORMAT, "version": PROJECT_VERSION, "game_dir": self.game.display().to_string(),
                "inputs": { "loosefiles_sha256": self.src.loosefiles_sha, "dlc_editnodes_sha256": self.src.dlc_sha() },
                "cursor": self.cursor, "revision": self.revision, "edits": edits, "group_names": names, "minted_names": minted, "patches": patches })
    }

    /// Write the project file. An existing file is first rotated into `<file>.bak1` … `.bak5`
    /// (newest first), so a save never destroys the previous state and an older-format file is kept
    /// before it is rewritten in the current format.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        crate::stage::guard_write(path, &self.game)?;
        if path.is_file() {
            let bak = |k: usize| PathBuf::from(format!("{}.bak{k}", path.display()));
            for k in (1..BACKUPS).rev() {
                if bak(k).is_file() {
                    std::fs::rename(bak(k), bak(k + 1)).map_err(|e| format!("{}: {e}", bak(k).display()))?;
                }
            }
            std::fs::copy(path, bak(1)).map_err(|e| format!("{}: {e}", bak(1).display()))?;
        }
        let text = serde_json::to_string_pretty(&self.to_project_json()).unwrap();
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Checks before staging (after Hammer's map check and CP77's export checks): errors block
    /// staging, warnings and notes are shown. Each issue names the object to go to.
    pub fn validate(&self) -> Vec<Issue> {
        let mut out = Vec::new();
        let mut touched: Vec<u32> = self.active().iter().map(|e| e.container()).collect();
        touched.sort_unstable();
        touched.dedup();
        for &c in &touched {
            let Some(e) = self.entry(c) else { continue };
            let Some((_, src)) = self.src.winner(c) else { continue };
            let count = |objs: &[Node], t: u32| objs.iter().filter(|o| o.tag() == t).count();
            for (i, o) in e.objects().iter().enumerate() {
                let (now, before) = (count(e.objects(), o.tag()), count(src.objects(), o.tag()));
                if now > 1 && now > before {
                    out.push(Issue { severity: Severity::Error, at: Some((c, i)), message: format!("tag {:08x} appears {now} times in this container (retail: {before}); Lua could not tell them apart", o.tag()) });
                }
            }
            if e.objects().is_empty() {
                out.push(Issue { severity: Severity::Warning, at: None, message: format!("container {c:08x} is now empty") });
            }
        }
        let unverified = self.active().iter().filter(|e| !e.kind_demonstrated_on_locators()).count();
        if unverified > 0 {
            out.push(Issue {
                severity: Severity::Note,
                at: None,
                message: format!("{unverified} active edit(s) are of kinds never demonstrated in game (a LuaParam, a tilt, another leaf, or a resize): statically checked only"),
            });
        }
        out
    }

    /// Reopen a project against the current game files.
    pub fn load(path: &Path) -> Result<Session, String> {
        let v = read_project(path)?;
        let game = PathBuf::from(v["game_dir"].as_str().ok_or("project has no game_dir")?);
        Session::load_with(EditNodesSources::open(&game)?, &game, &v)
    }

    /// Reopen a project's journal over the given sources. Refuses if the inputs' hashes differ from
    /// those recorded, or if any edit's expected old bytes do not match on replay.
    pub fn load_with(src: EditNodesSources, game: &Path, v: &Value) -> Result<Session, String> {
        if v["format"] != PROJECT_FORMAT || !matches!(v["version"].as_u64(), Some(1) | Some(PROJECT_VERSION)) {
            return Err(format!("not a {PROJECT_FORMAT} v1/v{PROJECT_VERSION} file"));
        }
        let want_lf = v["inputs"]["loosefiles_sha256"].as_str().unwrap_or("");
        let want_dlc = v["inputs"]["dlc_editnodes_sha256"].as_str().unwrap_or("");
        if want_lf != src.loosefiles_sha || want_dlc != src.dlc_sha() {
            let short = |s: &str| s.get(..12).unwrap_or(s).to_string();
            return Err(format!(
                "the game files changed since this project was saved (loosefiles {} vs {}, DLC/01 EditNodes {} vs {})",
                short(want_lf),
                short(&src.loosefiles_sha),
                short(want_dlc),
                short(&src.dlc_sha())
            ));
        }
        let mut journal = Vec::new();
        let mut groups = Vec::new();
        for (k, x) in v["edits"].as_array().ok_or("project has no edits array")?.iter().enumerate() {
            // Files written before undo groups existed: one group per edit.
            groups.push(x["group"].as_u64().unwrap_or(k as u64));
            journal.push(editnodes_stage::edit_from_json(x)?);
        }
        let mut group_names = BTreeMap::new();
        if let Some(m) = v["group_names"].as_object() {
            for (g, n) in m {
                if let (Ok(g), Some(n)) = (g.parse::<u64>(), n.as_str()) {
                    group_names.insert(g, n.to_string());
                }
            }
        }
        let cursor = v["cursor"].as_u64().ok_or("cursor")? as usize;
        if cursor > journal.len() {
            return Err("cursor is past the end of the journal".into());
        }
        if cursor > 0 && cursor < journal.len() && groups[cursor - 1] == groups[cursor] {
            return Err("the undo cursor falls inside an undo group".into());
        }
        let mut s = Session::from_sources(game, src);
        s.journal = journal;
        s.groups = groups;
        s.group_names = group_names;
        if let Some(m) = v["minted_names"].as_object() {
            for (t, n) in m {
                if let (Ok(t), Some(n)) = (u32::from_str_radix(t, 16), n.as_str()) {
                    s.minted_names.insert(t, n.to_string());
                }
            }
        }
        // Replay everything once so that redo entries are validated too, then settle at the cursor.
        s.cursor = s.journal.len();
        s.rebuild()?;
        s.cursor = cursor;
        s.rebuild()?;
        s.revision = v["revision"].as_u64().unwrap_or(0);
        if let Some(ps) = v["patches"].as_array() {
            for p in ps {
                let id = p["patch_id"].as_str().ok_or("patch receipt without patch_id")?.to_string();
                s.patches.push((id, p["receipt"].clone()));
            }
        }
        Ok(s)
    }

    /// Stage the active edits as a DLC/01 EditNodes override in `out` (never in the game folder).
    pub fn stage(&self, out: &Path) -> Result<StageOutcome, String> {
        crate::stage::guard_write(out, &self.game)?;
        if let Some(e) = self.validate().into_iter().find(|i| i.severity == Severity::Error) {
            return Err(format!("validation error blocks staging: {}", e.message));
        }
        let outcome = editnodes_stage::stage_editnodes(&self.game, &self.src, self.active(), out, "placements", json!({ "tool": "sab_tools session" }))?;
        let expect = json!({ "experiment": "placements", "artifact_sha256": outcome.artifact_sha, "positions": self.position_expectations() });
        std::fs::write(outcome.out_dir.join("expect.json"), serde_json::to_string_pretty(&expect).unwrap())
            .map_err(|e| format!("{}: {e}", outcome.out_dir.join("expect.json").display()))?;
        Ok(outcome)
    }

    /// Every object whose stored Position the active edits change, by container and tag: its name
    /// (`null` when not recovered, so a check by name is impossible), retail position (`null` for an
    /// inserted object) and staged position. A tag repeated within a container is listed with
    /// `"ambiguous": true`, since a lookup by name cannot tell its copies apart.
    pub fn position_expectations(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let touched: std::collections::BTreeSet<u32> = self.active().iter().map(|e| e.container()).collect();
        for c in touched {
            let (Some(now), Some((_, was))) = (self.entry(c), self.src.winner(c)) else { continue };
            for o in now.objects() {
                let Some(p) = o.position() else { continue };
                let same_tag = |e: &Entry| e.objects().iter().filter(|q| q.tag() == o.tag()).count();
                let before: Vec<[f32; 3]> = was.objects().iter().filter(|q| q.tag() == o.tag()).filter_map(|q| q.position()).collect();
                if before.len() == 1 && before[0] == p {
                    continue;
                }
                out.push(json!({
                    "container": format!("{c:08x}"),
                    "tag": format!("{:08x}", o.tag()),
                    "name": self.object_name(o.tag()),
                    "original": before.first(),
                    "expected": p,
                    "ambiguous": same_tag(now) > 1 || before.len() > 1,
                }));
            }
        }
        out
    }
}

/// Re-orthonormalise a basis only if it drifted past 1e-5 (Gram–Schmidt from X, then Z; Y rebuilt
/// from them with the input's handedness). A basis already within tolerance is returned unchanged.
fn orthonormal_keep_handedness(a: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let dot = |p: [f32; 3], q: [f32; 3]| p[0] * q[0] + p[1] * q[1] + p[2] * q[2];
    let cross = |p: [f32; 3], q: [f32; 3]| [p[1] * q[2] - p[2] * q[1], p[2] * q[0] - p[0] * q[2], p[0] * q[1] - p[1] * q[0]];
    let norm = |p: [f32; 3]| {
        let l = dot(p, p).sqrt();
        [p[0] / l, p[1] / l, p[2] / l]
    };
    let [x, y, z] = a;
    let dev = [dot(x, x) - 1.0, dot(y, y) - 1.0, dot(z, z) - 1.0, dot(x, y), dot(y, z), dot(z, x)].iter().fold(0f32, |m, v| m.max(v.abs()));
    if dev <= 1e-5 {
        return a;
    }
    let nx = norm(x);
    let d = dot(z, nx);
    let nz = norm([z[0] - d * nx[0], z[1] - d * nx[1], z[2] - d * nx[2]]);
    let c = cross(nz, nx);
    let ny = if dot(c, y) >= 0.0 { c } else { [-c[0], -c[1], -c[2]] };
    [nx, ny, nz]
}

pub fn read_project(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editnodes_stage::hex;
    use editnodes::{Pack, Root};

    fn obj(tag: u32, class: &str, pos: [f32; 3]) -> Node {
        let mut cn = class.as_bytes().to_vec();
        cn.push(0);
        Node::Container {
            tag,
            children: vec![
                Node::Leaf { tag: tags::CLASS_NAME, data: cn },
                Node::Leaf { tag: tags::POSITION, data: pos.iter().flat_map(|v| v.to_le_bytes()).collect() },
                Node::Leaf { tag: tags::LUA_PARAM, data: b"a\0\x31\x4f\x44\x1b\x01".to_vec() },
                Node::Leaf { tag: tags::LUA_PARAM, data: b"b\0\x31\x4f\x44\x1b\x01".to_vec() },
            ],
        }
    }

    fn sources() -> EditNodesSources {
        let base = Pack {
            entries: vec![
                editnodes::Entry { hash: 0x10, root: Root::EditNode { objects: vec![obj(0xA, "Locator", [1.0, 2.0, 3.0]), obj(0xB, "Prop", [4.0, 5.0, 6.0])] } },
                editnodes::Entry { hash: 0x20, root: Root::EditNode { objects: vec![obj(0xC, "Spore", [7.0, 8.0, 9.0])] } },
            ],
        };
        let dlc = Pack { entries: vec![editnodes::Entry { hash: 0x20, root: Root::EditNode { objects: vec![obj(0xD, "Spore", [0.5, 0.5, 0.5])] } }] };
        EditNodesSources::from_bytes(base.write(), dlc.write(), "LF".into()).unwrap()
    }

    fn pos(s: &Session, c: u32, i: usize) -> [f32; 3] {
        s.entry(c).unwrap().objects()[i].position().unwrap()
    }

    #[test]
    fn the_dlc_copy_wins_and_base_only_containers_come_from_base() {
        let s = Session::from_sources(Path::new("G"), sources());
        assert_eq!(s.source_of(0x20), Some("DLC/01"));
        assert_eq!(pos(&s, 0x20, 0), [0.5, 0.5, 0.5]);
        assert_eq!(s.source_of(0x10), Some("base (loosefiles)"));
        let c = s.containers();
        assert_eq!(c.len(), 2);
        assert!(c[1].in_dlc && c[1].objects == 1);
    }

    #[test]
    fn move_undo_redo_and_a_new_edit_drops_redo() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        s.move_object(0x10, 1, [40.0, 5.0, 6.0]).unwrap();
        s.move_object(0x10, 1, [41.0, 5.0, 6.0]).unwrap();
        assert_eq!(pos(&s, 0x10, 1), [41.0, 5.0, 6.0]);
        assert!(s.undo().unwrap());
        assert_eq!(pos(&s, 0x10, 1), [40.0, 5.0, 6.0]);
        assert!(s.undo().unwrap());
        assert_eq!(pos(&s, 0x10, 1), [4.0, 5.0, 6.0]);
        assert!(!s.undo().unwrap());
        assert!(s.redo().unwrap());
        assert_eq!(pos(&s, 0x10, 1), [40.0, 5.0, 6.0]);
        s.move_object(0x10, 0, [9.0, 9.0, 9.0]).unwrap();
        assert!(!s.can_redo(), "a new edit drops the redo history");
        assert_eq!(s.journal().0.len(), 2);
        assert_eq!(pos(&s, 0x10, 0), [9.0, 9.0, 9.0]);
        assert_eq!(pos(&s, 0x10, 1), [40.0, 5.0, 6.0]);
    }

    /// A multi-object move across containers is one undo step, survives save/reopen as one step, and
    /// a refused object leaves the session unchanged.
    #[test]
    fn a_group_move_is_one_undo_step_and_all_or_nothing() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        s.move_object(0x10, 0, [1.5, 2.0, 3.0]).unwrap();
        assert_eq!(s.move_objects(&[(0x10, 1, [5.0, 5.0, 6.0]), (0x20, 0, [1.5, 0.5, 0.5])]).unwrap(), 2);
        assert_eq!(s.journal().0.len(), 3);
        let dir = std::env::temp_dir().join(format!("sab_group_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("g.json");
        s.save(&p).unwrap();
        let mut r = Session::load_with(sources(), Path::new("G"), &read_project(&p).unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        for t in [&mut s, &mut r] {
            assert!(t.undo().unwrap());
            assert_eq!((pos(t, 0x10, 1), pos(t, 0x20, 0)), ([4.0, 5.0, 6.0], [0.5, 0.5, 0.5]), "both objects return in one step");
            assert_eq!(pos(t, 0x10, 0), [1.5, 2.0, 3.0], "the earlier single move stays");
            assert!(t.redo().unwrap());
            assert_eq!((pos(t, 0x10, 1), pos(t, 0x20, 0)), ([5.0, 5.0, 6.0], [1.5, 0.5, 0.5]));
        }
        let before = (s.journal().0.len(), s.revision());
        assert!(s.move_objects(&[(0x10, 1, [9.0, 9.0, 9.0]), (0x10, 7, [0.0, 0.0, 0.0])]).is_err());
        assert_eq!((s.journal().0.len(), s.revision()), before);
        assert_eq!(pos(&s, 0x10, 1), [5.0, 5.0, 6.0]);
    }

    #[test]
    fn a_repeated_tag_is_addressed_by_occurrence() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        s.set_param(0x10, 0, tags::LUA_PARAM, 1, b"b\0\x31\x4f\x44\x1b\x00".to_vec()).unwrap();
        let o = &s.entry(0x10).unwrap().objects()[0];
        let idx = o.params_with_tag(tags::LUA_PARAM);
        assert_eq!(leaf(o, idx[0]).unwrap(), b"a\0\x31\x4f\x44\x1b\x01", "the first LuaParam is untouched");
        assert_eq!(leaf(o, idx[1]).unwrap(), b"b\0\x31\x4f\x44\x1b\x00");
    }

    #[test]
    fn a_class_name_change_is_one_stable_undo_step() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        assert_eq!(s.entry(0x10).unwrap().objects()[1].class_name().as_deref(), Some("Prop"));
        s.set_class_name(0x10, 1, "P_INT_chair2").unwrap();
        assert_eq!(s.entry(0x10).unwrap().objects()[1].class_name().as_deref(), Some("P_INT_chair2"));
        assert_eq!(s.journal().0.len(), 1);
        assert!(s.undo().unwrap());
        assert_eq!(s.entry(0x10).unwrap().objects()[1].class_name().as_deref(), Some("Prop"));
        assert!(s.redo().unwrap());
        assert_eq!(s.entry(0x10).unwrap().objects()[1].class_name().as_deref(), Some("P_INT_chair2"));
        assert!(s.set_class_name(0x10, 1, "").is_err());
        assert!(s.set_class_name(0x10, 1, "A\0B").is_err());
        assert!(s.set_class_name(0x10, 9, "X").is_err());
        // The old bytes are part of the edit, so staging after the change keeps every other byte.
        let dir = std::env::temp_dir().join(format!("sab_class_{}", std::process::id()));
        let out = s.stage(&dir).unwrap();
        assert!(out.all_pass, "{}", out.report);
        let staged = Pack::parse(&std::fs::read(dir.join("EditNodes.pack")).unwrap()).unwrap();
        assert_eq!(staged.entry(0x10).unwrap().objects()[1].class_name().as_deref(), Some("P_INT_chair2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_and_reopen_restores_the_journal_and_cursor_and_refuses_changed_inputs() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        s.move_object(0x10, 1, [40.0, 5.0, 6.0]).unwrap();
        s.move_object(0x20, 0, [1.5, 0.5, 0.5]).unwrap();
        s.undo().unwrap();
        let v = s.to_project_json();
        let r = Session::load_with(sources(), Path::new("G"), &v).unwrap();
        assert_eq!(r.journal().0, s.journal().0);
        assert_eq!(r.journal().1, 1);
        assert_eq!(pos(&r, 0x10, 1), [40.0, 5.0, 6.0]);
        assert_eq!(pos(&r, 0x20, 0), [0.5, 0.5, 0.5]);
        let mut changed = sources();
        changed.loosefiles_sha = "OTHER".into();
        assert!(Session::load_with(changed, Path::new("G"), &v).is_err());
    }

    #[test]
    fn staging_writes_the_dlc_override_with_only_the_edited_bytes_changed() {
        let dir = std::env::temp_dir().join(format!("sab_session_test_{}", std::process::id()));
        let mut s = Session::from_sources(Path::new("G"), sources());
        s.move_object(0x10, 1, [40.0, 5.0, 6.0]).unwrap();
        let out = s.stage(&dir).unwrap();
        assert!(out.all_pass, "{}", out.report);
        let staged = Pack::parse(&std::fs::read(dir.join("EditNodes.pack")).unwrap()).unwrap();
        assert_eq!(staged.entries.len(), 2, "the original DLC entry plus the touched base container");
        assert_eq!(staged.entry(0x10).unwrap().objects()[1].position(), Some([40.0, 5.0, 6.0]));
        assert_eq!(staged.entry(0x20).unwrap().objects()[0].position(), Some([0.5, 0.5, 0.5]));
        for f in ["manifest.json", "install.ps1", "restore.ps1", "report.txt", "expect.json"] {
            assert!(dir.join(f).is_file(), "{f}");
        }
        let expect: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("expect.json")).unwrap()).unwrap();
        let ps = expect["positions"].as_array().unwrap();
        assert_eq!(ps.len(), 1, "only the moved object: {ps:?}");
        assert_eq!((ps[0]["container"].as_str(), ps[0]["tag"].as_str()), (Some("00000010"), Some("0000000b")));
        assert_eq!(ps[0]["original"], json!([4.0, 5.0, 6.0]));
        assert_eq!(ps[0]["expected"], json!([40.0, 5.0, 6.0]));
        assert_eq!(expect["artifact_sha256"].as_str(), Some(out.artifact_sha.as_str()));
        assert!(s.stage(Path::new("G/inside")).is_err(), "never inside the game folder");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_edit_is_refused_on_replay() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        s.move_object(0x10, 1, [40.0, 5.0, 6.0]).unwrap();
        let mut v = s.to_project_json();
        v["edits"][0]["old"] = Value::String(hex(&[0u8; 12]));
        assert!(Session::load_with(sources(), Path::new("G"), &v).is_err());
    }

    /// Objects with a yaw basis: 0xA at yaw 30°, 0xB at yaw 0°, in container 0x10.
    fn sources_with_axes() -> EditNodesSources {
        let mut a = obj(0xA, "Locator", [1.0, 2.0, 3.0]);
        a.set_yaw_degrees(30.0).unwrap();
        let mut b = obj(0xB, "Prop", [5.0, 2.0, 3.0]);
        b.set_yaw_degrees(0.0).unwrap();
        let base = Pack { entries: vec![editnodes::Entry { hash: 0x10, root: Root::EditNode { objects: vec![a, b] } }] };
        EditNodesSources::from_bytes(base.write(), Pack { entries: vec![] }.write(), "LF".into()).unwrap()
    }

    fn yaw(deg: f32) -> [[f32; 3]; 3] {
        let (s, c) = deg.to_radians().sin_cos();
        [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
    }

    fn axes(s: &Session, c: u32, i: usize) -> [[f32; 3]; 3] {
        let o = &s.entry(c).unwrap().objects()[i];
        [o.vec3_param(tags::X_AXIS).unwrap(), o.vec3_param(tags::Y_AXIS).unwrap(), o.vec3_param(tags::Z_AXIS).unwrap()]
    }

    fn close(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
        (0..3).all(|k| (a[k] - b[k]).abs() <= tol)
    }

    #[test]
    fn rotation_composes_on_the_basis_and_an_identity_writes_nothing() {
        let mut s = Session::from_sources(Path::new("G"), sources_with_axes());
        s.rotate_objects(&[(0x10, 0)], [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], None).unwrap();
        assert_eq!(s.journal().0.len(), 0, "an identity rotation writes no bytes");

        s.rotate_objects(&[(0x10, 0)], yaw(90.0), None).unwrap();
        let mut want = obj(0, "", [0.0; 3]);
        want.set_yaw_degrees(120.0).unwrap();
        let got = axes(&s, 0x10, 0);
        for (k, tag) in [tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS].into_iter().enumerate() {
            assert!(close(got[k], want.vec3_param(tag).unwrap(), 1e-5), "yaw 30° + 90° = the retail yaw-120° basis");
        }
        assert_eq!(got[1], [0.0, 1.0, 0.0], "a yaw keeps Y exactly");
        assert_eq!(s.group_name(s.groups()[0]), "Rotate");
        assert!(s.undo().unwrap());
        assert_eq!(s.active().len(), 0);
    }

    /// Uniform scale multiplies all three basis axes, keeps the basis orthonormal, is one undo step,
    /// and is (correctly) not graded as a demonstrated-in-game kind.
    #[test]
    fn uniform_scale_multiplies_the_basis_and_is_statically_checked_only() {
        let mut s = Session::from_sources(Path::new("G"), sources_with_axes());
        let before = axes(&s, 0x10, 0);
        s.scale_objects(&[(0x10, 0)], 2.0).unwrap();
        let after = axes(&s, 0x10, 0);
        for k in 0..3 {
            assert!(close(after[k], before[k].map(|v| v * 2.0), 1e-5), "axis {k} scaled");
            let n = (after[k][0] * after[k][0] + after[k][1] * after[k][1] + after[k][2] * after[k][2]).sqrt();
            assert!((n - 2.0).abs() < 1e-5, "axis {k} length {n}");
        }
        assert_eq!(s.group_name(s.groups()[0]), "Scale");
        assert!(s.active().iter().all(|e| !e.kind_demonstrated_on_locators()), "a scale is not a demonstrated yaw");
        assert!(s.undo().unwrap());
        assert_eq!(axes(&s, 0x10, 0), before);
        assert!(s.scale_objects(&[(0x10, 0)], 0.0).is_err());
        assert!(s.scale_objects(&[(0x10, 0)], f32::NAN).is_err());
    }

    #[test]
    fn a_thousand_rotations_stay_orthonormal_and_right_handed() {
        let mut s = Session::from_sources(Path::new("G"), sources_with_axes());
        let (a, b) = (0.13f32, 0.07f32);
        let (sa, ca, sb, cb) = (a.sin(), a.cos(), b.sin(), b.cos());
        // A yaw followed by a tilt about X.
        let rx = [[1.0, 0.0, 0.0], [0.0, cb, -sb], [0.0, sb, cb]];
        let ry = [[ca, 0.0, sa], [0.0, 1.0, 0.0], [-sa, 0.0, ca]];
        let mut r = [[0f32; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                r[i][j] = (0..3).map(|k| rx[i][k] * ry[k][j]).sum();
            }
        }
        for _ in 0..1000 {
            s.rotate_objects(&[(0x10, 0)], r, None).unwrap();
        }
        let [x, y, z] = axes(&s, 0x10, 0);
        let dot = |p: [f32; 3], q: [f32; 3]| p[0] * q[0] + p[1] * q[1] + p[2] * q[2];
        for v in [dot(x, x) - 1.0, dot(y, y) - 1.0, dot(z, z) - 1.0, dot(x, y), dot(y, z), dot(z, x)] {
            assert!(v.abs() < 1e-4, "drifted: {v}");
        }
        let c = [x[1] * y[2] - x[2] * y[1], x[2] * y[0] - x[0] * y[2], x[0] * y[1] - x[1] * y[0]];
        assert!(dot(c, z) > 0.999, "still right-handed (X × Y = Z)");
    }

    #[test]
    fn rotating_two_objects_about_their_midpoint_swaps_them_in_one_step() {
        let mut s = Session::from_sources(Path::new("G"), sources_with_axes());
        s.rotate_objects(&[(0x10, 0), (0x10, 1)], yaw(180.0), Some([3.0, 2.0, 3.0])).unwrap();
        assert!(close(pos(&s, 0x10, 0), [5.0, 2.0, 3.0], 1e-5) && close(pos(&s, 0x10, 1), [1.0, 2.0, 3.0], 1e-5));
        assert!(s.undo().unwrap());
        assert_eq!(s.active().len(), 0, "one undo step");
        assert!(s.rotate_objects(&[(0x20, 0)], yaw(10.0), None).is_err(), "unknown container refused");
    }

    #[test]
    fn insert_and_remove_are_undoable_survive_reopen_and_stage_with_a_structural_check() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        let mut copy = s.entry(0x10).unwrap().objects()[0].clone();
        let (tag, name) = s.mint_tag(0x10, copy.tag());
        assert!(name.is_none(), "synthetic tag when names are unknown");
        copy.set_tag(tag);
        copy.set_position([1.0, 2.0, 9.0]).unwrap();
        let placed = s.insert_objects(vec![(0x10, 1, copy.clone())], "Duplicate").unwrap();
        assert_eq!(placed, vec![(0x10, 1)]);
        assert_eq!(s.entry(0x10).unwrap().objects().len(), 3);
        assert!(s.insert_objects(vec![(0x10, 0, copy)], "Duplicate").is_err(), "a duplicate tag is refused");
        assert_eq!(s.remove_objects(&[(0x10, 2)]).unwrap(), 1);
        assert_eq!(s.entry(0x10).unwrap().objects().iter().map(|o| o.tag()).collect::<Vec<_>>(), vec![0xA, tag]);
        assert_eq!(s.group_name(s.groups()[1]), "Delete");

        s.note_name(tag, r"Some\Path\Thing_1");
        let dir = std::env::temp_dir().join(format!("sab_struct_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        s.save(&dir.join("p.json")).unwrap();
        let r = Session::load_with(sources(), Path::new("G"), &read_project(&dir.join("p.json")).unwrap()).unwrap();
        assert_eq!(r.journal().0, s.journal().0);
        assert_eq!(r.object_name(tag).as_deref(), Some(r"Some\Path\Thing_1"), "minted names survive reopen");
        assert_eq!(r.group_name(r.groups()[0]), "Duplicate");
        let out = s.stage(&dir.join("stage")).unwrap();
        assert!(out.all_pass, "{}", out.report);
        assert!(out.report.contains("structural or size-changing edits"), "{}", out.report);
        let staged = Pack::parse(&std::fs::read(dir.join("stage").join("EditNodes.pack")).unwrap()).unwrap();
        assert_eq!(staged.entry(0x10).unwrap().objects().iter().map(|o| o.tag()).collect::<Vec<_>>(), vec![0xA, tag]);
        std::fs::remove_dir_all(&dir).ok();

        assert!(s.undo().unwrap() && s.undo().unwrap());
        assert_eq!(s.entry(0x10).unwrap().objects().len(), 2, "back to the retail container");
    }

    #[test]
    fn a_lua_param_is_edited_by_occurrence_and_a_resize_is_flagged() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        s.set_lua_param(0x10, 0, 1, editnodes::LuaValue::Bool(0)).unwrap();
        let o = &s.entry(0x10).unwrap().objects()[0];
        let leaves = o.params_with_tag(tags::LUA_PARAM);
        let read = |i: usize| match &o.params()[leaves[i]] {
            Node::Leaf { data, .. } => editnodes::lua_param(data).unwrap(),
            _ => unreachable!(),
        };
        assert_eq!((read(0).value, read(1).value), (editnodes::LuaValue::Bool(1), editnodes::LuaValue::Bool(0)), "only occurrence 1 changed");
        assert_eq!(s.group_name(s.groups()[0]), "Set b");
        assert!(s.active()[0].is_same_size_param());
        s.set_lua_param(0x10, 0, 0, editnodes::LuaValue::Str("hello".into())).unwrap();
        assert!(!s.active()[1].is_same_size_param(), "a resize is flagged");
        let dir = std::env::temp_dir().join(format!("sab_lua_{}", std::process::id()));
        let out = s.stage(&dir).unwrap();
        assert!(out.all_pass, "{}", out.report);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_write_guard_backups_and_validation_protect_the_user() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        assert!(s.save(Path::new("G/project.json")).is_err(), "never inside the game folder");
        assert!(crate::stage::guard_write(Path::new("tools/x/target/release/p.json"), Path::new("G")).is_err(), "never inside build output");
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            assert!(crate::stage::guard_write(&Path::new(&home).join("downloads").join("p.json"), Path::new("G")).is_err(), "never inside Downloads, any case");
        }
        let dir = std::env::temp_dir().join(format!("sab_guard_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("p.json");
        for k in 0..7 {
            s.move_object(0x10, 0, [k as f32, 2.0, 3.0]).unwrap();
            s.save(&p).unwrap();
        }
        let bak = |k: usize| PathBuf::from(format!("{}.bak{k}", p.display()));
        assert!(bak(1).is_file() && bak(BACKUPS).is_file() && !bak(BACKUPS + 1).is_file(), "five rotating backups");
        let newest = read_project(&bak(1)).unwrap();
        assert_eq!(newest["cursor"], 6, "bak1 is the previous save");
        std::fs::remove_dir_all(&dir).ok();

        assert!(s.validate().iter().all(|i| i.severity != Severity::Error), "moves are fine");
        let dup = s.entry(0x10).unwrap().objects()[0].clone();
        s.journal.push(Edit::Insert { container: 0x10, index: 2, node: dup });
        s.groups.push(99);
        s.cursor += 1;
        s.rebuild().unwrap();
        let issues = s.validate();
        assert!(issues.iter().any(|i| i.severity == Severity::Error && i.at.is_some()), "a new duplicate tag is an error with a go-to");
        assert!(!issues.iter().any(|i| i.severity == Severity::Note), "moves and inserts were observed in game");
        s.set_lua_param(0x10, 1, 0, editnodes::LuaValue::Bool(0)).unwrap();
        assert!(s.validate().iter().any(|i| i.severity == Severity::Note), "a LuaParam edit is noted as unverified");
        assert!(s.stage(&std::env::temp_dir().join("sab_never")).err().unwrap_or_default().contains("validation error"), "errors block staging");
    }

    #[test]
    fn an_agent_patch_is_one_undo_step() {
        let mut s = Session::from_sources(Path::new("G"), sources());
        let patch = json!({ "patch_id": "two", "operations": [
            { "action": "offset", "container": "10", "tag": "a", "delta": [1, 0, 0] },
            { "action": "offset", "container": "10", "tag": "b", "delta": [0, 1, 0] } ] });
        crate::agent::apply_patch(&mut s, &patch).unwrap();
        assert_eq!(s.active().len(), 2);
        assert_eq!(s.group_name(s.groups()[0]), "Agent patch two");
        assert!(s.undo().unwrap());
        assert_eq!(s.active().len(), 0, "one undo reverts the whole patch");
    }
}
