use std::collections::HashMap;
use std::path::Path;

use sab_formats::collision::{self, Payload};
use sab_formats::editnodes::tags;
use sab_formats::{cellplace, megapack, mesh, pandemic_hash, sbla};

use crate::{cells, stage};

const MAX_CELL_DECODE_BYTES: usize = 128 * 1024 * 1024;
const MAX_SURFACE_VERTICES: usize = 1_000_000;
const MAX_SURFACE_TRIANGLES: usize = 2_000_000;

#[derive(Clone, Debug)]
pub struct Surface {
    pub record: usize,
    pub hash: u32,
    pub list: usize,
    pub tag: u32,
    pub layer: u32,
    pub class_name: String,
    pub vertices: Vec<[f32; 3]>,
    pub triangles: Vec<[u16; 3]>,
    pub triangle_metadata: Vec<[u16; 2]>,
    pub local_topology: collision::Topology,
}

#[derive(Clone, Debug)]
pub struct CellCollision {
    pub cell: u32,
    pub source_pack: String,
    pub source_crc: u32,
    pub subpack_sha256: String,
    pub surfaces: Vec<Surface>,
    pub issues: Vec<String>,
    pub unsupported_records: usize,
    pub edge_records: usize,
    pub baked_records: usize,
}

pub fn inspect_cell(game: &Path, cell: u32) -> Result<CellCollision, String> {
    let mounts = cells::france_mounts(game)?;
    let crc = cells::cell_crc(cell);
    let (mount, entry) = cells::resolve_entry(&mounts, crc)
        .ok_or_else(|| format!("no effective mounted cell {cell}"))?;
    let source = megapack::entry_slice(&mount.map, entry)?;
    inspect_subpack(source, cell, &mount.rel)
}

fn field<'a>(
    blob: &'a [u8],
    object: &cellplace::ObjectLoc,
    tag: u32,
) -> Result<Option<&'a [u8]>, String> {
    let mut matching = object.params.iter().filter(|param| param.tag == tag);
    let first = matching.next();
    if matching.next().is_some() {
        return Err(format!("duplicate placement parameter {tag:08x}"));
    }
    first
        .map(|param| {
            sab_formats::bytes::slice(blob, param.data_off, param.len, "collision placement field")
        })
        .transpose()
}

fn vector(blob: &[u8], object: &cellplace::ObjectLoc, tag: u32) -> Result<[f32; 3], String> {
    let data = field(blob, object, tag)?
        .ok_or_else(|| format!("missing collision placement vector {tag:08x}"))?;
    let point = cells::vec3(data).ok_or("collision placement vector is not 12 bytes")?;
    if point.iter().any(|value| !value.is_finite()) {
        return Err("non-finite collision placement vector".into());
    }
    Ok(point)
}

fn transformed(
    vertices: &[[f32; 3]],
    position: [f32; 3],
    axes: [[f32; 3]; 3],
) -> Result<Vec<[f32; 3]>, String> {
    let determinant = axes[0][0] * (axes[1][1] * axes[2][2] - axes[1][2] * axes[2][1])
        - axes[0][1] * (axes[1][0] * axes[2][2] - axes[1][2] * axes[2][0])
        + axes[0][2] * (axes[1][0] * axes[2][1] - axes[1][1] * axes[2][0]);
    if !determinant.is_finite() || determinant.abs() < 1e-8 {
        return Err("singular collision placement basis".into());
    }
    vertices
        .iter()
        .map(|point| {
            let world = std::array::from_fn(|axis| {
                position[axis]
                    + axes[0][axis] * point[0]
                    + axes[1][axis] * point[1]
                    + axes[2][axis] * point[2]
            });
            if world.iter().any(|value: &f32| !value.is_finite()) {
                Err("collision world transform overflow".into())
            } else {
                Ok(world)
            }
        })
        .collect()
}

pub fn inspect_subpack(
    source: &[u8],
    cell: u32,
    source_pack: &str,
) -> Result<CellCollision, String> {
    let pack = sbla::parse(source, None)?;
    let placements = cellplace::read(source)?;
    let mut output = CellCollision {
        cell,
        source_pack: source_pack.into(),
        source_crc: cells::cell_crc(cell),
        subpack_sha256: stage::sha256(source),
        surfaces: Vec::new(),
        issues: Vec::new(),
        unsupported_records: 0,
        edge_records: 0,
        baked_records: 0,
    };
    let mut frequencies = HashMap::new();
    for record in pack.records.iter().filter(|record| record.kind == 2) {
        *frequencies.entry(record.hash).or_insert(0usize) += 1;
    }
    let prefix = format!("france_streamblock_{cell}_baked_collision");
    let mut decoded_bytes = 0usize;
    let mut surface_vertices = 0usize;
    let mut surface_triangles = 0usize;
    for (record_index, (record, stored)) in pack
        .records
        .iter()
        .zip(&pack.blobs)
        .enumerate()
        .filter(|(_, (record, _))| record.kind == 2)
    {
        let inspected = (|| -> Result<bool, String> {
            if frequencies[&record.hash] != 1 {
                return Err("ambiguous duplicate type-2 hash".into());
            }
            decoded_bytes = decoded_bytes
                .checked_add(record.uncomp as usize)
                .ok_or("cell inspection size overflow")?;
            if decoded_bytes > MAX_CELL_DECODE_BYTES {
                return Err("cell exceeds 128 MiB decoded-record budget".into());
            }
            let payload = collision::read_record(stored, record.uncomp as usize)?;
            let decoded = collision::decode(&payload)?;
            let payload_hash = match &decoded {
                Payload::Edges(edges) => edges.hash,
                Payload::Baked(mesh) => mesh.hash,
            };
            if payload_hash != record.hash {
                return Err("type-2 payload hash differs from ALBS record".into());
            }
            let baked = match decoded {
                Payload::Edges(_) => {
                    output.edge_records += 1;
                    return Ok(false);
                }
                Payload::Baked(baked) => baked,
            };
            output.baked_records += 1;
            if !baked.reference_frame_supported() {
                return Err("unsupported internal reference-frame profile; no guessed world transform drawn".into());
            }
            let models: Vec<_> = pack
                .records
                .iter()
                .zip(&pack.blobs)
                .filter(|(model, _)| model.kind == 0 && model.hash == record.hash)
                .collect();
            if models.len() != 1 {
                return Err("baked mesh requires one exact same-cell type-0 name".into());
            }
            let header = mesh::parse_msha(models[0].1)?;
            if pandemic_hash(&header.name) != record.hash
                || !header.name.to_ascii_lowercase().starts_with(&prefix)
            {
                return Err("type-0 name/hash is not this cell's baked-collision resource".into());
            }
            let selected: Vec<_> = (0..baked.triangles.len()).collect();
            let local_topology = collision::topology(&baked.triangles, &selected)?;
            let mut matched = 0usize;
            if let Some(placements) = &placements {
                for (list_index, list) in placements.lists.iter().enumerate() {
                    let blob = &source[list.loc.off..list.loc.off + list.loc.comp];
                    for object in &list.objects {
                        let Some(class) = field(blob, object, tags::CLASS_NAME)? else {
                            continue;
                        };
                        if !cells::cstr(class).eq_ignore_ascii_case(&header.name) {
                            continue;
                        }
                        let placement_result = (|| -> Result<Surface, String> {
                            if list
                                .objects
                                .iter()
                                .filter(|other| other.tag == object.tag)
                                .count()
                                != 1
                            {
                                return Err("duplicate collision placement tag in list".into());
                            }
                            let position = vector(blob, object, tags::POSITION)?;
                            let axes = [
                                vector(blob, object, tags::X_AXIS)?,
                                vector(blob, object, tags::Y_AXIS)?,
                                vector(blob, object, tags::Z_AXIS)?,
                            ];
                            if surface_vertices + baked.vertices.len() > MAX_SURFACE_VERTICES {
                                return Err(
                                    "cell exceeds one-million displayed-vertex budget".into()
                                );
                            }
                            if surface_triangles + baked.triangles.len() > MAX_SURFACE_TRIANGLES {
                                return Err(
                                    "cell exceeds two-million displayed-triangle budget".into()
                                );
                            }
                            let vertices = transformed(&baked.vertices, position, axes)?;
                            surface_vertices += vertices.len();
                            surface_triangles += baked.triangles.len();
                            Ok(Surface {
                                record: record_index,
                                hash: record.hash,
                                list: list_index,
                                tag: object.tag,
                                layer: list.f14,
                                class_name: header.name.clone(),
                                vertices,
                                triangles: baked
                                    .triangles
                                    .iter()
                                    .map(|triangle| triangle.indices)
                                    .collect(),
                                triangle_metadata: baked
                                    .triangles
                                    .iter()
                                    .map(|triangle| [triangle.material, triangle.flags])
                                    .collect(),
                                local_topology: local_topology.clone(),
                            })
                        })();
                        match placement_result {
                            Ok(surface) => {
                                output.surfaces.push(surface);
                                matched += 1;
                            }
                            Err(error) => output.issues.push(format!(
                                "record {record_index} list {list_index} tag {:08x}: {error}",
                                object.tag
                            )),
                        }
                    }
                }
            }
            if matched == 0 {
                return Err("no unambiguous complete baked-collision placement; local mesh not drawn at origin".into());
            }
            Ok(true)
        })();
        match inspected {
            Ok(true) => (),
            Ok(false) => output.unsupported_records += 1,
            Err(error) => {
                output.unsupported_records += 1;
                output.issues.push(format!(
                    "record {record_index} hash {:08x}: {error}",
                    record.hash
                ));
            }
        }
    }
    if output.edge_records != 0 {
        output.issues.push(format!(
            "{} edge-item records parsed as opaque metadata, not proven solid colliders",
            output.edge_records
        ));
    }
    output.issues.push("Read-only source geometry, not live physics; building-to-collider ownership and runtime layer activation are unknown. Type-3/terrain/other physics are not covered. Visual edits do not update this overlay or collision.".into());
    Ok(output)
}

pub fn report(inspected: &CellCollision, geometry: bool) -> serde_json::Value {
    let surfaces: Vec<_> = inspected.surfaces.iter().map(|surface| {
        let bounds: Option<[[f32; 3]; 2]> = surface.vertices.first().map(|first| {
            surface.vertices.iter().fold([*first, *first], |mut bounds, point| {
                for axis in 0..3 {
                    bounds[0][axis] = bounds[0][axis].min(point[axis]);
                    bounds[1][axis] = bounds[1][axis].max(point[axis]);
                }
                bounds
            })
        });
        let topology = &surface.local_topology;
        let mut value = serde_json::json!({
            "record": surface.record, "hash": format!("{:08x}", surface.hash), "list": surface.list,
            "tag": format!("{:08x}", surface.tag), "layer": format!("{:08x}", surface.layer), "class_name": surface.class_name,
            "vertices": surface.vertices.len(), "triangles": surface.triangles.len(), "world_bounds": bounds,
            "topology": { "boundary_edges": topology.boundary_edges, "non_manifold_edges": topology.non_manifold_edges, "degenerate_faces": topology.degenerate_faces },
        });
        if geometry {
            value["world_vertices"] = serde_json::json!(surface.vertices);
            value["triangle_indices"] = serde_json::json!(surface.triangles);
            value["triangle_material_flags"] = serde_json::json!(surface.triangle_metadata);
        }
        value
    }).collect();
    serde_json::json!({
        "schema": "sab_collision_inspection_v1", "read_only": true, "runtime_verified": false,
        "cell": inspected.cell, "source_pack": inspected.source_pack, "source_crc": format!("{:08x}", inspected.source_crc),
        "subpack_sha256": inspected.subpack_sha256, "baked_records": inspected.baked_records,
        "edge_records": inspected.edge_records, "unsupported_records": inspected.unsupported_records,
        "surfaces": surfaces, "issues": inspected.issues,
    })
}
