use crate::{bytes, mesh};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct TerrainStream {
    pub format: u32,
    pub bone: usize,
    pub positions: Vec<[f32; 3]>,
    pub bounds: [[f32; 3]; 2],
    pub quantization: f32,
}

#[derive(Clone, Debug)]
pub struct TerrainMesh {
    pub name: String,
    pub streams: Vec<TerrainStream>,
    pub vertices: Vec<[f32; 3]>,
    pub triangles: Vec<[usize; 3]>,
    pub bounds: [[f32; 3]; 2],
}

#[derive(Clone, Copy)]
struct Bone {
    translation: [f32; 3],
    scale: [f32; 3],
    bounds: [[f32; 3]; 2],
}

fn vector(data: &[u8], offset: usize) -> Result<[f32; 3], String> {
    let result = [
        bytes::f32_le(data, offset, "vector")?,
        bytes::f32_le(data, offset + 4, "vector")?,
        bytes::f32_le(data, offset + 8, "vector")?,
    ];
    if result.iter().any(|value| !value.is_finite()) {
        return Err("non-finite terrain vector".into());
    }
    Ok(result)
}

pub fn unpack_vertex(
    raw: [u16; 2],
    translation: [f32; 3],
    scale: [f32; 3],
) -> Result<[f32; 3], String> {
    if translation
        .iter()
        .chain(&scale)
        .any(|value| !value.is_finite())
        || scale.iter().any(|value| *value < 0.0)
    {
        return Err("invalid packed terrain transform".into());
    }
    let height = raw[0] ^ 0x8000;
    let lattice = raw[1] ^ 0x8000;
    if lattice >= 33 * 33 {
        return Err(format!("terrain lattice {lattice} outside 33x33"));
    }
    Ok([
        translation[0] + (lattice % 33) as f32 * scale[0],
        translation[1] + height as f32 / 65535.0 * scale[1],
        translation[2] + (lattice / 33) as f32 * scale[2],
    ])
}

pub fn decode(blob: &[u8]) -> Result<TerrainMesh, String> {
    let header = mesh::parse_msha(blob)?;
    if !header.name.starts_with("France_Chunk") {
        return Err("not a France_Chunk terrain candidate".into());
    }
    let (body, data) = mesh::read_body_and_dat(blob, &header)?;
    let read = |offset| bytes::u32_le(&body, offset, "terrain MESH").map(|value| value as usize);
    let bone_count = read(204)?;
    let stream_count = bytes::u16_le(&body, 216, "stream count")? as usize;
    let primitive_count = bytes::u16_le(&body, 218, "primitive count")? as usize;
    let draw_count = read(232)?;
    if !(2..=3).contains(&bone_count) || read(208)? != 0 || !(1..=4).contains(&stream_count) {
        return Err(format!(
            "unsupported terrain layout: {bone_count} bones, {stream_count} streams"
        ));
    }
    if [read(256)?, read(264)?, read(272)?] != [bone_count; 3] {
        return Err("terrain skeleton counts disagree".into());
    }
    let matrices = 288usize
        .checked_add(bone_count)
        .and_then(|offset| offset.checked_add(read(244).ok()?))
        .ok_or("skeleton offset overflow")?;
    let boxes = matrices + bone_count * 64;
    let transforms = boxes + bone_count * 64;
    let parents = transforms + bone_count * 48;
    let streams_offset = parents + bone_count * 6 + if read(260)? == 0 { 0 } else { 2 };
    let primitives_offset = streams_offset + stream_count * 152;
    let draws_offset = primitives_offset + primitive_count * 100;
    let expected_end = draw_count
        .checked_mul(16)
        .and_then(|size| draws_offset.checked_add(size))
        .ok_or("draw table overflow")?;
    if expected_end != body.len() {
        return Err(format!(
            "terrain body ends at {expected_end}, length {}",
            body.len()
        ));
    }
    let mut bones = Vec::new();
    for index in 0..bone_count {
        let transform = transforms + index * 48;
        let translation = vector(&body, transform)?;
        let scale = vector(&body, transform + 32)?;
        let rotation = [
            bytes::f32_le(&body, transform + 16, "rotation")?,
            bytes::f32_le(&body, transform + 20, "rotation")?,
            bytes::f32_le(&body, transform + 24, "rotation")?,
            bytes::f32_le(&body, transform + 28, "rotation")?,
        ];
        let parent = bytes::u16_le(&body, parents + index * 2, "parent")? as i16;
        if rotation != [0.0, 0.0, 0.0, 1.0]
            || scale.iter().any(|value| *value < 0.0)
            || (index == 0 && parent != -1)
            || (index != 0 && parent != 0)
        {
            return Err("unsupported terrain bone transform or hierarchy".into());
        }
        if index == 0 && scale != [1.0; 3] {
            return Err("terrain root has unsupported scale".into());
        }
        let expected_matrix = [
            scale[0],
            0.0,
            0.0,
            0.0,
            0.0,
            scale[1],
            0.0,
            0.0,
            0.0,
            0.0,
            scale[2],
            0.0,
            translation[0],
            translation[1],
            translation[2],
            1.0,
        ];
        for (component, expected) in expected_matrix.iter().enumerate() {
            if bytes::f32_le(&body, matrices + index * 64 + component * 4, "matrix")? != *expected {
                return Err("matrix and terrain RTS disagree".into());
            }
        }
        let center = vector(&body, boxes + index * 64 + 32)?;
        let half = vector(&body, boxes + index * 64 + 48)?;
        if half.iter().any(|value| *value < 0.0) {
            return Err("negative terrain box half-size".into());
        }
        bones.push(Bone {
            translation,
            scale,
            bounds: [
                std::array::from_fn(|axis| center[axis] - half[axis]),
                std::array::from_fn(|axis| center[axis] + half[axis]),
            ],
        });
    }
    for index in 1..bones.len() {
        bones[index].translation =
            std::array::from_fn(|axis| bones[index].translation[axis] + bones[0].translation[axis]);
    }
    let mut stream_bones = BTreeMap::new();
    let mut active_primitives = BTreeSet::new();
    for index in 0..draw_count {
        let offset = draws_offset + index * 16;
        let primitive = read(offset)?;
        let bone = bytes::u16_le(&body, offset + 12, "draw bone")? as usize;
        let flags = bytes::u16_le(&body, offset + 14, "draw flags")?;
        if primitive >= primitive_count || bone >= bones.len() || flags & 7 != 0 {
            return Err("unsupported terrain draw reference or LOD".into());
        }
        let stream = read(primitives_offset + primitive * 100 + 80)?;
        if stream >= stream_count {
            return Err("terrain primitive stream out of range".into());
        }
        if let Some(previous) = stream_bones.insert(stream, bone) {
            if previous != bone {
                return Err("terrain stream drawn with multiple transforms".into());
            }
        }
        active_primitives.insert(primitive);
    }
    let mut streams = Vec::new();
    let mut vertices = Vec::new();
    let mut starts = Vec::new();
    let mut index_buffers = Vec::new();
    for index in 0..stream_count {
        let offset = streams_offset + index * 152;
        let bone_index = *stream_bones
            .get(&index)
            .ok_or("terrain stream lacks draw transform")?;
        let bone = bones[bone_index];
        let format = read(offset + 40)? as u32;
        let stride = read(offset + 120)?;
        let count = read(offset + 24)?;
        let expected_stride = match format {
            0x9a00_0001 => 4,
            0x9a00_0101 => 8,
            0x1a00_0002 => 12,
            0x1a00_0102 => 16,
            _ => return Err(format!("unsupported terrain vertex format {format:08x}")),
        };
        if stride != expected_stride
            || count.checked_mul(stride) != Some(read(offset + 104)?)
            || read(offset + 140)? != 1
        {
            return Err("invalid terrain stream sizes/topology".into());
        }
        let raw = bytes::slice(
            &data,
            read(offset + 88)?,
            read(offset + 104)?,
            "terrain vertices",
        )?;
        let index_count = read(offset + 144)?;
        if index_count.checked_mul(2) != Some(read(offset + 132)?) {
            return Err("invalid terrain index size".into());
        }
        let indices = bytes::slice(
            &data,
            read(offset + 128)?,
            read(offset + 132)?,
            "terrain indices",
        )?
        .chunks_exact(2)
        .map(|value| u16::from_le_bytes([value[0], value[1]]) as usize)
        .collect::<Vec<_>>();
        if indices.iter().any(|vertex| *vertex >= count) {
            return Err("terrain index outside vertex buffer".into());
        }
        let mut positions = Vec::with_capacity(count);
        for vertex in raw.chunks_exact(stride) {
            let position = if format & 0x8000_0000 != 0 {
                unpack_vertex(
                    [
                        bytes::u16_le(vertex, 0, "height")?,
                        bytes::u16_le(vertex, 2, "lattice")?,
                    ],
                    bone.translation,
                    bone.scale,
                )?
            } else {
                let local = vector(vertex, 0)?;
                std::array::from_fn(|axis| bone.translation[axis] + local[axis] * bone.scale[axis])
            };
            if position.iter().any(|value| !value.is_finite()) {
                return Err("non-finite decoded vertex".into());
            }
            let tolerance = 0.002
                + if format & 0x8000_0000 != 0 {
                    bone.scale[1] / 65535.0
                } else {
                    0.0
                };
            if (0..3).any(|axis| {
                position[axis] < bone.bounds[0][axis] - tolerance
                    || position[axis] > bone.bounds[1][axis] + tolerance
                    || position[axis] < bones[0].bounds[0][axis] - tolerance
                    || position[axis] > bones[0].bounds[1][axis] + tolerance
            }) {
                return Err("decoded terrain vertex outside stored boxes".into());
            }
            positions.push(position);
        }
        starts.push(vertices.len());
        vertices.extend_from_slice(&positions);
        streams.push(TerrainStream {
            format,
            bone: bone_index,
            positions,
            bounds: bone.bounds,
            quantization: if format & 0x8000_0000 != 0 {
                bone.scale[1] / 65535.0
            } else {
                0.0
            },
        });
        index_buffers.push(indices);
    }
    let mut unique = BTreeSet::new();
    let mut triangles = Vec::new();
    for primitive in active_primitives {
        let offset = primitives_offset + primitive * 100;
        if read(offset + 4)? != u32::MAX as usize {
            return Err("terrain primitive guard is not -1".into());
        }
        let stream = read(offset + 80)?;
        let first = read(offset + 88)?;
        let count = read(offset + 96)?;
        if count % 3 != 0 || read(offset + 92)?.checked_mul(3) != Some(count) {
            return Err("terrain primitive is not a triangle list".into());
        }
        let end = first.checked_add(count).ok_or("primitive range overflow")?;
        let indices = index_buffers[stream]
            .get(first..end)
            .ok_or("primitive range outside stream")?;
        for triangle in indices.chunks_exact(3) {
            let triangle = std::array::from_fn(|corner| starts[stream] + triangle[corner]);
            let mut key = triangle;
            key.sort();
            if unique.insert(key) {
                triangles.push(triangle);
            }
        }
    }
    if triangles.is_empty() {
        return Err("terrain mesh has no triangles".into());
    }
    Ok(TerrainMesh {
        name: header.name,
        streams,
        vertices,
        triangles,
        bounds: bones[0].bounds,
    })
}

pub fn triangle_height(vertices: [[f32; 3]; 3], world_x: f64, world_z: f64) -> Option<f64> {
    if !world_x.is_finite()
        || !world_z.is_finite()
        || vertices.iter().flatten().any(|value| !value.is_finite())
    {
        return None;
    }
    let [first, second, third] = vertices.map(|position| position.map(f64::from));
    let determinant = (second[2] - third[2]) * (first[0] - third[0])
        + (third[0] - second[0]) * (first[2] - third[2]);
    if determinant.abs() < 1e-12 {
        return None;
    }
    let weight_first = ((second[2] - third[2]) * (world_x - third[0])
        + (third[0] - second[0]) * (world_z - third[2]))
        / determinant;
    let weight_second = ((third[2] - first[2]) * (world_x - third[0])
        + (first[0] - third[0]) * (world_z - third[2]))
        / determinant;
    let weight_third = 1.0 - weight_first - weight_second;
    if [weight_first, weight_second, weight_third]
        .iter()
        .any(|weight| *weight < -1e-8)
    {
        return None;
    }
    Some(weight_first * first[1] + weight_second * second[1] + weight_third * third[1])
}

impl TerrainMesh {
    pub fn heights_at(&self, world_x: f64, world_z: f64) -> Vec<f64> {
        if world_x < f64::from(self.bounds[0][0]) - 0.001
            || world_x > f64::from(self.bounds[1][0]) + 0.001
            || world_z < f64::from(self.bounds[0][2]) - 0.001
            || world_z > f64::from(self.bounds[1][2]) + 0.001
        {
            return Vec::new();
        }
        let mut heights: Vec<_> = self
            .triangles
            .iter()
            .filter_map(|triangle| {
                triangle_height(triangle.map(|index| self.vertices[index]), world_x, world_z)
            })
            .collect();
        heights.sort_by(f64::total_cmp);
        heights.dedup_by(|left, right| (*left - *right).abs() < 1e-5);
        heights
    }

    pub fn clipped_triangles(
        &self,
        min_x: f64,
        min_z: f64,
        max_x: f64,
        max_z: f64,
    ) -> Result<Vec<[[f64; 3]; 3]>, String> {
        if [min_x, min_z, max_x, max_z]
            .iter()
            .any(|value| !value.is_finite())
            || min_x >= max_x
            || min_z >= max_z
        {
            return Err("invalid clip rectangle".into());
        }
        if max_x < f64::from(self.bounds[0][0])
            || min_x > f64::from(self.bounds[1][0])
            || max_z < f64::from(self.bounds[0][2])
            || min_z > f64::from(self.bounds[1][2])
        {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for triangle in &self.triangles {
            let mut polygon: Vec<_> = triangle
                .iter()
                .map(|index| self.vertices[*index].map(f64::from))
                .collect();
            for (axis, edge, keep_above) in [
                (0, min_x, true),
                (0, max_x, false),
                (2, min_z, true),
                (2, max_z, false),
            ] {
                let mut clipped = Vec::new();
                if let Some(mut previous) = polygon.last().copied() {
                    for current in &polygon {
                        let previous_inside = if keep_above {
                            previous[axis] >= edge
                        } else {
                            previous[axis] <= edge
                        };
                        let current_inside = if keep_above {
                            current[axis] >= edge
                        } else {
                            current[axis] <= edge
                        };
                        if previous_inside != current_inside {
                            let fraction =
                                (edge - previous[axis]) / (current[axis] - previous[axis]);
                            let mut intersection: [f64; 3] = std::array::from_fn(|component| {
                                previous[component]
                                    + fraction * (current[component] - previous[component])
                            });
                            intersection[axis] = edge;
                            clipped.push(intersection);
                        }
                        if current_inside {
                            clipped.push(*current);
                        }
                        previous = *current;
                    }
                }
                polygon = clipped;
            }
            for index in 1..polygon.len().saturating_sub(1) {
                let triangle = [polygon[0], polygon[index], polygon[index + 1]];
                let first_edge: [f64; 3] =
                    std::array::from_fn(|axis| triangle[1][axis] - triangle[0][axis]);
                let second_edge: [f64; 3] =
                    std::array::from_fn(|axis| triangle[2][axis] - triangle[0][axis]);
                let normal = [
                    first_edge[1] * second_edge[2] - first_edge[2] * second_edge[1],
                    first_edge[2] * second_edge[0] - first_edge[0] * second_edge[2],
                    first_edge[0] * second_edge[1] - first_edge[1] * second_edge[0],
                ];
                if normal
                    .iter()
                    .map(|component| component * component)
                    .sum::<f64>()
                    > 1e-24
                {
                    result.push(triangle);
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_msha() -> Vec<u8> {
        let mut body = vec![0; 936];
        let put = |data: &mut [u8], offset: usize, value: u32| {
            data[offset..offset + 4].copy_from_slice(&value.to_le_bytes())
        };
        put(&mut body, 204, 2);
        put(&mut body, 216, 1 | (1 << 16));
        put(&mut body, 232, 1);
        put(&mut body, 244, 14);
        for offset in [256, 264, 272] {
            put(&mut body, offset, 2);
        }
        for index in 0..2 {
            let translation: [f32; 3] = if index == 0 {
                [300.0, 0.0, -300.0]
            } else {
                [150.0, 10.0, 75.0]
            };
            let scale: [f32; 3] = if index == 0 {
                [1.0; 3]
            } else {
                [2.34375, 20.0, 2.34375]
            };
            let transform = 560 + index * 48;
            for axis in 0..3 {
                put(&mut body, transform + axis * 4, translation[axis].to_bits());
                put(&mut body, transform + 32 + axis * 4, scale[axis].to_bits());
                put(
                    &mut body,
                    304 + index * 64 + axis * 20,
                    scale[axis].to_bits(),
                );
                put(
                    &mut body,
                    304 + index * 64 + 48 + axis * 4,
                    translation[axis].to_bits(),
                );
                put(
                    &mut body,
                    432 + index * 64 + 32 + axis * 4,
                    [487.5f32, 20.0, -187.5][axis].to_bits(),
                );
                put(
                    &mut body,
                    432 + index * 64 + 48 + axis * 4,
                    [37.5f32, 10.0, 37.5][axis].to_bits(),
                );
            }
            put(&mut body, transform + 28, 1.0f32.to_bits());
            put(&mut body, 304 + index * 64 + 60, 1.0f32.to_bits());
        }
        body[656..658].copy_from_slice(&(-1i16).to_le_bytes());
        for (offset, value) in [
            (24, 3),
            (40, 0x9a000001),
            (104, 12),
            (120, 4),
            (128, 12),
            (132, 6),
            (140, 1),
            (144, 3),
        ] {
            put(&mut body, 668 + offset, value);
        }
        put(&mut body, 824, u32::MAX);
        put(&mut body, 912, 1);
        put(&mut body, 916, 3);
        body[932..934].copy_from_slice(&1u16.to_le_bytes());
        let mut data = Vec::new();
        for (height, lattice) in [(0u16, 0u16), (65535, 32), (0, 1056)] {
            data.extend_from_slice(&(height ^ 0x8000).to_le_bytes());
            data.extend_from_slice(&(lattice ^ 0x8000).to_le_bytes());
        }
        for index in [0u16, 1, 2] {
            data.extend_from_slice(&index.to_le_bytes());
        }
        let mut blob = vec![0; mesh::HEADER];
        blob[..4].copy_from_slice(mesh::MAGIC);
        for (offset, value) in [
            (4, body.len()),
            (8, data.len()),
            (12, body.len()),
            (16, data.len()),
        ] {
            put(&mut blob, offset, value as u32);
        }
        let name = b"France_ChunkSynthetic";
        blob[20..20 + name.len()].copy_from_slice(name);
        blob.extend_from_slice(&body);
        blob.extend_from_slice(&data);
        blob
    }

    #[test]
    fn complete_decoder_composes_root_and_child_translations() {
        let blob = synthetic_msha();
        let mesh = decode(&blob).unwrap();
        assert_eq!(
            mesh.vertices,
            [
                [450.0, 10.0, -225.0],
                [525.0, 30.0, -225.0],
                [450.0, 10.0, -150.0]
            ]
        );
        assert_eq!(mesh.triangles, [[0, 1, 2]]);
        assert!((mesh.heights_at(475.0, -200.0)[0] - 16.6666666667).abs() < 1e-8);
        for end in [276, 500, blob.len() - 1] {
            assert!(decode(&blob[..end]).is_err());
        }
        let mut bad_index = blob.clone();
        let end = bad_index.len();
        bad_index[end - 2..].copy_from_slice(&3u16.to_le_bytes());
        assert!(decode(&bad_index).is_err());
        let mut bad_format = blob;
        bad_format[mesh::HEADER + 708..mesh::HEADER + 712].copy_from_slice(&0u32.to_le_bytes());
        assert!(decode(&bad_format).is_err());
    }

    #[test]
    fn packed_endpoints_and_unsigned_bias() {
        assert_eq!(
            unpack_vertex(
                [0x8000, 0x8000],
                [10.0, 20.0, -30.0],
                [2.34375, 10.0, 2.34375]
            )
            .unwrap(),
            [10.0, 20.0, -30.0]
        );
        assert_eq!(
            unpack_vertex(
                [0x7fff, 1088 ^ 0x8000],
                [10.0, 20.0, -30.0],
                [2.34375, 10.0, 2.34375]
            )
            .unwrap(),
            [85.0, 30.0, 45.0]
        );
        assert!(unpack_vertex([0, 1089 ^ 0x8000], [0.0; 3], [1.0; 3]).is_err());
    }

    #[test]
    fn sampling_is_barycentric_not_bilinear_or_extrapolated() {
        let triangle = [[-10.0, 1.0, -20.0], [0.0, 3.0, -20.0], [-10.0, 5.0, -10.0]];
        assert_eq!(triangle_height(triangle, -7.5, -17.5), Some(2.5));
        assert_eq!(triangle_height(triangle, -10.0, -20.0), Some(1.0));
        assert_eq!(triangle_height(triangle, 0.0, -10.0), None);
        assert_eq!(triangle_height(triangle, f64::NAN, 0.0), None);
        assert_eq!(triangle_height([[0.0; 3]; 3], 0.0, 0.0), None);
    }

    #[test]
    fn truncated_input_returns_error() {
        for size in 0..276 {
            assert!(decode(&vec![0; size]).is_err());
        }
    }

    #[test]
    fn clip_preserves_plane_and_does_not_fill_missing_surface() {
        let mesh = TerrainMesh {
            name: "synthetic".into(),
            streams: Vec::new(),
            vertices: vec![
                [-10.0, -10.0, -10.0],
                [10.0, 10.0, -10.0],
                [-10.0, -10.0, 10.0],
            ],
            triangles: vec![[0, 1, 2]],
            bounds: [[-10.0; 3], [10.0; 3]],
        };
        let clipped = mesh.clipped_triangles(-5.0, -5.0, 5.0, 5.0).unwrap();
        assert!(!clipped.is_empty());
        for position in clipped.iter().flatten() {
            assert!((-5.0..=5.0).contains(&position[0]));
            assert!((-5.0..=5.0).contains(&position[2]));
            assert_eq!(position[1], position[0]);
            assert!(position[0] + position[2] <= 1e-12);
        }
        assert!(mesh
            .clipped_triangles(20.0, 20.0, 30.0, 30.0)
            .unwrap()
            .is_empty());
        assert!(mesh.clipped_triangles(0.0, 0.0, f64::NAN, 1.0).is_err());
    }

    #[test]
    fn export_retains_vertical_geometry_that_height_query_cannot_intersect() {
        let mesh = TerrainMesh {
            name: "vertical".into(),
            streams: Vec::new(),
            vertices: vec![[0.0, 0.0, -1.0], [0.0, 10.0, -1.0], [0.0, 0.0, 1.0]],
            triangles: vec![[0, 1, 2]],
            bounds: [[0.0, 0.0, -1.0], [0.0, 10.0, 1.0]],
        };
        assert!(mesh.heights_at(0.0, 0.5).is_empty());
        assert!(!mesh
            .clipped_triangles(-1.0, 0.0, 1.0, 1.0)
            .unwrap()
            .is_empty());
    }
}
