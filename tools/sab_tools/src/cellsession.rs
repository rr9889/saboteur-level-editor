//! Terrain-cell editing through the ordinary placement `Session` (EXPERIMENTAL: the caller gates it behind a feature flag).
//!
//! A cell's type-8 placement list is one EditNode entry body (`docs/formats/cell_placements.md`), so each list is loaded as a normal EditNodes container of a synthetic
//! session. Selection, move, undo and validation then work unchanged. Only the export differs: the session's net Position changes are written back into the cell
//! sub-pack in place (`cellplace::patch_in_place`, which also updates mirrored type-4 copies) and shipped as a new `France\patchmega{N}.megapack`.
//!
//! Scope, deliberately narrow (and enforced, not assumed):
//! * only same-size edits of existing objects inside their current cell: **Position** moves and **rotations** (the three basis leaves XAxis/YAxis/ZAxis);
//!   plus **insert** (duplicate) and **remove** (delete) of whole objects, which resize the list and take the REBUILD path below; any other edit (class, scale, LuaParam) is refused;
//! * the cell must be won by a base `Mega*` pack: a cell also held by the DLC pack (priority 100100), by `patchmega.megapack` or by any other patch pack is refused;
//! * the free patch slot must be higher in priority than every pack holding the cell, and its file must not exist yet;
//! * nothing is installed here: the output folder holds the staged pack plus hash-checked install/restore scripts (`stage::write_scripts`).
//! Verified in game: one slab moved 4 m (`docs/level_editor/e2b_terrain_cell_result.md`); rotation: see that file's rotation section.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sab_formats::editnodes::{self, tags, Entry, Node, Pack, Root};
use sab_formats::{cellplace, map6, megapack, sbla};
use serde_json::json;

use crate::cells::{cell_crc, france_mounts, free_patch_slot, resolve_entry, scan_cells, DLC_MEGAPACK, DLC_PRIORITY};
use crate::editnodes_stage::Edit;
use crate::session::Session;
use crate::stage::{diff_positions, ranges_cover, sha256, write, write_scripts, Dest, EditNodesSources};

/// The bytes of a placement list holding `objects`: `u32 0, u32 count, objects` (the framing of an EditNodes entry body; a one-entry pack is a 20-byte header plus that body).
pub fn list_body(objects: &[Node]) -> Vec<u8> {
    let p = Pack { entries: vec![Entry { hash: 0, root: Root::EditNode { objects: objects.to_vec() } }] }.write();
    p[20..].to_vec()
}

/// Shown in every terrain-cell export report and when a cell is opened. Basis:
/// `docs/level_editor/point4_resolution.md`: one baked-collision-only placement's +5 m X transform
/// shifted the obstruction +5.018 m in an A/B. Type-2 payloads and navigation were not rebuilt.
pub const COLLISION_CAVEAT: &str = "placement edits do NOT rebuild type-2 collision payloads or navigation. Moving a visual placement does NOT imply collision follows: invisible collision may remain, or the new location may have none. Baked-collision-only placement transforms are exportable: one cell/object A/B verified +5 m X -> +5.018 m stop shift; not a guarantee for all objects or axes";

/// The synthetic EditNodes container hash of placement list `li` of `cell` (never a real EditNodes container).
pub fn list_container(cell: u32, li: usize) -> u32 {
    cell ^ 0xCE11_A000 ^ (li as u32)
}

/// An opened cell: what the export needs besides the session.
pub struct CellEdit {
    pub cell: u32,
    /// The pack that wins the cell today, and its hash when the cell was opened.
    pub source_rel: String,
    pub source_sha: String,
    sub: Vec<u8>,
    layout: cellplace::Cell,
    /// `containers[li]` is the synthetic container of list `li`.
    pub containers: Vec<u32>,
}

impl CellEdit {
    pub fn list_layer(&self, list: usize) -> Option<u32> {
        self.layout.lists.get(list).map(|placement_list| placement_list.f14)
    }

    /// (list index, object index) of the object with `tag`, if exactly one list holds it exactly once.
    pub fn find(&self, tag: u32) -> Option<(usize, usize)> {
        let hits: Vec<(usize, usize)> = self.layout.lists.iter().enumerate().flat_map(|(li, l)| l.objects.iter().enumerate().filter(move |(_, o)| o.tag == tag).map(move |(i, _)| (li, i))).collect();
        (hits.len() == 1).then(|| hits[0])
    }
}

/// Checks shared by open and export: who holds this cell, and may we edit it? Returns the winning mount's name and the mounts' hashes.
fn ownership(game: &Path, cell: u32) -> Result<(String, String, Vec<u8>), String> {
    let mounts = france_mounts(game)?;
    let crc = cell_crc(cell);
    let owners: Vec<_> = mounts.iter().filter(|m| m.find(crc).is_some()).collect();
    let (winner, _) = resolve_entry(&mounts, crc).ok_or_else(|| format!("no mounted pack holds cell {cell}"))?;
    if let Some(unknown) = owners.iter().find(|m| m.priority.is_none()) {
        return Err(format!("{} also holds cell {cell} at an unestablished priority; refusing", unknown.rel));
    }
    if owners.iter().any(|m| m.rel == DLC_MEGAPACK) {
        return Err(format!("{DLC_MEGAPACK} holds cell {cell} at priority {DLC_PRIORITY}, above every France patch slot; refusing DLC authoring"));
    }
    if owners.len() > 1 {
        return Err(format!("cell {cell} is held by several packs ({}); refusing, the winner is not the only copy", owners.iter().map(|m| m.rel.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if !winner.rel.contains(r"\Mega") {
        return Err(format!("cell {cell} is won by {}, a patch pack (another mod's, or the retail patchmega.megapack): a new patch pack cannot override it; refusing", winner.rel));
    }
    let e = *winner.find(crc).unwrap();
    let sub = megapack::entry_slice(&winner.map, &e)?.to_vec();
    Ok((winner.rel.clone(), sha256(&winner.map), sub))
}

/// The cell's edit state and the synthetic EditNodes sources of its placement lists (deterministic for a given install).
fn cell_parts(game: &Path, cell: u32) -> Result<(CellEdit, EditNodesSources), String> {
    let (source_rel, source_sha, sub) = ownership(game, cell)?;
    let layout = cellplace::read(&sub)?.ok_or_else(|| format!("cell {cell} has no placement list"))?;
    let mut entries = Vec::new();
    let mut containers = Vec::new();
    for (li, l) in layout.lists.iter().enumerate() {
        let blob = &sub[l.loc.off..l.loc.off + l.loc.comp];
        let objects = editnodes::parse_entry(blob).map_err(|e| format!("cell {cell} list {li}: {e}"))?;
        if objects.len() != l.objects.len() || objects.iter().zip(&l.objects).any(|(n, o)| n.tag() != o.tag) {
            return Err(format!("cell {cell} list {li}: the EditNode parse and the cell index disagree; refusing"));
        }
        if list_body(&objects) != blob {
            return Err(format!("cell {cell} list {li} does not re-serialize byte-exactly; refusing (the rebuild path would corrupt it)"));
        }
        let hash = list_container(cell, li);
        containers.push(hash);
        entries.push(Entry { hash, root: Root::EditNode { objects } });
    }
    let src = EditNodesSources::from_bytes(Pack { entries }.write(), Pack { entries: vec![] }.write(), format!("terrain cell {cell}"))?;
    Ok((CellEdit { cell, source_rel, source_sha, sub, layout, containers }, src))
}

/// Open one terrain cell as an editable session. Fails (never guesses) when the cell is not editable under the scope above.
pub fn open_cell(game: &Path, cell: u32) -> Result<(CellEdit, Session), String> {
    let (edit, src) = cell_parts(game, cell)?;
    Ok((edit, Session::from_sources(game, src)))
}

/// Save the session as a project: the ordinary project journal plus a `terrain_cell` record (cell id and the winning pack's hash).
/// An ordinary `Session::load` refuses such a file (its inputs are synthetic); use [`open_cell_project`].
pub fn save_cell_project(edit: &CellEdit, s: &Session, path: &Path) -> Result<(), String> {
    let mut v = s.to_project_json();
    v["terrain_cell"] = json!({ "cell": edit.cell, "source": edit.source_rel, "source_sha256": edit.source_sha });
    let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The cell a project file edits, if it is a terrain-cell project.
pub fn cell_project_id(path: &Path) -> Option<u32> {
    let v = crate::session::read_project(path).ok()?;
    v["terrain_cell"]["cell"].as_u64().map(|c| c as u32)
}

/// Reopen a terrain-cell project: rebuild the cell's sources, refuse if the pack holding the cell changed since the save, replay the journal (and its undo cursor).
pub fn open_cell_project(path: &Path) -> Result<(CellEdit, Session), String> {
    let v = crate::session::read_project(path)?;
    let tc = v.get("terrain_cell").ok_or("not a terrain-cell project")?;
    let cell = tc["cell"].as_u64().ok_or("project has no cell id")? as u32;
    let game = PathBuf::from(v["game_dir"].as_str().ok_or("project has no game_dir")?);
    let (edit, src) = cell_parts(&game, cell)?;
    if tc["source_sha256"].as_str() != Some(edit.source_sha.as_str()) || tc["source"].as_str() != Some(edit.source_rel.as_str()) {
        return Err(format!("the pack holding cell {cell} changed since this project was saved (was {} {}, now {} {}); refusing", tc["source"], tc["source_sha256"], edit.source_rel, edit.source_sha));
    }
    let s = Session::load_with(src, &game, &v)?;
    Ok((edit, s))
}

#[derive(Debug)]
pub struct CellStageOutcome {
    pub out_dir: PathBuf,
    pub report: String,
    pub all_pass: bool,
    pub artifact_sha: String,
    pub slot_name: String,
    pub moved: usize,
}

/// The 12-byte leaves a cell export may change in place: Position and the rotation basis.
const LEAVES: [u32; 4] = [tags::POSITION, tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS];

/// Stage the session's edits as a France patch megapack in `out` (created). Writes nothing outside `out`.
/// Same-size edits (moves, rotations) patch the cell in place; inserted or removed objects rebuild the changed lists.
pub fn stage_cell(game: &Path, edit: &CellEdit, s: &Session, out: &Path) -> Result<CellStageOutcome, String> {
    for e in s.active() {
        match e {
            Edit::Param(p) if LEAVES.contains(&p.param_tag) => {}
            Edit::Insert { .. } | Edit::Remove { .. } => {}
            _ => return Err("terrain-cell export supports Position moves, rotations, duplicates and deletions only; undo the other edits (class, parameters) first".into()),
        }
    }
    let finals: Vec<&[Node]> = edit.containers.iter().map(|c| s.entry(*c).map(|e| e.objects()).ok_or("the session lost a placement list")).collect::<Result<_, _>>()?;
    let resized = edit.layout.lists.iter().enumerate().any(|(li, l)| finals[li].len() != l.objects.len() || finals[li].iter().zip(&l.objects).any(|(n, o)| n.tag() != o.tag));
    if resized { stage_rebuild(game, edit, &finals, out, &[]) } else { stage_in_place(game, edit, &finals, out) }
}

/// [`stage_cell`] plus whole-record payload replacements in the same sub-pack (for example a rebuilt
/// baked-collision payload that follows the session's building edits). Only the rebuild path supports
/// this; each replaced record must be a type-2 record that the caller produced and verified.
pub fn stage_cell_with_records(game: &Path, edit: &CellEdit, s: &Session, out: &Path, records: &[(usize, Vec<u8>)]) -> Result<CellStageOutcome, String> {
    for e in s.active() {
        match e {
            Edit::Param(p) if LEAVES.contains(&p.param_tag) => {}
            Edit::Insert { .. } | Edit::Remove { .. } => {}
            _ => return Err("terrain-cell export supports Position moves, rotations, duplicates and deletions only".into()),
        }
    }
    let finals: Vec<&[Node]> = edit.containers.iter().map(|c| s.entry(*c).map(|e| e.objects()).ok_or("the session lost a placement list")).collect::<Result<_, _>>()?;
    stage_rebuild(game, edit, &finals, out, records)
}

fn stage_in_place(game: &Path, edit: &CellEdit, finals: &[&[Node]], out: &Path) -> Result<CellStageOutcome, String> {
    // net changes: current leaf bytes vs the cell's bytes
    let mut changes: Vec<(usize, usize, Vec<u8>, Vec<u8>)> = Vec::new(); // (list, data_off, old, new)
    for (li, l) in edit.layout.lists.iter().enumerate() {
        let objs = finals[li];
        for (i, o) in l.objects.iter().enumerate() {
            for tag in LEAVES {
                let locs: Vec<_> = o.params.iter().filter(|p| p.tag == tag).collect();
                let cur = objs[i].param(tag);
                match (locs.as_slice(), cur) {
                    ([p], Some(cur)) => {
                        let old = &edit.sub[edit.layout.lists[li].loc.off + p.data_off..edit.layout.lists[li].loc.off + p.data_off + p.len];
                        if old != cur {
                            changes.push((li, p.data_off, old.to_vec(), cur.to_vec()));
                        }
                    }
                    ([], None) => {}
                    _ => return Err(format!("list {li} object {i}: {} is missing or ambiguous; refusing", editnodes::tag_name(tag).unwrap_or("leaf"))),
                }
            }
        }
    }
    if changes.is_empty() {
        return Err("nothing to export: no object has a different Position or orientation".into());
    }
    let mut report = String::new();
    let _ = writeln!(report, "terrain cell {} (crc {:08x}, path {}.pack), won by {}; in-place patch", edit.cell, cell_crc(edit.cell), map6::streamblock_path("France", edit.cell), edit.source_rel);
    let mut patched = edit.sub.clone();
    let mut written: Vec<(usize, usize)> = Vec::new();
    for (li, off, old, new) in &changes {
        let p = cellplace::patch_in_place(&patched, &edit.layout, *li, *off, old, new)?;
        written.extend(p.written_at.iter().map(|&w| (w, new.len())));
        patched = p.bytes;
        let f = |b: &Vec<u8>| crate::cells::vec3(b);
        let _ = writeln!(report, "list {li} leaf @ {off}: {:?} -> {:?}", f(old), f(new));
    }
    let diffs = diff_positions(&edit.sub, &patched);
    let b_ok = patched.len() == edit.sub.len() && !diffs.is_empty() && ranges_cover(&diffs, &written);
    let again = cellplace::read(&patched)?.ok_or("the patched cell lost its list")?;
    let b_ok = b_ok && again.prefixes.iter().map(|p| p.mirrors).collect::<Vec<_>>() == edit.layout.prefixes.iter().map(|p| p.mirrors).collect::<Vec<_>>();
    let _ = writeln!(report, "(b) {} differing bytes, all inside the written ranges, type-4 mirror relations unchanged: {b_ok}", diffs.len());
    ship(game, edit, patched, report, b_ok, changes.len(), out)
}

fn stage_rebuild(game: &Path, edit: &CellEdit, finals: &[&[Node]], out: &Path, records: &[(usize, Vec<u8>)]) -> Result<CellStageOutcome, String> {
    let sp = sbla::parse(&edit.sub, None)?;
    let mut blobs = sp.blobs();
    let mut report = String::new();
    let _ = writeln!(report, "terrain cell {} (crc {:08x}, path {}.pack), won by {}; REBUILD of resized/changed lists", edit.cell, cell_crc(edit.cell), map6::streamblock_path("France", edit.cell), edit.source_rel);
    let mut bodies: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut inserted: Vec<u32> = Vec::new();
    let mut moved = 0usize;
    for (li, l) in edit.layout.lists.iter().enumerate() {
        let body = list_body(finals[li]);
        let orig = &edit.sub[l.loc.off..l.loc.off + l.loc.comp];
        if body == orig {
            continue;
        }
        if edit.layout.prefixes.iter().any(|p| p.mirrors == Some(li)) {
            return Err(format!("list {li} is mirrored by a type-4 record; resizing it would let the copies diverge; refusing"));
        }
        let old_tags: Vec<u32> = l.objects.iter().map(|o| o.tag).collect();
        let new_tags: Vec<u32> = finals[li].iter().map(|n| n.tag()).collect();
        let removed: Vec<u32> = old_tags.iter().copied().filter(|t| !new_tags.contains(t)).collect();
        let added: Vec<u32> = new_tags.iter().copied().filter(|t| !old_tags.contains(t)).collect();
        let _ = writeln!(report, "list {li}: {} -> {} objects; removed {:08x?}; inserted {:08x?}; {} -> {} bytes", old_tags.len(), new_tags.len(), removed, added, orig.len(), body.len());
        inserted.extend(&added);
        moved += removed.len() + added.len() + 1;
        bodies.push((li, body));
    }
    if bodies.is_empty() {
        return Err("nothing to export".into());
    }
    // identifiers of new objects must be unique across every base cell and the EditNodes packs
    if !inserted.is_empty() {
        let mounts = france_mounts(game)?;
        let (_, counts) = scan_cells(&mounts)?;
        let en = Session::open(game)?;
        let mut en_tags = std::collections::HashSet::new();
        for c in en.containers() {
            if let Some(e) = en.entry(c.hash) {
                en_tags.extend(e.objects().iter().map(|o| o.tag()));
            }
        }
        for t in &inserted {
            if counts.get(t).copied().unwrap_or(0) > 0 || en_tags.contains(t) {
                return Err(format!("the new object tag {t:08x} already exists elsewhere in the game's placements; refusing (re-duplicate to mint another)"));
            }
        }
        let _ = writeln!(report, "new tags {:08x?} collide with none of the {} base-cell tags or the EditNodes objects", inserted, counts.len());
    }
    for (li, body) in &bodies {
        let l = &edit.layout.lists[*li];
        let b = &mut blobs[l.loc.record];
        b.data = body;
        b.uncomp = body.len() as u32;
    }
    // whole-record replacements (e.g. rebuilt baked collision), stored the way the original record was
    let mut stored: Vec<(usize, Vec<u8>)> = Vec::new();
    for (ri, payload) in records {
        let r = sp.records.get(*ri).ok_or_else(|| format!("record {ri} does not exist"))?;
        if r.kind != 2 {
            return Err(format!("record {ri} is type {}, only type-2 payload replacement is supported", r.kind));
        }
        if bodies.iter().any(|(li, _)| edit.layout.lists[*li].loc.record == *ri) {
            return Err(format!("record {ri} is also a placement list"));
        }
        let data = if r.comp == r.uncomp { payload.clone() } else { sab_formats::zlib::deflate(payload, 9) };
        if r.comp != r.uncomp && data.len() == payload.len() {
            return Err(format!("record {ri}: ambiguous raw/zlib length"));
        }
        stored.push((*ri, data));
    }
    for (ri, data) in &stored {
        blobs[*ri].data = data;
        blobs[*ri].uncomp = records.iter().find(|(r, _)| r == ri).unwrap().1.len() as u32;
        let _ = writeln!(report, "record {ri} (type 2) replaced: {} -> {} payload bytes, sha256 {}", sp.records[*ri].uncomp, blobs[*ri].uncomp, sha256(&records.iter().find(|(r, _)| r == ri).unwrap().1));
    }
    let patched = sbla::write(sp.descriptor.as_ref(), &blobs)?;
    // checks: the rebuilt cell parses; each list holds exactly the session's objects in order; every other blob is byte-identical
    let again = cellplace::read(&patched)?.ok_or("the rebuilt cell lost its lists")?;
    let mut ok = again.lists.len() == edit.layout.lists.len() && again.prefixes.iter().map(|p| p.mirrors).collect::<Vec<_>>() == edit.layout.prefixes.iter().map(|p| p.mirrors).collect::<Vec<_>>();
    for (li, l) in again.lists.iter().enumerate() {
        ok &= l.objects.len() == finals[li].len() && l.objects.iter().zip(finals[li]).all(|(o, n)| o.tag == n.tag());
        let blob = &patched[l.loc.off..l.loc.off + l.loc.comp];
        ok &= blob == list_body(finals[li]).as_slice();
    }
    let sp2 = sbla::parse(&patched, None)?;
    let mut changed_records: Vec<usize> = bodies.iter().map(|(li, _)| edit.layout.lists[*li].loc.record).collect();
    for (ri, payload) in records {
        changed_records.push(*ri);
        let rr = &sp2.records[*ri];
        let back = if rr.comp == rr.uncomp { sp2.blobs[*ri].to_vec() } else { sab_formats::zlib::inflate_exact(sp2.blobs[*ri], rr.uncomp as usize, "replaced record")? };
        ok &= back == *payload && rr.kind == 2 && rr.hash == sp.records[*ri].hash;
    }
    for (i, (a, b)) in sp.blobs.iter().zip(sp2.blobs.iter()).enumerate() {
        if !changed_records.contains(&i) {
            ok &= a == b;
        }
    }
    ok &= sp.records.len() == sp2.records.len();
    let _ = writeln!(report, "(b) rebuilt sub-pack parses; every list holds exactly the session's objects in order; all {} unchanged blobs are byte-identical: {ok}", sp.records.len() - changed_records.len());
    ship(game, edit, patched, report, ok, moved, out)
}

/// Common tail: re-check ownership and the slot, write the megapack, verify it round-trips, write the scripts.
fn ship(game: &Path, edit: &CellEdit, patched: Vec<u8>, mut report: String, checks_ok: bool, moved: usize, out: &Path) -> Result<CellStageOutcome, String> {
    let (rel, sha, sub_now) = ownership(game, edit.cell)?;
    if rel != edit.source_rel || sha != edit.source_sha || sub_now != edit.sub {
        return Err("the pack holding this cell changed since it was opened; reopen the cell".into());
    }
    let mounts = france_mounts(game)?;
    let crc = cell_crc(edit.cell);
    let e = *mounts.iter().find(|m| m.rel == rel).and_then(|m| m.find(crc)).ok_or("the winning entry disappeared")?;
    let (slot_name, slot_prio) = free_patch_slot(game)?;
    if let Some(shadow) = mounts.iter().find(|m| m.priority.map_or(true, |p| p >= slot_prio) && m.find(crc).is_some()) {
        return Err(format!("{slot_name} (priority {slot_prio}) cannot override {} (priority {:?}): the staged patch must be strictly higher priority; refusing", shadow.rel, shadow.priority));
    }
    let dest_rel = format!(r"France\{slot_name}");
    if game.join(&dest_rel).is_file() {
        return Err(format!("{dest_rel} already exists"));
    }
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let sp = sbla::parse(&edit.sub, None)?;
    let a_ok = sbla::write(sp.descriptor.as_ref(), &sp.blobs())? == edit.sub;
    let mp = megapack::write(&[megapack::Item { crc: e.crc, index: e.index, data: &patched }]);
    let parsed = megapack::parse(&mp)?;
    let c_ok = parsed.entries.len() == 1 && parsed.entries[0].crc == e.crc && parsed.entries[0].index == e.index && megapack::entry_slice(&mp, &parsed.entries[0])? == patched.as_slice();
    let _ = writeln!(report, "(a) source sub-pack re-serializes byte-identically: {a_ok}");
    let _ = writeln!(report, "(c) {slot_name} (priority {slot_prio}): 1 entry crc {:08x} index {} holding the patched sub-pack: {c_ok}", e.crc, e.index);
    let _ = writeln!(report, "NOTE: {COLLISION_CAVEAT}");
    let all = a_ok && checks_ok && c_ok;
    let artifact_sha = sha256(&mp);
    write(out, &slot_name, &mp)?;
    let dest = Dest { rel: dest_rel, existed: false, original_sha: None, staged_name: slot_name.clone(), installed_sha: artifact_sha.clone() };
    let mut pre = vec![(rel.clone(), sha.clone())];
    if slot_name != "patchmega.megapack" {
        for m in mounts.iter().filter(|m| m.rel.contains("patchmega")) {
            pre.push((m.rel.clone(), sha256(&m.map)));
        }
    }
    let prov = json!({ "cell": edit.cell, "crc": format!("{crc:08x}"), "source": rel, "source_sha256": sha, "edits": moved,
                       "patch_slot": slot_name, "patch_priority": slot_prio, "static_checks_pass": all, "authored_by": "sab_tools::cellsession" });
    write_scripts(out, game, "cellws", &[dest], &pre, prov)?;
    let _ = writeln!(report, "ALL STATIC CHECKS PASS: {all}");
    write(out, "report.txt", report.as_bytes())?;
    Ok(CellStageOutcome { out_dir: out.to_path_buf(), report, all_pass: all, artifact_sha, slot_name, moved })
}

#[cfg(test)]
mod caveat_tests {
    use super::COLLISION_CAVEAT;

    #[test]
    fn the_collision_caveat_distinguishes_payload_rebuilding_from_verified_baked_transforms() {
        let c = COLLISION_CAVEAT.to_ascii_lowercase();
        assert!(c.contains("do not rebuild type-2 collision payloads or navigation"), "{c}");
        assert!(c.contains("visual placement does not imply collision follows"), "{c}");
        assert!(c.contains("invisible collision") && c.contains("may have none"), "both failure directions are stated: {c}");
        assert!(c.contains("baked-collision-only placement transforms are exportable"), "{c}");
        assert!(c.contains("one cell/object a/b verified +5 m x -> +5.018 m stop shift"), "{c}");
        assert!(c.contains("not a guarantee for all objects or axes"), "{c}");
    }
}
