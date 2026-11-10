//! Opt-in offline experiment: translate one visual placement and cut collision subset together.
//! This does not certify ownership, opaque acceleration data, or runtime compatibility.
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use sab_formats::collision::{self, Payload};
use sab_formats::editnodes::tags;
use sab_formats::{cellplace, megapack, sbla, zlib};
use serde_json::{json, Value};

use crate::collision_stage::{basis, delta, hex, ids, local_delta, number, text, unique_surface};
use crate::{cells, stage};

pub const SCHEMA: &str = "sab_collision_module_translation_request_v1";

#[derive(Debug)]
pub struct ModuleDiagnostic {
    pub pack: Vec<u8>,
    pub subpack: Vec<u8>,
    pub collision_payload: Vec<u8>,
    pub manifest: Value,
}

/// Package a separately compiled collision payload without accepting any collision-geometry
/// changes. The caller pins the reviewed compiler receipt and explicitly enumerates size-field
/// byte offsets (0x10c..0x110 only). This is still an offline experiment, not runtime acceptance.
pub fn package_replacement(
    diagnostic: &ModuleDiagnostic,
    replacement: &[u8],
    repair: &Value,
    compiler_receipt: &Value,
) -> Result<ModuleDiagnostic, String> {
    if text(repair, "schema")? != "sab_collision_module_repair_request_v1" {
        return Err("unsupported module repair schema".into());
    }
    if repair["compiler_receipt"] != *compiler_receipt {
        return Err("compiler receipt hash mismatch".into());
    }
    if compiler_receipt["verification"]["passed"].as_bool() != Some(true)
        || compiler_receipt["input_sha256"]
            .as_str()
            .map(str::to_ascii_uppercase)
            != Some(stage::sha256(&diagnostic.collision_payload))
        || compiler_receipt["output_sha256"]
            .as_str()
            .map(str::to_ascii_uppercase)
            != Some(stage::sha256(replacement))
    {
        return Err("compiler receipt verification or payload identity mismatch".into());
    }
    if stage::sha256(&diagnostic.collision_payload) != text(repair, "diagnostic_payload_sha256")?
        || stage::sha256(replacement) != text(repair, "replacement_payload_sha256")?
    {
        return Err("diagnostic/replacement payload hash mismatch".into());
    }
    let Payload::Baked(before_mesh) = collision::decode(&diagnostic.collision_payload)? else {
        return Err("diagnostic is not baked".into());
    };
    let Payload::Baked(after_mesh) = collision::decode(replacement)? else {
        return Err("replacement is not baked".into());
    };
    if before_mesh.triangles != after_mesh.triangles
        || before_mesh.vertices.len() != after_mesh.vertices.len()
        || before_mesh
            .vertices
            .iter()
            .zip(&after_mesh.vertices)
            .any(|(a, b)| a.map(f32::to_bits) != b.map(f32::to_bits))
        || before_mesh.padding != after_mesh.padding
    {
        return Err("repair changed collision vertices, triangles or alignment".into());
    }
    let allowed: BTreeSet<_> = ids(repair, "allowed_header_byte_offsets")?
        .into_iter()
        .collect();
    if allowed.iter().any(|&i| !(0x10c..0x110).contains(&i)) {
        return Err("repair header permission outside bounded header".into());
    }
    let header_changes: Vec<_> = before_mesh
        .header
        .iter()
        .zip(after_mesh.header)
        .enumerate()
        .filter_map(|(i, (a, b))| (a != b).then_some(i))
        .collect();
    if header_changes.iter().any(|i| !allowed.contains(i)) {
        return Err("repair changed a header byte outside explicit reviewed permissions".into());
    }
    if before_mesh.trailer == after_mesh.trailer {
        return Err("replacement did not replace collision acceleration trailer".into());
    }
    let translation = &diagnostic.manifest["request"];
    if repair["translation_request"] != *translation {
        return Err("repair translation request differs from diagnostic".into());
    }
    let record_index = number(translation, "record")?;
    let cell = u32::try_from(number(translation, "cell")?).map_err(|_| "cell out of range")?;
    let before = sbla::parse(&diagnostic.subpack, None)?;
    let record = before
        .records
        .get(record_index)
        .ok_or("missing collision record")?;
    let stored = if record.comp == record.uncomp {
        replacement.to_vec()
    } else {
        zlib::deflate(replacement, 9)
    };
    if record.comp != record.uncomp && stored.len() == replacement.len() {
        return Err("ambiguous repaired raw/zlib length".into());
    }
    let mut blobs = before.blobs();
    blobs[record_index].data = &stored;
    blobs[record_index].uncomp =
        u32::try_from(replacement.len()).map_err(|_| "repair payload too large")?;
    let rebuilt = sbla::write(before.descriptor.as_ref(), &blobs)?;
    let after = sbla::parse(&rebuilt, None)?;
    if before.descriptor != after.descriptor
        || before.counts != after.counts
        || before.records.len() != after.records.len()
    {
        return Err("repair ALBS structure changed".into());
    }
    for (i, (a, b)) in before.records.iter().zip(&after.records).enumerate() {
        if (a.kind, a.hash, a.f10, a.f14) != (b.kind, b.hash, b.f10, b.f14)
            || (i != record_index && (a.uncomp != b.uncomp || before.blobs[i] != after.blobs[i]))
        {
            return Err(format!("repair changed unrelated record {i}"));
        }
    }
    if collision::read_record(
        after.blobs[record_index],
        after.records[record_index].uncomp as usize,
    )? != replacement
        || sbla::write(after.descriptor.as_ref(), &after.blobs())? != rebuilt
    {
        return Err("repair ALBS round trip failed".into());
    }
    let pack = megapack::write(&[megapack::Item {
        crc: cells::cell_crc(cell),
        index: cell,
        data: &rebuilt,
    }]);
    let outer = megapack::parse(&pack)?;
    if outer.entries.len() != 1
        || outer.entries[0].crc != cells::cell_crc(cell)
        || outer.entries[0].index != cell
        || megapack::entry_slice(&pack, &outer.entries[0])? != rebuilt
        || megapack::write(&[megapack::Item {
            crc: cells::cell_crc(cell),
            index: cell,
            data: megapack::entry_slice(&pack, &outer.entries[0])?,
        }]) != pack
    {
        return Err("repair MP00 round trip failed".into());
    }
    let manifest = json!({"schema":"sab_collision_module_repaired_experiment_v1","diagnostic_only":true,"installable":false,"runtime_verified":false,
        "repair_request":repair,"compiler_receipt":compiler_receipt,"original_diagnostic_manifest":diagnostic.manifest,
        "pack_sha256":stage::sha256(&pack),"pack_bytes":pack.len(),"subpack_sha256":stage::sha256(&rebuilt),"payload_sha256":stage::sha256(replacement),"payload_bytes":replacement.len(),
        "geometry_checks":{"all_triangle_records_identical_to_diagnostic":true,"all_triangle_referenced_vertices_bit_identical":true,"all_vertices_bit_identical":true,"alignment_preserved":true,"changed_header_byte_offsets":header_changes,"unrelated_record_blobs_identical":true,"edited_albs_byte_roundtrip":true,"megapack_byte_roundtrip":true},
        "blockers":["Compiler receipt recorded; separate independent compiler verification and runtime-readiness review required","No live loading or physical behavior tested; no automatic installation"]});
    Ok(ModuleDiagnostic {
        pack,
        subpack: rebuilt,
        collision_payload: replacement.to_vec(),
        manifest,
    })
}

/// Rebuild a pinned diagnostic from retail before accepting separately verified acceleration.
/// The baseline mode changes collision acceleration only, isolating compiler compatibility
/// from any visual or triangle edits. Neither mode installs or authorizes a live artifact.
pub fn stage_repaired(
    game: &Path,
    repair: &Value,
    replacement: &[u8],
    compiler_receipt: &Value,
    destination: &Path,
) -> Result<Value, String> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if parent
        .canonicalize()
        .map_err(|e| e.to_string())?
        .starts_with(game.canonicalize().map_err(|e| e.to_string())?)
    {
        return Err("repair output must be outside the game directory".into());
    }
    if destination.exists() {
        return Err("repair output must not exist".into());
    }
    let request = &repair["translation_request"];
    request_transform(request)?;
    let cell = u32::try_from(number(request, "cell")?).map_err(|_| "cell out of range")?;
    let mounts = cells::france_mounts(game)?;
    let (mount, entry) =
        cells::resolve_entry(&mounts, cells::cell_crc(cell)).ok_or("cell not mounted")?;
    if mount.rel != text(request, "source_pack")? {
        return Err("effective source pack changed".into());
    }
    let source = megapack::entry_slice(&mount.map, entry)?;
    if stage::sha256(source) != text(request, "source_subpack_sha256")? {
        return Err("stale repair source subpack SHA-256".into());
    }
    let diagnostic = match text(repair, "mode")? {
        "combined_translation" => build_subpack(source, request)?,
        "baseline_recompile_control" => {
            let inspection = crate::collision::inspect_subpack(source, cell, &mount.rel)?;
            let surface = unique_surface(&inspection, request)?;
            let parsed = sbla::parse(source, None)?;
            if sbla::write(parsed.descriptor.as_ref(), &parsed.blobs())? != source {
                return Err("baseline source ALBS round trip failed".into());
            }
            let record = parsed.records[surface.record];
            let payload =
                collision::read_record(parsed.blobs[surface.record], record.uncomp as usize)?;
            let pack = megapack::write(&[megapack::Item {
                crc: cells::cell_crc(cell),
                index: cell,
                data: source,
            }]);
            ModuleDiagnostic {
                pack,
                subpack: source.to_vec(),
                collision_payload: payload,
                manifest: json!({"schema":"sab_collision_baseline_recompile_control_v1","request":request,"visual_placement_unchanged":true,"collision_geometry_unchanged":true,"source_subpack_sha256":stage::sha256(source)}),
            }
        }
        _ => return Err("unsupported repair mode".into()),
    };
    let result = package_replacement(&diagnostic, replacement, repair, compiler_receipt)?;
    fs::create_dir(destination).map_err(|e| e.to_string())?;
    let write = |name: &str, data: &[u8]| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination.join(name))
            .map_err(|e| e.to_string())?;
        file.write_all(data).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())
    };
    write("repaired-experiment.megapack", &result.pack)?;
    write("edited-subpack.albs", &result.subpack)?;
    write("collision-payload.bin", &result.collision_payload)?;
    write(
        "manifest.json",
        &serde_json::to_vec_pretty(&result.manifest).map_err(|e| e.to_string())?,
    )?;
    write("REQUIRES-RUNTIME-READINESS-REVIEW.txt",b"OFFLINE REPAIRED EXPERIMENT. Compiler receipt and geometry checks do not establish runtime compatibility. No installation performed. Preserve original diagnostic artifact and review exact hashes before a separately managed runtime experiment.\r\n")?;
    Ok(result.manifest)
}

fn request_transform(request: &Value) -> Result<[f32; 3], String> {
    if text(request, "schema")? != SCHEMA {
        return Err("unsupported module translation request schema".into());
    }
    if request["diagnostic_cut"].as_bool() != Some(true) {
        return Err(
            "diagnostic_cut must be explicitly true; opaque trailer safety is unknown".into(),
        );
    }
    if request["rotation_y_deg"].as_f64() != Some(0.0) {
        return Err("module experiment requires explicit zero rotation_y_deg".into());
    }
    delta(request)
}

fn visual_translation(
    source: &[u8],
    request: &Value,
    world_delta: [f32; 3],
) -> Result<(cellplace::Patched, [f32; 3], [f32; 3]), String> {
    let target = &request["target_visual"];
    if number(target, "cell")? != number(request, "cell")? {
        return Err("visual and collision must be in the same cell".into());
    }
    let cell = cellplace::read(source)?.ok_or("missing visual placements")?;
    let list_index = number(target, "list")?;
    let list = cell.lists.get(list_index).ok_or("missing visual list")?;
    if list.f14 != hex(target, "layer")? {
        return Err("visual layer mismatch".into());
    }
    let tag = hex(target, "tag")?;
    let objects: Vec<_> = list
        .objects
        .iter()
        .filter(|object| object.tag == tag)
        .collect();
    if objects.len() != 1 {
        return Err("visual tag must be unique in the requested list".into());
    }
    let object = objects[0];
    let blob = &source[list.loc.off..list.loc.off + list.loc.comp];
    let field = |tag| -> Result<&cellplace::ParamLoc, String> {
        let matching: Vec<_> = object.params.iter().filter(|p| p.tag == tag).collect();
        if matching.len() != 1 {
            return Err(format!("visual field {tag:08x} must be unique"));
        }
        Ok(matching[0])
    };
    let class = field(tags::CLASS_NAME)?;
    let expected_class = format!("{}\0", text(target, "class_name")?);
    if &blob[class.data_off..class.data_off + class.len] != expected_class.as_bytes() {
        return Err("visual class mismatch".into());
    }
    let position = field(tags::POSITION)?;
    let old_bytes = &blob[position.data_off..position.data_off + position.len];
    let old = cells::vec3(old_bytes).ok_or("visual Position must be three floats")?;
    let expected: [f32; 3] = serde_json::from_value(target["expected_position"].clone())
        .map_err(|_| "expected_position requires three finite numbers")?;
    if old.iter().any(|v| !v.is_finite())
        || expected.iter().any(|v| !v.is_finite())
        || old.map(f32::to_bits) != expected.map(f32::to_bits)
    {
        return Err("visual source position mismatch/non-finite".into());
    }
    let new = std::array::from_fn(|axis| {
        if world_delta[axis] == 0.0 {
            old[axis]
        } else {
            old[axis] + world_delta[axis]
        }
    });
    if new.iter().any(|v| !v.is_finite()) || old.map(f32::to_bits) == new.map(f32::to_bits) {
        return Err("visual translation overflows or rounds to no change".into());
    }
    // Also require valid, complete visual axes; they are deliberately unchanged.
    local_delta(basis(source, list_index, tag)?, world_delta)?;
    let new_bytes: Vec<_> = new.into_iter().flat_map(f32::to_le_bytes).collect();
    let patched = cellplace::patch_in_place(
        source,
        &cell,
        list_index,
        position.data_off,
        old_bytes,
        &new_bytes,
    )?;
    let allowed: BTreeSet<_> = patched
        .written_at
        .iter()
        .flat_map(|offset| *offset..*offset + 12)
        .collect();
    if source
        .iter()
        .zip(&patched.bytes)
        .enumerate()
        .any(|(offset, (a, b))| a != b && !allowed.contains(&offset))
    {
        return Err("visual patch changed bytes outside Position and its mirrors".into());
    }
    Ok((patched, old, new))
}

pub fn build_subpack(source: &[u8], request: &Value) -> Result<ModuleDiagnostic, String> {
    let world_delta = request_transform(request)?;
    if stage::sha256(source) != text(request, "source_subpack_sha256")? {
        return Err("stale source subpack SHA-256".into());
    }
    let cell = u32::try_from(number(request, "cell")?).map_err(|_| "cell out of range")?;
    let inspected = crate::collision::inspect_subpack(source, cell, text(request, "source_pack")?)?;
    let surface = unique_surface(&inspected, request)?;
    let selected = ids(request, "triangle_ids")?;
    let axes = basis(source, surface.list, surface.tag)?;
    let collision_delta = local_delta(axes, world_delta)?;
    let before = sbla::parse(source, None)?;
    if sbla::write(before.descriptor.as_ref(), &before.blobs())? != source {
        return Err("source ALBS byte round trip failed".into());
    }
    let record = before.records[surface.record];
    let original_payload =
        collision::read_record(before.blobs[surface.record], record.uncomp as usize)?;
    let cut =
        collision::diagnostic_cut_and_translate(&original_payload, &selected, collision_delta)?;
    let Payload::Baked(original_mesh) = collision::decode(&original_payload)? else {
        return Err("not a baked payload".into());
    };
    let Payload::Baked(edited_mesh) = collision::decode(&cut.payload)? else {
        return Err("cut did not decode as baked payload".into());
    };
    let selected_set: BTreeSet<_> = selected.iter().copied().collect();
    let old_count = original_mesh.vertices.len();
    if original_mesh.vertices != edited_mesh.vertices[..old_count]
        || original_mesh.trailer != edited_mesh.trailer
        || original_mesh.triangles.len() != edited_mesh.triangles.len()
    {
        return Err("cut changed original vertices, trailer or triangle count".into());
    }
    for (index, (old, new)) in original_mesh
        .triangles
        .iter()
        .zip(&edited_mesh.triangles)
        .enumerate()
    {
        if old.material != new.material
            || old.flags != new.flags
            || (!selected_set.contains(&index) && old != new)
        {
            return Err(format!("unexpected triangle edit {index}"));
        }
        if selected_set.contains(&index) {
            for axis in 0..3 {
                let j = cut
                    .old_vertex_ids
                    .binary_search(&usize::from(old.indices[axis]))
                    .map_err(|_| "missing vertex remap")?;
                if usize::from(new.indices[axis]) != old_count + j {
                    return Err("incorrect selected triangle remap".into());
                }
            }
        }
    }
    let (visual, old_position, new_position) = visual_translation(source, request, world_delta)?;
    let visual_pack = sbla::parse(&visual.bytes, None)?;
    let stored = if record.comp == record.uncomp {
        cut.payload.clone()
    } else {
        zlib::deflate(&cut.payload, 9)
    };
    if record.comp != record.uncomp && stored.len() == cut.payload.len() {
        return Err("ambiguous raw/zlib length".into());
    }
    let mut blobs = visual_pack.blobs();
    blobs[surface.record].data = &stored;
    blobs[surface.record].uncomp =
        u32::try_from(cut.payload.len()).map_err(|_| "payload too large")?;
    let rebuilt = sbla::write(visual_pack.descriptor.as_ref(), &blobs)?;
    let after = sbla::parse(&rebuilt, None)?;
    if before.descriptor != after.descriptor
        || before.counts != after.counts
        || before.records.len() != after.records.len()
    {
        return Err("ALBS descriptor/counts changed".into());
    }
    let changed_visual_records: Vec<_> = before
        .blobs
        .iter()
        .zip(&visual_pack.blobs)
        .enumerate()
        .filter_map(|(i, (a, b))| (a != b).then_some(i))
        .collect();
    for (i, (a, b)) in before.records.iter().zip(&after.records).enumerate() {
        if (a.kind, a.hash, a.f10, a.f14) != (b.kind, b.hash, b.f10, b.f14)
            || (i != surface.record && a.uncomp != b.uncomp)
            || (i != surface.record && after.blobs[i] != visual_pack.blobs[i])
        {
            return Err(format!("unexpected ALBS record change {i}"));
        }
    }
    if collision::read_record(
        after.blobs[surface.record],
        after.records[surface.record].uncomp as usize,
    )? != cut.payload
        || sbla::write(after.descriptor.as_ref(), &after.blobs())? != rebuilt
    {
        return Err("edited ALBS round trip failed".into());
    }
    let after_inspected =
        crate::collision::inspect_subpack(&rebuilt, cell, text(request, "source_pack")?)?;
    let after_surface = unique_surface(&after_inspected, request)?;
    let mut maximum_world_error = 0.0f32;
    for (j, &old) in cut.old_vertex_ids.iter().enumerate() {
        for axis in 0..3 {
            maximum_world_error = maximum_world_error.max(
                (surface.vertices[old][axis] + world_delta[axis]
                    - after_surface.vertices[old_count + j][axis])
                    .abs(),
            );
        }
    }
    if maximum_world_error > 0.0001 {
        return Err(format!(
            "collision world translation error {maximum_world_error}"
        ));
    }
    let pack = megapack::write(&[megapack::Item {
        crc: cells::cell_crc(cell),
        index: cell,
        data: &rebuilt,
    }]);
    let outer = megapack::parse(&pack)?;
    if outer.entries.len() != 1
        || outer.entries[0].crc != cells::cell_crc(cell)
        || outer.entries[0].index != cell
        || megapack::entry_slice(&pack, &outer.entries[0])? != rebuilt
        || megapack::write(&[megapack::Item {
            crc: cells::cell_crc(cell),
            index: cell,
            data: megapack::entry_slice(&pack, &outer.entries[0])?,
        }]) != pack
    {
        return Err("MP00 round trip failed".into());
    }
    let manifest = json!({
        "schema":"sab_collision_module_translation_diagnostic_v1","diagnostic_only":true,"installable":false,"runtime_verified":false,
        "request":request,"request_sha256":stage::sha256(&serde_json::to_vec(request).map_err(|e|e.to_string())?),
        "source_subpack_sha256":stage::sha256(source),"subpack_sha256":stage::sha256(&rebuilt),"pack_sha256":stage::sha256(&pack),"pack_bytes":pack.len(),
        "source_payload_sha256":stage::sha256(&original_payload),"payload_sha256":stage::sha256(&cut.payload),"payload_bytes":cut.payload.len(),
        "old_vertex_ids":cut.old_vertex_ids,"selected_triangle_ids":selected,"vertex_count_before":old_count,"vertex_count_after":edited_mesh.vertices.len(),
        "visual_position_before":old_position,"visual_position_after":new_position,"delta_world":world_delta,"delta_collision_local":collision_delta,
        "visual_position_offsets_in_original_subpack":visual.written_at,"changed_visual_records":changed_visual_records,
        "trailer":{"bytes":original_mesh.trailer.len(),"sha256":stage::sha256(original_mesh.trailer),"preserved":true,"runtime_safety":"unknown"},
        "checks":{"source_albs_byte_roundtrip":true,"edited_albs_byte_roundtrip":true,"megapack_byte_roundtrip":true,"original_vertices_preserved":true,"nonselected_triangles_preserved":true,"materials_flags_preserved":true,"unrelated_record_blobs_preserved":true,"maximum_world_error_m":maximum_world_error},
        "blockers":["Selection ownership/coverage requires independent review","Opaque acceleration/trailer compatibility unknown","No runtime result or installation scripts; diagnostic only"]
    });
    Ok(ModuleDiagnostic {
        pack,
        subpack: rebuilt,
        collision_payload: cut.payload,
        manifest,
    })
}

pub fn stage_diagnostic(game: &Path, request: &Value, destination: &Path) -> Result<Value, String> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolved_parent = parent
        .canonicalize()
        .map_err(|e| format!("output parent must exist: {e}"))?;
    if resolved_parent.starts_with(game.canonicalize().map_err(|e| e.to_string())?) {
        return Err("diagnostic output must be outside the game directory".into());
    }
    if destination.exists() {
        return Err("diagnostic destination must not exist".into());
    }
    let cell = u32::try_from(number(request, "cell")?).map_err(|_| "cell out of range")?;
    let mounts = cells::france_mounts(game)?;
    let (mount, entry) =
        cells::resolve_entry(&mounts, cells::cell_crc(cell)).ok_or("cell not mounted")?;
    if mount.rel != text(request, "source_pack")? {
        return Err("effective source pack changed".into());
    }
    let result = build_subpack(megapack::entry_slice(&mount.map, entry)?, request)?;
    fs::create_dir(destination).map_err(|e| e.to_string())?;
    let write = |name: &str, bytes: &[u8]| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination.join(name))
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())
    };
    write("combined-diagnostic.megapack", &result.pack)?;
    write("edited-subpack.albs", &result.subpack)?;
    write("collision-payload.bin", &result.collision_payload)?;
    write(
        "manifest.json",
        &serde_json::to_vec_pretty(&result.manifest).map_err(|e| e.to_string())?,
    )?;
    write("DO-NOT-INSTALL.txt",b"OFFLINE DIAGNOSTIC. Opaque trailer/acceleration safety and collision ownership are unverified. Not a runtime-test artifact.\r\n")?;
    Ok(result.manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_translation_diagnostic_guards() {
        let valid =
            json!({"schema":SCHEMA,"diagnostic_cut":true,"rotation_y_deg":0,"delta_world":[1,0,0]});
        assert_eq!(request_transform(&valid).unwrap(), [1.0, 0.0, 0.0]);
        for (key, value) in [
            ("schema", json!("wrong")),
            ("diagnostic_cut", json!(false)),
            ("rotation_y_deg", json!(90)),
            ("rotation_y_deg", Value::Null),
            ("delta_world", json!([0, 0, 0])),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(request_transform(&invalid).is_err(), "{key}");
        }
    }
}
