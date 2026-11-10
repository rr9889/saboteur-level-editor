//! Level-editor milestone M0: stage two reversible world edits for a user-run in-game test.
//!
//! * E1 moves one EditNodes object through the DLC slot's EditNodes pack, which the engine searches
//!   before the base pack per entry hash (`docs/formats/editnodes.md`, DLC override).
//! * E2 moves one terrain-cell placement through a France patch megapack, mounted as
//!   `sab_tools::cells` describes; a cell the DLC megapack holds is refused (its priority is not
//!   established).

use std::fmt::Write as _;
use std::path::Path;

use sab_formats::editnodes::{self, tags, Node};
use sab_formats::{cellplace, map6, megapack, sbla};
use sab_tools::cells::{cell_crc, cstr, france_mounts, free_patch_slot, scan_cells, vec3, Mount, Placement, DLC_MEGAPACK};
use sab_tools::editnodes_stage::{stage_editnodes, Edit, ParamEdit};
use sab_tools::stage::{diff_positions, ranges_cover, sha256, write, write_scripts, Dest, EditNodesSources};
use serde_json::json;

pub fn run(args: &[String]) -> Result<(), String> {
    let game = sab_formats::game_paths::retail_dir(&std::env::current_dir().map_err(|e| e.to_string())?)?;
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["survey-cells", x, y, z, r] => survey_cells(&game, [f(x)?, f(y)?, f(z)?], f(r)?),
        ["survey-editnodes"] => survey_editnodes(&game, None),
        ["survey-editnodes", q] => survey_editnodes(&game, Some(q)),
        ["stage-e1", c, t, dx, dy, dz, out] => stage_e1(&game, hex(c)?, hex(t)?, [f(dx)?, f(dy)?, f(dz)?], Path::new(out)),
        ["stage-e1b", out] => stage_e1b(&game, Path::new(out)),
        ["stage-e2", id, t, dx, dy, dz, out] => {
            let id = id.parse::<u32>().map_err(|e| format!("cell id {id}: {e}"))?;
            stage_e2(&game, id, hex(t)?, [f(dx)?, f(dy)?, f(dz)?], Path::new(out))
        }
        _ => Err(super::USAGE.to_string()),
    }
}

fn f(s: &str) -> Result<f32, String> {
    s.parse::<f32>().map_err(|e| format!("{s}: {e}"))
}

fn hex(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("{s}: {e}"))
}

fn vec3_bytes(v: [f32; 3]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

// ------------------------------------------------------------------ surveys

fn survey_cells(game: &Path, at: [f32; 3], radius: f32) -> Result<(), String> {
    let mounts = france_mounts(game)?;
    let (all, counts) = scan_cells(&mounts)?;
    println!("{} placements with a Position in base terrain cells; {} distinct tags", all.len(), counts.len());
    let mut near: Vec<&Placement> = all.iter().filter(|p| dist(p.pos, at) <= radius).collect();
    near.sort_by(|a, b| dist(a.pos, at).total_cmp(&dist(b.pos, at)));
    println!("dist  cell      pack                  tag       list(f10,f14)        nth occ  params lua prefix type / class / position / cell owner");
    for p in near {
        let crc = cell_crc(p.cell);
        let owners: Vec<String> = mounts.iter().filter(|m| m.find(crc).is_some()).map(|m| m.rel.clone()).collect();
        println!(
            "{:5.1} {:<9} {:<21} {:08x} {}/{}({:x},{:x}) {:>3} {:>3} {:>6} {:>3} {:<6} {} / {} / ({:.2}, {:.2}, {:.2}) / {}",
            dist(p.pos, at),
            p.cell,
            p.pack,
            p.tag,
            p.list,
            p.lists_in_cell,
            p.list_f10,
            p.list_f14,
            p.nth_in_cell,
            counts[&p.tag],
            p.params,
            p.lua_params,
            p.in_prefix,
            p.type_name,
            p.class_name,
            p.pos[0],
            p.pos[1],
            p.pos[2],
            owners.join(" > ")
        );
    }
    Ok(())
}

fn editnodes_sources(game: &Path) -> Result<EditNodesSources, String> {
    EditNodesSources::open(game)
}

fn object_line(o: &Node) -> String {
    let name = editnodes::tag_name(o.tag()).unwrap_or("?");
    let pos = o.position().map(|p| format!("({:.2}, {:.2}, {:.2})", p[0], p[1], p[2])).unwrap_or_default();
    format!(
        "  {:08x} {:<40} {:<12} {:<36} {} params={}",
        o.tag(),
        name,
        o.type_name().unwrap_or_default(),
        o.class_name().unwrap_or_default(),
        pos,
        o.params().len()
    )
}

fn survey_editnodes(game: &Path, q: Option<&str>) -> Result<(), String> {
    let s = editnodes_sources(game)?;
    println!(
        "base EditNodes (loosefiles, sha256 {}): {} entries; DLC/01 EditNodes (sha256 {}): {} entries",
        s.loosefiles_sha,
        s.base.entries.len(),
        sha256(&s.dlc_bytes),
        s.dlc.entries.len()
    );
    for e in &s.dlc.entries {
        println!("DLC entry {:08x} {}", e.hash, editnodes::tag_name(e.hash).unwrap_or("?"));
    }
    if let Some(q) = q.filter(|q| q.starts_with("0x")) {
        let h = hex(q)?;
        let (src, e) = match s.dlc.entry(h) {
            Some(e) => ("DLC/01 (wins)", e),
            None => ("base", s.base.entry(h).ok_or_else(|| format!("container {h:08x} not found"))?),
        };
        println!("container {h:08x} {} from {src}: {} objects", editnodes::tag_name(h).unwrap_or("?"), e.objects().len());
        for o in e.objects() {
            println!("{}", object_line(o));
        }
        return Ok(());
    }
    let q = q.map(str::to_ascii_lowercase);
    for e in &s.base.entries {
        let name = editnodes::tag_name(e.hash).unwrap_or("?");
        if q.as_ref().map_or(true, |q| name.to_ascii_lowercase().contains(q.as_str())) {
            let shadow = if s.dlc.entry(e.hash).is_some() { " [shadowed by DLC/01]" } else { "" };
            println!("{:08x} {:>4} objects  {name}{shadow}", e.hash, e.objects().len());
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ staging

/// E1: move one EditNodes object by `delta` through the shared EditNodes staging path
/// ([`sab_tools::editnodes_stage::stage_editnodes`], the path the editor uses too).
fn stage_e1(game: &Path, container: u32, tag: u32, delta: [f32; 3], out: &Path) -> Result<(), String> {
    let s = editnodes_sources(game)?;
    let (_, entry) = s.winner(container).ok_or_else(|| format!("container {container:08x} not found"))?;
    let hits: Vec<usize> = entry.objects().iter().enumerate().filter(|(_, o)| o.tag() == tag).map(|(i, _)| i).collect();
    let [idx] = hits[..] else { return Err(format!("object tag {tag:08x}: {} matches in the container; exactly one required", hits.len())) };
    let object = &entry.objects()[idx];
    let positions = object.params_with_tag(tags::POSITION);
    let [_] = positions[..] else { return Err(format!("object has {} Position params; exactly one required", positions.len())) };
    let old_bytes = object.param(tags::POSITION).unwrap().to_vec();
    let old = vec3(&old_bytes).ok_or("Position is not 12 bytes")?;
    let new = [old[0] + delta[0], old[1] + delta[1], old[2] + delta[2]];
    println!("E1: object {tag:08x} ({}) {} / {}", editnodes::tag_name(tag).unwrap_or("?"), object.type_name().unwrap_or_default(), object.class_name().unwrap_or_default());
    println!("Position {old:?} -> {new:?}");
    let edit = ParamEdit { container, object_index: idx, object_tag: tag, param_tag: tags::POSITION, occurrence: 0, old: old_bytes, new: vec3_bytes(new) };
    let r = stage_editnodes(game, &s, &[Edit::Param(edit)], out, "m0e1", json!({ "experiment": "E1", "old_position": old, "new_position": new }))?;
    print!("{}", r.report);
    if r.all_pass { Ok(()) } else { Err("a static check failed; do not install".into()) }
}

/// Gate A (E1b): one DLC/01 package with the editor's structural edits on always-loaded exterior
/// blip locators (`InteriorManager.LoadAllExteriorBlips` loads every interior's blip node at game
/// load), staged through the editor's own session:
/// * duplicate: a copy of `LOC_LV_Blip_Ext` named `LOC_LV_Blip_Ext_1`, 3 m east, keeping the
///   original basis;
/// * rotate: `LOC_LV_Blip_Ext` turned +90° in yaw about its own position (the unrotated copy is the
///   in-session reference for `Object.GetAngle`, whose convention is not established);
/// * delete: `LOC_Saar_Blip_Ext` replaced by a copy `LOC_Saar_Blip_Ext_1` at the same spot, so the
///   container never becomes empty. Every script use of that name is nil-guarded
///   (`InteriorManager.lua:632-692`, warning branches).
/// Writes `expect.json` with the values the harness judges against.
fn stage_e1b(game: &Path, out: &Path) -> Result<(), String> {
    use sab_tools::session::Session;
    const LV: &str = r"PARIS\area01\lavillette\interior\lavillette_ext\ext_blip_loc";
    const SAAR: &str = r"CountrySide\alsace\town\interior\int_saar_hotel_ext\ext_blip_loc";
    let mut s = Session::open(game)?;
    let find = |s: &Session, path: &str, name: &str| -> Result<(u32, usize, u32), String> {
        let c = editnodes::container_hash(path);
        let t = editnodes::object_tag(path, name);
        let e = s.entry(c).ok_or_else(|| format!("container {path} not found"))?;
        let hits: Vec<usize> = e.objects().iter().enumerate().filter(|(_, o)| o.tag() == t).map(|(i, _)| i).collect();
        let [i] = hits[..] else { return Err(format!("{path}\\{name}: {} matches, exactly one required", hits.len())) };
        Ok((c, i, t))
    };
    let (lv_c, lv_i, _) = find(&s, LV, "LOC_LV_Blip_Ext")?;
    let (sa_c, sa_i, _) = find(&s, SAAR, "LOC_Saar_Blip_Ext")?;
    let lv_obj = s.entry(lv_c).unwrap().objects()[lv_i].clone();
    let sa_obj = s.entry(sa_c).unwrap().objects()[sa_i].clone();
    let lv_pos = lv_obj.position().ok_or("LOC_LV_Blip_Ext has no Position")?;
    let sa_pos = sa_obj.position().ok_or("LOC_Saar_Blip_Ext has no Position")?;

    let mut lv_copy = lv_obj.clone();
    lv_copy.set_tag(editnodes::object_tag(LV, "LOC_LV_Blip_Ext_1"));
    let copy_pos = [lv_pos[0] + 3.0, lv_pos[1], lv_pos[2]];
    lv_copy.set_position(copy_pos)?;
    let lv_len = s.entry(lv_c).unwrap().objects().len();
    s.insert_objects(vec![(lv_c, lv_len, lv_copy)], "Duplicate")?;
    let r90 = [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]];
    s.rotate_objects(&[(lv_c, lv_i)], r90, None)?;

    let mut sa_copy = sa_obj.clone();
    sa_copy.set_tag(editnodes::object_tag(SAAR, "LOC_Saar_Blip_Ext_1"));
    let sa_len = s.entry(sa_c).unwrap().objects().len();
    s.insert_objects(vec![(sa_c, sa_len, sa_copy)], "Duplicate")?;
    s.remove_objects(&[(sa_c, sa_i)])?;

    let r = s.stage(out)?;
    let expect = json!({
        "experiment": "E1b",
        "blip": { "name": format!("{LV}\\LOC_LV_Blip_Ext"), "position": lv_pos, "rotated_yaw_deg": 90.0 },
        "copy": { "name": format!("{LV}\\LOC_LV_Blip_Ext_1"), "position": copy_pos },
        "deleted": { "name": format!("{SAAR}\\LOC_Saar_Blip_Ext") },
        "replacement": { "name": format!("{SAAR}\\LOC_Saar_Blip_Ext_1"), "position": sa_pos },
        "artifact_sha256": r.artifact_sha,
    });
    std::fs::write(out.join("expect.json"), serde_json::to_string_pretty(&expect).unwrap()).map_err(|e| e.to_string())?;
    print!("{}", r.report);
    if r.all_pass { Ok(()) } else { Err("a static check failed; do not install".into()) }
}

fn stage_e2(game: &Path, id: u32, tag: u32, delta: [f32; 3], out: &Path) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let mounts = france_mounts(game)?;
    let crc = cell_crc(id);
    let mut report = String::new();
    let owners: Vec<&Mount> = mounts.iter().filter(|m| m.find(crc).is_some()).collect();
    let _ = writeln!(report, "E2: cell {id} (crc {crc:08x}, path {}.pack)", map6::streamblock_path("France", id));
    let _ = writeln!(report, "mounted owners, highest priority first: {}", owners.iter().map(|m| format!("{} ({:?})", m.rel, m.priority)).collect::<Vec<_>>().join(" > "));
    let winner = *owners.first().ok_or("no mounted pack holds this cell")?;
    if owners.iter().any(|m| m.priority.is_none()) {
        return Err(format!("{DLC_MEGAPACK} holds this cell at an unestablished priority; choose another cell"));
    }
    if !winner.rel.contains(r"\Mega") {
        return Err(format!("the winning copy is in {}, another mod's patch pack; refusing", winner.rel));
    }
    let e = *winner.find(crc).unwrap();
    let sub = megapack::entry_slice(&winner.map, &e)?;
    let cell = cellplace::read(sub)?.ok_or("cell has no placement list")?;
    let hits: Vec<(usize, usize)> = cell
        .lists
        .iter()
        .enumerate()
        .flat_map(|(li, l)| l.objects.iter().enumerate().filter(|(_, o)| o.tag == tag).map(move |(i, _)| (li, i)))
        .collect();
    let [(li, idx)] = hits[..] else { return Err(format!("tag {tag:08x}: {} matches across the cell's lists; exactly one required", hits.len())) };
    let list = &cell.lists[li];
    let blob = &sub[list.loc.off..list.loc.off + list.loc.comp];
    let o = &list.objects[idx];
    let pos: Vec<_> = o.params.iter().filter(|p| p.tag == tags::POSITION).collect();
    let [p] = pos[..] else { return Err(format!("object has {} Position params; exactly one required", pos.len())) };
    let old_bytes = blob[p.data_off..p.data_off + p.len].to_vec();
    let old = vec3(&old_bytes).ok_or("Position is not 12 bytes")?;
    let new = [old[0] + delta[0], old[1] + delta[1], old[2] + delta[2]];
    let get = |t: u32| o.params.iter().find(|q| q.tag == t).map(|q| cstr(&blob[q.data_off..q.data_off + q.len])).unwrap_or_default();
    let _ = writeln!(report, "object {tag:08x}: {} / {}; Position {old:?} -> {new:?}", get(tags::TYPE), get(tags::CLASS_NAME));
    let (_, counts) = scan_cells(&mounts)?;
    let _ = writeln!(report, "tag occurrences across all base terrain cells: {} (other copies are NOT edited)", counts.get(&tag).copied().unwrap_or(0));
    let _ = writeln!(report, "placement list {li} of {} (record {}, f10 {:#x}, f14 {:#x}), object {idx} of {}", cell.lists.len(), list.loc.record, list.f10, list.f14, list.objects.len());
    let _ = writeln!(report, "type-4 blobs (record, comp, uncomp, mirrors list): {:?}", cell.prefixes.iter().map(|p| (p.loc.record, p.loc.comp, p.loc.uncomp, p.mirrors)).collect::<Vec<_>>());

    let patched = cellplace::patch_in_place(sub, &cell, li, p.data_off, &old_bytes, &vec3_bytes(new))?;
    // (a) codec no-op reconstruction preserves the selected sub-pack.
    let sp = sbla::parse(sub, None)?;
    let a_ok = sbla::write(sp.descriptor.as_ref(), &sp.blobs())? == sub;
    let _ = writeln!(report, "(a) source sub-pack re-serializes byte-identically through sbla: {a_ok}");
    // (b) only the Position payload (and its mirrored type-4 copy) changed.
    let diffs = diff_positions(sub, &patched.bytes);
    let ranges: Vec<(usize, usize)> = patched.written_at.iter().map(|&w| (w, 12)).collect();
    let b_ok = patched.bytes.len() == sub.len() && !diffs.is_empty() && ranges_cover(&diffs, &ranges);
    let again = cellplace::read(&patched.bytes)?.ok_or("patched cell lost its list")?;
    let al = &again.lists[li];
    let re_pos = al.objects[idx].params.iter().find(|q| q.tag == tags::POSITION).map(|q| vec3(&patched.bytes[al.loc.off + q.data_off..al.loc.off + q.data_off + 12]));
    let mirrors_kept = again.prefixes.iter().map(|p| p.mirrors).collect::<Vec<_>>() == cell.prefixes.iter().map(|p| p.mirrors).collect::<Vec<_>>();
    let b_ok = b_ok && mirrors_kept;
    let _ = writeln!(report, "(b) {} differing bytes, all inside the written ranges {:?}; type-4 mirror relations unchanged {mirrors_kept}: {b_ok}; re-read Position {:?}", diffs.len(), patched.written_at, re_pos);
    // (c) the output container: one entry, the base crc/index, the patched bytes.
    let (slot_name, slot_prio) = free_patch_slot(game)?;
    if mounts.iter().any(|m| m.priority.map_or(false, |p| p > slot_prio) && m.find(crc).is_some()) {
        return Err("a higher-priority patch pack already holds this cell".into());
    }
    let mp = megapack::write(&[megapack::Item { crc: e.crc, index: e.index, data: &patched.bytes }]);
    let parsed = megapack::parse(&mp)?;
    let c_ok = parsed.entries.len() == 1
        && parsed.entries[0].crc == e.crc
        && parsed.entries[0].index == e.index
        && megapack::entry_slice(&mp, &parsed.entries[0])? == patched.bytes.as_slice();
    let _ = writeln!(report, "(c) {slot_name} (priority {slot_prio}, above every Mega pack): 1 entry crc {:08x} index {} holding the patched sub-pack: {c_ok}", e.crc, e.index);
    let noop = megapack::write(&[megapack::Item { crc: e.crc, index: e.index, data: sub }]);
    write(out, "validator_source_noop.megapack", &noop)?;
    let _ = writeln!(report, "(d) sab_validator: run on validator_source_noop.megapack (unchanged source) and on {slot_name}; results recorded in docs/level_editor/m0_log.md");
    let all = a_ok && b_ok && c_ok;
    write(out, &slot_name, &mp)?;
    let rel = format!(r"France\{slot_name}");
    let dest = Dest { rel: rel.clone(), existed: game.join(&rel).is_file(), original_sha: None, staged_name: slot_name.clone(), installed_sha: sha256(&mp) };
    if dest.existed {
        return Err(format!("{rel} already exists"));
    }
    let _ = writeln!(report, "(e) sha256: winning source {} {}; staged {slot_name} {}", winner.rel, sha256(&winner.map), dest.installed_sha);
    let _ = writeln!(report, "ALL STATIC CHECKS PASS: {all}");
    let prov = json!({ "cell": id, "crc": format!("{crc:08x}"), "object_tag": format!("{tag:08x}"), "source": winner.rel,
                       "source_sha256": sha256(&winner.map), "old_position": old, "new_position": new,
                       "patch_slot": slot_name, "patch_priority": slot_prio, "static_checks_pass": all });
    let mut pre = vec![(winner.rel.clone(), sha256(&winner.map))];
    if slot_name != "patchmega.megapack" {
        for m in mounts.iter().filter(|m| m.rel.contains("patchmega")) {
            pre.push((m.rel.clone(), sha256(&m.map)));
        }
    }
    write_scripts(out, game, "m0e2", &[dest], &pre, prov)?;
    write(out, "report.txt", report.as_bytes())?;
    print!("{report}");
    if all { Ok(()) } else { Err("a static check failed; do not install".into()) }
}
