use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use sab_formats::collision::{self, Payload};
use sab_formats::editnodes::tags;
use sab_formats::{cellplace, megapack, sbla, zlib};
use serde_json::{json, Value};

use crate::{cells, stage};

pub const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub struct DryRun {
    pub pack: Vec<u8>,
    pub manifest: Value,
}

pub(super) fn number(request: &Value, key: &str) -> Result<usize, String> {
    request[key]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("missing or invalid {key}"))
}

pub(super) fn text<'a>(request: &'a Value, key: &str) -> Result<&'a str, String> {
    request[key]
        .as_str()
        .ok_or_else(|| format!("missing {key}"))
}

pub(super) fn hex(request: &Value, key: &str) -> Result<u32, String> {
    u32::from_str_radix(text(request, key)?, 16).map_err(|_| format!("invalid hex {key}"))
}

pub(super) fn delta(request: &Value) -> Result<[f32; 3], String> {
    let values = request["delta_world"]
        .as_array()
        .ok_or("missing delta_world")?;
    if values.len() != 3 {
        return Err("delta_world requires exactly three finite numbers".into());
    }
    let mut result = [0.0; 3];
    for (index, value) in values.iter().enumerate() {
        result[index] = value.as_f64().ok_or("invalid delta_world")? as f32;
    }
    if result.iter().any(|value| !value.is_finite()) || result == [0.0; 3] {
        return Err("delta_world must be finite and nonzero".into());
    }
    Ok(result)
}

pub(super) fn ids(request: &Value, key: &str) -> Result<Vec<usize>, String> {
    request[key]
        .as_array()
        .ok_or_else(|| format!("missing {key}"))?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| format!("invalid {key}"))
        })
        .collect()
}

pub(super) fn basis(
    source: &[u8],
    list_index: usize,
    object_tag: u32,
) -> Result<[[f32; 3]; 3], String> {
    let layout = cellplace::read(source)?.ok_or("missing placements")?;
    let list = layout
        .lists
        .get(list_index)
        .ok_or("missing placement list")?;
    let objects: Vec<_> = list
        .objects
        .iter()
        .filter(|object| object.tag == object_tag)
        .collect();
    if objects.len() != 1 {
        return Err("collision placement tag must be unique in its list".into());
    }
    let blob = &source[list.loc.off..list.loc.off + list.loc.comp];
    let mut axes = [[0.0; 3]; 3];
    for (axis, tag) in [tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS]
        .iter()
        .enumerate()
    {
        let params: Vec<_> = objects[0]
            .params
            .iter()
            .filter(|param| param.tag == *tag)
            .collect();
        if params.len() != 1 {
            return Err("collision placement needs exactly one of each basis vector".into());
        }
        let param = params[0];
        axes[axis] = cells::vec3(&blob[param.data_off..param.data_off + param.len])
            .ok_or("invalid collision placement basis")?;
    }
    Ok(axes)
}

pub(super) fn local_delta(axes: [[f32; 3]; 3], world: [f32; 3]) -> Result<[f32; 3], String> {
    let columns = axes.map(|axis| axis.map(f64::from));
    let cross = |first: [f64; 3], second: [f64; 3]| {
        [
            first[1] * second[2] - first[2] * second[1],
            first[2] * second[0] - first[0] * second[2],
            first[0] * second[1] - first[1] * second[0],
        ]
    };
    let dot = |first: [f64; 3], second: [f64; 3]| {
        first
            .iter()
            .zip(second)
            .map(|(left, right)| left * right)
            .sum::<f64>()
    };
    let determinant = dot(columns[0], cross(columns[1], columns[2]));
    if !determinant.is_finite() || determinant.abs() < 1e-8 {
        return Err("singular or non-finite collision placement basis".into());
    }
    let world = world.map(f64::from);
    let local = [
        dot(world, cross(columns[1], columns[2])) / determinant,
        dot(world, cross(columns[2], columns[0])) / determinant,
        dot(world, cross(columns[0], columns[1])) / determinant,
    ]
    .map(|value| value as f32);
    if local.iter().any(|value| !value.is_finite()) {
        return Err("collision translation inverse overflow".into());
    }
    Ok(local)
}

pub(super) fn unique_surface<'a>(
    inspected: &'a crate::collision::CellCollision,
    request: &Value,
) -> Result<&'a crate::collision::Surface, String> {
    let record = number(request, "record")?;
    let matching: Vec<_> = inspected
        .surfaces
        .iter()
        .filter(|surface| surface.record == record)
        .collect();
    if matching.len() != 1 {
        return Err("edited resource must have exactly one supported placement".into());
    }
    let surface = matching[0];
    if surface.hash != hex(request, "hash")?
        || surface.tag != hex(request, "tag")?
        || surface.layer != hex(request, "layer")?
        || surface.list != number(request, "list")?
        || surface.class_name != text(request, "class_name")?
    {
        return Err("collision resource/placement identity mismatch".into());
    }
    if inspected
        .issues
        .iter()
        .any(|issue| issue.starts_with(&format!("record {record} ")))
    {
        return Err("edited collision record has unresolved placement issues".into());
    }
    Ok(surface)
}

pub fn plan_subpack(
    source: &[u8],
    cell: u32,
    source_pack: &str,
    hash: u32,
    component: usize,
    delta_world: [f32; 3],
) -> Result<Value, String> {
    let inspected = crate::collision::inspect_subpack(source, cell, source_pack)?;
    let matches: Vec<_> = inspected
        .surfaces
        .iter()
        .filter(|surface| surface.hash == hash)
        .collect();
    if matches.len() != 1 {
        return Err("plan requires exactly one inspected placement for the hash".into());
    }
    let surface = matches[0];
    let pack = sbla::parse(source, None)?;
    let record = &pack.records[surface.record];
    let payload = collision::read_record(pack.blobs[surface.record], record.uncomp as usize)?;
    let Payload::Baked(mesh) = collision::decode(&payload)? else {
        return Err("not baked collision".into());
    };
    let components = collision::triangle_components(&mesh.triangles)?;
    let selected = components
        .get(component)
        .ok_or("component index out of range")?;
    let vertex_ids: BTreeSet<_> = selected
        .iter()
        .flat_map(|&triangle| mesh.triangles[triangle].indices)
        .collect();
    let mut lower = [f32::INFINITY; 3];
    let mut upper = [f32::NEG_INFINITY; 3];
    for vertex in &vertex_ids {
        for axis in 0..3 {
            lower[axis] = lower[axis].min(surface.vertices[*vertex as usize][axis]);
            upper[axis] = upper[axis].max(surface.vertices[*vertex as usize][axis]);
        }
    }
    let selected_row = json!({"component":component,"triangles":selected.len(),"vertices":vertex_ids.len(),"world_bounds":[lower,upper]});
    let request = json!({
        "schema":"sab_collision_dry_run_request_v1", "cell":cell,
        "source_pack":source_pack,"source_subpack_sha256":stage::sha256(source),
        "record":surface.record,"hash":format!("{:08x}",surface.hash),
        "list":surface.list,"tag":format!("{:08x}",surface.tag),"layer":format!("{:08x}",surface.layer),
        "class_name":surface.class_name,"triangle_ids":selected,"delta_world":delta_world,
        "selection_evidence":{"ownership":"inferred, not proven","policy":"explicit component only; every other component excluded","component":component,"component_count":components.len(),"selected_component":selected_row},
        "runtime_verified":false
    });
    delta(&request)?;
    if serde_json::to_vec_pretty(&request)
        .map_err(|error| error.to_string())?
        .len() as u64
        >= MAX_REQUEST_BYTES
    {
        return Err("collision plan exceeds the 1 MiB executable request budget".into());
    }
    Ok(request)
}

pub fn dry_run_subpack(source: &[u8], request: &Value) -> Result<DryRun, String> {
    if text(request, "schema")? != "sab_collision_dry_run_request_v1" {
        return Err("unsupported collision request schema".into());
    }
    if stage::sha256(source) != text(request, "source_subpack_sha256")? {
        return Err("stale source subpack SHA-256".into());
    }
    let cell = u32::try_from(number(request, "cell")?).map_err(|_| "cell out of range")?;
    let inspected = crate::collision::inspect_subpack(source, cell, text(request, "source_pack")?)?;
    let surface = unique_surface(&inspected, request)?;
    let selected = ids(request, "triangle_ids")?;
    let delta_world = delta(request)?;
    let axes = basis(source, surface.list, surface.tag)?;
    let delta_local = local_delta(axes, delta_world)?;
    let parsed = sbla::parse(source, None)?;
    if sbla::write(parsed.descriptor.as_ref(), &parsed.blobs())? != source {
        return Err("source ALBS is not byte-identical through existing pack writer".into());
    }
    let record = parsed.records[surface.record];
    let original = collision::read_record(parsed.blobs[surface.record], record.uncomp as usize)?;
    let edited = collision::translate_vertices(&original, &selected, delta_local)?;
    let actual_offsets: Vec<_> = original
        .iter()
        .zip(&edited.payload)
        .enumerate()
        .filter_map(|(offset, (before, after))| (before != after).then_some(offset))
        .collect();
    if edited.payload.len() != original.len()
        || actual_offsets.is_empty()
        || actual_offsets != edited.changed_offsets
    {
        return Err("collision edit byte-diff receipt mismatch".into());
    }
    let allowed: BTreeSet<_> = edited
        .vertex_ids
        .iter()
        .flat_map(|vertex| {
            collision::VERTEX_OFFSET + vertex * 12..collision::VERTEX_OFFSET + vertex * 12 + 12
        })
        .collect();
    if actual_offsets
        .iter()
        .any(|offset| !allowed.contains(offset))
    {
        return Err("collision writer changed bytes outside selected vertices".into());
    }
    let Payload::Baked(original_mesh) = collision::decode(&original)? else {
        return Err("not baked".into());
    };
    let components = collision::triangle_components(&original_mesh.triangles)?;
    let component = number(&request["selection_evidence"], "component")?;
    let mut ordered_selection = selected.clone();
    ordered_selection.sort_unstable();
    if components.get(component) != Some(&ordered_selection) {
        return Err("triangle selection does not match its declared complete component".into());
    }
    let Payload::Baked(edited_mesh) = collision::decode(&edited.payload)? else {
        return Err("not baked".into());
    };
    let expected_vertices: BTreeSet<_> = selected
        .iter()
        .flat_map(|&triangle| original_mesh.triangles[triangle].indices.map(usize::from))
        .collect();
    if expected_vertices.iter().copied().collect::<Vec<_>>() != edited.vertex_ids {
        return Err("selected triangle/vertex receipt mismatch".into());
    }
    if original_mesh.triangles != edited_mesh.triangles
        || original_mesh.header != edited_mesh.header
        || original_mesh.padding != edited_mesh.padding
        || original_mesh.trailer != edited_mesh.trailer
    {
        return Err("collision non-vertex bytes changed".into());
    }
    for (index, (before, after)) in original_mesh
        .vertices
        .iter()
        .zip(&edited_mesh.vertices)
        .enumerate()
    {
        let expected = if edited.vertex_ids.binary_search(&index).is_ok() {
            std::array::from_fn(|axis| {
                if delta_local[axis] == 0.0 {
                    before[axis]
                } else {
                    before[axis] + delta_local[axis]
                }
            })
        } else {
            *before
        };
        if expected.map(f32::to_bits) != after.map(f32::to_bits) {
            return Err(format!("unexpected vertex edit at {index}"));
        }
    }
    let compressed = if record.comp == record.uncomp {
        edited.payload.clone()
    } else {
        let result = zlib::deflate(&edited.payload, 9);
        if result.len() == edited.payload.len() {
            return Err("ambiguous raw/zlib stored length".into());
        }
        result
    };
    let mut blobs = parsed.blobs();
    blobs[surface.record].data = &compressed;
    let rebuilt = sbla::write(parsed.descriptor.as_ref(), &blobs)?;
    let verified = sbla::parse(&rebuilt, None)?;
    if verified.descriptor != parsed.descriptor
        || verified.counts != parsed.counts
        || verified.records.len() != parsed.records.len()
    {
        return Err("ALBS descriptor/counts/order changed".into());
    }
    for (index, (before, after)) in parsed.records.iter().zip(&verified.records).enumerate() {
        if (
            before.kind,
            before.hash,
            before.uncomp,
            before.f10,
            before.f14,
        ) != (after.kind, after.hash, after.uncomp, after.f10, after.f14)
            || (index != surface.record && parsed.blobs[index] != verified.blobs[index])
        {
            return Err(format!("unrelated ALBS record {index} changed"));
        }
    }
    if collision::read_record(verified.blobs[surface.record], record.uncomp as usize)?
        != edited.payload
        || sbla::write(verified.descriptor.as_ref(), &verified.blobs())? != rebuilt
    {
        return Err("modified ALBS round trip failed".into());
    }
    let after_inspection =
        crate::collision::inspect_subpack(&rebuilt, cell, text(request, "source_pack")?)?;
    let after_surface = unique_surface(&after_inspection, request)?;
    let mut maximum_world_error = 0.0f32;
    for (index, (before, after)) in surface
        .vertices
        .iter()
        .zip(&after_surface.vertices)
        .enumerate()
    {
        for axis in 0..3 {
            let expected = before[axis]
                + if edited.vertex_ids.binary_search(&index).is_ok() {
                    delta_world[axis]
                } else {
                    0.0
                };
            maximum_world_error = maximum_world_error.max((after[axis] - expected).abs());
        }
    }
    if maximum_world_error > 0.0001 {
        return Err(format!(
            "world translation differs by {maximum_world_error} m"
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
    {
        return Err("megapack entry round trip failed".into());
    }
    let replay = megapack::write(&[megapack::Item {
        crc: outer.entries[0].crc,
        index: outer.entries[0].index,
        data: megapack::entry_slice(&pack, &outer.entries[0])?,
    }]);
    if replay != pack {
        return Err("megapack byte round trip failed".into());
    }
    let manifest = json!({
        "schema":"sab_collision_dry_run_v1","diagnostic_only":true,"installable":false,"runtime_verified":false,
        "request":request,"request_sha256":stage::sha256(&serde_json::to_vec(request).map_err(|error|error.to_string())?),
        "delta_local":delta_local,"basis":axes,"selected_triangle_ids":selected,"selected_vertex_ids":edited.vertex_ids,
        "verified_selection":{"component":component,"component_count":components.len(),"excluded_component_count":components.len()-1,"triangle_count":ordered_selection.len(),"private_vertex_count":expected_vertices.len()},
        "changed_payload_byte_offsets":actual_offsets,"changed_payload_bytes":actual_offsets.len(),
        "source_payload_sha256":stage::sha256(&original),"edited_payload_sha256":stage::sha256(&edited.payload),
        "source_subpack_sha256":stage::sha256(source),"edited_subpack_sha256":stage::sha256(&rebuilt),
        "pack_sha256":stage::sha256(&pack),"pack_bytes":pack.len(),
        "trailer":{"bytes":original_mesh.trailer.len(),"sha256":stage::sha256(original_mesh.trailer),"preserved":true,"runtime_safety":"unknown"},
        "checks":{"source_albs_byte_roundtrip":true,"edited_albs_byte_roundtrip":true,"megapack_byte_roundtrip":true,"unrelated_record_blobs_identical":true,"only_selected_vertex_bytes_changed":true,"maximum_world_error_m":maximum_world_error},
        "blockers":["Building ownership inferred, not proven","West-side landing attribution unresolved unless separately documented","Stored acceleration/trailer safety not runtime tested","Translation only; no visual placement or rotation edited","No install or restore scripts generated"]
    });
    Ok(DryRun { pack, manifest })
}

pub fn plan(
    game: &Path,
    cell: u32,
    hash: u32,
    component: usize,
    delta_world: [f32; 3],
) -> Result<Value, String> {
    let mounts = cells::france_mounts(game)?;
    let (mount, entry) =
        cells::resolve_entry(&mounts, cells::cell_crc(cell)).ok_or("cell not mounted")?;
    plan_subpack(
        megapack::entry_slice(&mount.map, entry)?,
        cell,
        &mount.rel,
        hash,
        component,
        delta_world,
    )
}

pub fn stage_dry_run(game: &Path, request: &Value, destination: &Path) -> Result<Value, String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolved_parent = parent
        .canonicalize()
        .map_err(|error| format!("output parent must exist: {error}"))?;
    if resolved_parent.starts_with(game.canonicalize().map_err(|error| error.to_string())?) {
        return Err("dry-run output must be outside the game directory".into());
    }
    if destination.exists() {
        return Err("dry-run destination must not exist".into());
    }
    let cell = u32::try_from(number(request, "cell")?).map_err(|_| "cell out of range")?;
    let mounts = cells::france_mounts(game)?;
    let (mount, entry) =
        cells::resolve_entry(&mounts, cells::cell_crc(cell)).ok_or("cell not mounted")?;
    if mount.rel != text(request, "source_pack")? {
        return Err("effective source pack changed".into());
    }
    let result = dry_run_subpack(megapack::entry_slice(&mount.map, entry)?, request)?;
    fs::create_dir(destination).map_err(|error| error.to_string())?;
    let write_new = |name: &str, bytes: &[u8]| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination.join(name))
            .map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())
    };
    write_new("collision-diagnostic.megapack", &result.pack)?;
    write_new(
        "manifest.json",
        &serde_json::to_vec_pretty(&result.manifest).map_err(|error| error.to_string())?,
    )?;
    write_new("DO-NOT-INSTALL.txt",b"OFFLINE DIAGNOSTIC ONLY. Ownership, neighboring collision and stored acceleration safety remain unverified. No game installation is authorized.\r\n")?;
    Ok(result.manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_delta_inverts_rotated_scaled_basis() {
        assert_eq!(
            local_delta(
                [[0.0, 0.0, -2.0], [0.0, 3.0, 0.0], [4.0, 0.0, 0.0]],
                [8.0, 9.0, -4.0]
            )
            .unwrap(),
            [2.0, 3.0, 2.0]
        );
        assert!(local_delta([[0.0; 3]; 3], [1.0; 3]).is_err());
        assert!(local_delta([[f32::NAN; 3]; 3], [1.0; 3]).is_err());
    }

    #[test]
    fn request_fields_fail_closed() {
        for invalid in [
            json!({}),
            json!({"delta_world":[0,0,0]}),
            json!({"delta_world":[1,2]}),
            json!({"delta_world":[1e100,0,0]}),
        ] {
            assert!(delta(&invalid).is_err());
        }
        assert!(ids(&json!({"triangle_ids":[-1]}), "triangle_ids").is_err());
        assert!(dry_run_subpack(&[], &json!({"schema":"wrong"})).is_err());
        assert!(dry_run_subpack(
            &[],
            &json!({"schema":"sab_collision_dry_run_request_v1","source_subpack_sha256":"wrong"})
        )
        .unwrap_err()
        .contains("stale"));
    }
}
