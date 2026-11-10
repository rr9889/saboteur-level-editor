use crate::bytes;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

pub const VERTEX_OFFSET: usize = 0x160;
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

pub fn read_record(stored: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    if expected > MAX_PAYLOAD_BYTES || stored.len() > MAX_PAYLOAD_BYTES {
        return Err("collision record exceeds 64 MiB inspection budget".into());
    }
    if stored.len() == expected {
        return Ok(stored.to_vec());
    }
    let mut decoder = flate2::read::ZlibDecoder::new(stored);
    let mut output = Vec::new();
    decoder
        .by_ref()
        .take(expected as u64 + 1)
        .read_to_end(&mut output)
        .map_err(|error| format!("collision zlib: {error}"))?;
    if output.len() != expected || decoder.total_in() as usize != stored.len() {
        return Err("collision zlib length mismatch or trailing data".into());
    }
    Ok(output)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Triangle {
    pub indices: [u16; 3],
    pub material: u16,
    pub flags: u16,
}

#[derive(Clone, Debug)]
pub struct BakedMesh<'a> {
    pub hash: u32,
    pub header: &'a [u8],
    pub vertices: Vec<[f32; 3]>,
    pub padding: &'a [u8],
    pub triangles: Vec<Triangle>,
    pub trailer: &'a [u8],
}

#[derive(Clone, Debug)]
pub struct EdgeItems<'a> {
    pub hash: u32,
    pub items: &'a [u8],
    pub count: usize,
}

#[derive(Clone, Debug)]
pub enum Payload<'a> {
    Edges(EdgeItems<'a>),
    Baked(BakedMesh<'a>),
}

fn extent(offset: usize, count: usize, stride: usize) -> Result<usize, String> {
    count
        .checked_mul(stride)
        .and_then(|size| offset.checked_add(size))
        .ok_or_else(|| "collision extent overflow".into())
}

pub fn decode(payload: &[u8]) -> Result<Payload<'_>, String> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err("collision payload exceeds 64 MiB inspection budget".into());
    }
    let hash = bytes::u32_le(payload, 0, "collision hash")?;
    let models = bytes::u32_le(payload, 4, "collision models")?;
    let other = bytes::u32_le(payload, 8, "collision other count")?;
    let count = bytes::u32_le(payload, 12, "collision edge count")? as usize;
    if models == 0 && other == 0 {
        let end = extent(16, count, 80)?;
        if end != payload.len() {
            return Err(format!(
                "collision edge items end at {end}, payload has {} bytes",
                payload.len()
            ));
        }
        return Ok(Payload::Edges(EdgeItems {
            hash,
            items: &payload[16..],
            count,
        }));
    }
    if models != 1 || other != 0 || count != 0 {
        return Err(format!("unsupported collision model profile ({models}, {other}, {count}); only one baked model supported"));
    }
    let header = bytes::slice(payload, 0, VERTEX_OFFSET, "collision baked header")?;
    for (offset, expected) in [
        (0x10, 5),
        (0x14, hash),
        (0xc0, 3),
        (0xc4, hash),
        (0xf0, 5),
        (0xf4, 1),
        (0x104, 0),
    ] {
        if bytes::u32_le(payload, offset, "collision baked profile")? != expected {
            return Err(format!(
                "unsupported collision baked header at 0x{offset:x}"
            ));
        }
    }
    let vertex_count = bytes::u32_le(payload, 0x100, "collision vertex count")? as usize;
    let triangle_count = bytes::u32_le(payload, 0x108, "collision triangle count")? as usize;
    if vertex_count == 0 || vertex_count > 65536 || triangle_count == 0 {
        return Err(format!(
            "unsupported collision counts: {vertex_count} vertices, {triangle_count} triangles"
        ));
    }
    let vertex_end = extent(VERTEX_OFFSET, vertex_count, 12)?;
    let triangle_offset = vertex_end
        .checked_add(15)
        .ok_or("collision alignment overflow")?
        & !15;
    let triangle_end = extent(triangle_offset, triangle_count, 10)?;
    bytes::slice(
        payload,
        VERTEX_OFFSET,
        triangle_end - VERTEX_OFFSET,
        "collision geometry",
    )?;
    let mut vertices = Vec::with_capacity(vertex_count);
    for index in 0..vertex_count {
        let offset = VERTEX_OFFSET + index * 12;
        let point = [
            bytes::f32_le(payload, offset, "collision x")?,
            bytes::f32_le(payload, offset + 4, "collision y")?,
            bytes::f32_le(payload, offset + 8, "collision z")?,
        ];
        if point.iter().any(|value| !value.is_finite()) {
            return Err(format!("collision vertex {index} is non-finite"));
        }
        vertices.push(point);
    }
    let mut triangles = Vec::with_capacity(triangle_count);
    for index in 0..triangle_count {
        let offset = triangle_offset + index * 10;
        let indices = [
            bytes::u16_le(payload, offset, "collision index")?,
            bytes::u16_le(payload, offset + 2, "collision index")?,
            bytes::u16_le(payload, offset + 4, "collision index")?,
        ];
        if indices.iter().any(|value| *value as usize >= vertex_count) {
            return Err(format!(
                "collision triangle {index} has an out-of-range index"
            ));
        }
        triangles.push(Triangle {
            indices,
            material: bytes::u16_le(payload, offset + 6, "collision material")?,
            flags: bytes::u16_le(payload, offset + 8, "collision flags")?,
        });
    }
    Ok(Payload::Baked(BakedMesh {
        hash,
        header,
        vertices,
        padding: &payload[vertex_end..triangle_offset],
        triangles,
        trailer: &payload[triangle_end..],
    }))
}

impl BakedMesh<'_> {
    pub fn reference_frame_supported(&self) -> bool {
        (0x50..0xc0).step_by(4).all(|offset| {
            let expected = if [0x6c, 0x8c, 0x90, 0xa0, 0xb0].contains(&offset) {
                1.0f32.to_bits()
            } else {
                0
            };
            bytes::u32_le(self.header, offset, "collision reference frame") == Ok(expected)
        })
    }
}

pub fn triangle_components(triangles: &[Triangle]) -> Result<Vec<Vec<usize>>, String> {
    let mut incident = BTreeMap::<u16, Vec<usize>>::new();
    for (index, triangle) in triangles.iter().enumerate() {
        for vertex in triangle.indices {
            incident.entry(vertex).or_default().push(index);
        }
    }
    let mut visited = vec![false; triangles.len()];
    let mut components = Vec::new();
    for first in 0..triangles.len() {
        if visited[first] {
            continue;
        }
        visited[first] = true;
        let mut pending = vec![first];
        let mut component = Vec::new();
        while let Some(index) = pending.pop() {
            component.push(index);
            for vertex in triangles[index].indices {
                if let Some(neighbors) = incident.remove(&vertex) {
                    for neighbor in neighbors {
                        if !visited[neighbor] {
                            visited[neighbor] = true;
                            pending.push(neighbor);
                        }
                    }
                }
            }
        }
        component.sort_unstable();
        components.push(component);
    }
    components.sort_by_key(|component| std::cmp::Reverse(component.len()));
    Ok(components)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Translation {
    pub payload: Vec<u8>,
    pub vertex_ids: Vec<usize>,
    pub changed_offsets: Vec<usize>,
}

pub fn translate_vertices(
    payload: &[u8],
    selected: &[usize],
    delta: [f32; 3],
) -> Result<Translation, String> {
    if delta.iter().any(|value| !value.is_finite()) {
        return Err("collision local delta must be finite".into());
    }
    if delta.iter().all(|&value| value == 0.0) {
        return Err("collision local delta must be nonzero".into());
    }
    let Payload::Baked(mesh) = decode(payload)? else {
        return Err("unsupported collision translation profile: requires one baked model".into());
    };
    if !mesh.reference_frame_supported() {
        return Err("unsupported collision reference frame for local translation".into());
    }
    if selected.is_empty() {
        return Err("collision triangle selection must not be empty".into());
    }
    let selection = topology(&mesh.triangles, selected)?;
    if selection.degenerate_faces != 0 {
        return Err("collision triangle selection contains repeated-index degenerate faces".into());
    }
    if selection.shared_vertices != 0 {
        return Err(
            "collision triangle selection has shared vertices with unselected triangles".into(),
        );
    }
    let vertex_ids: Vec<usize> = selected
        .iter()
        .flat_map(|&index| mesh.triangles[index].indices.map(usize::from))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut output = payload.to_vec();
    let mut changed_offsets = Vec::new();
    for &vertex in &vertex_ids {
        for (axis, &axis_delta) in delta.iter().enumerate() {
            if axis_delta == 0.0 {
                continue;
            }
            let translated = mesh.vertices[vertex][axis] + axis_delta;
            if !translated.is_finite() {
                return Err(format!(
                    "collision translated vertex {vertex} axis {axis} is non-finite (overflow)"
                ));
            }
            let offset = VERTEX_OFFSET + vertex * 12 + axis * 4;
            for (byte_index, value) in translated.to_le_bytes().into_iter().enumerate() {
                let byte_offset = offset + byte_index;
                if output[byte_offset] != value {
                    output[byte_offset] = value;
                    changed_offsets.push(byte_offset);
                }
            }
        }
    }
    Ok(Translation {
        payload: output,
        vertex_ids,
        changed_offsets,
    })
}

/// Receipt for an explicitly diagnostic cut/duplicate translation.
///
/// Appended vertex `original_vertex_count + j` is a copy of `old_vertex_ids[j]`.
/// The opaque header/trailer are preserved, not rebuilt or semantically validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutTranslation {
    pub payload: Vec<u8>,
    pub old_vertex_ids: Vec<usize>,
    pub selected_triangle_ids: Vec<usize>,
    pub original_vertex_count: usize,
    pub original_triangle_offset: usize,
    pub new_triangle_offset: usize,
}

/// Detach selected faces by appending translated copies of all their vertices.
///
/// This opt-in experiment permits shared vertices and open wall patches, unlike
/// `translate_vertices`. All original vertices and unselected triangles stay
/// unchanged. Only the vertex count and selected face indices are rewritten.
/// New alignment bytes are zero-filled. Nonzero old alignment bytes are refused
/// if their extent would change; otherwise they are preserved exactly.
///
/// This is NOT a game-compatible writer guarantee: opaque acceleration data,
/// bounds and internal offsets in the header/trailer are not updated. Callers
/// must retain diagnostic status until separately establishing their validity.
pub fn diagnostic_cut_and_translate(
    payload: &[u8],
    selected: &[usize],
    delta: [f32; 3],
) -> Result<CutTranslation, String> {
    if delta.iter().any(|value| !value.is_finite()) {
        return Err("collision local delta must be finite".into());
    }
    if delta.iter().all(|&value| value == 0.0) {
        return Err("collision local delta must be nonzero".into());
    }
    let Payload::Baked(mesh) = decode(payload)? else {
        return Err("unsupported collision cut profile: requires one baked model".into());
    };
    if !mesh.reference_frame_supported() {
        return Err("unsupported collision reference frame for cut translation".into());
    }
    if selected.is_empty() {
        return Err("collision triangle selection must not be empty".into());
    }
    if topology(&mesh.triangles, selected)?.degenerate_faces != 0 {
        return Err("collision triangle selection contains repeated-index degenerate faces".into());
    }
    let selected_triangle_ids: Vec<_> = selected
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let old_vertex_ids: Vec<_> = selected_triangle_ids
        .iter()
        .flat_map(|&face| mesh.triangles[face].indices.map(usize::from))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let original_vertex_count = mesh.vertices.len();
    let new_vertex_count = original_vertex_count
        .checked_add(old_vertex_ids.len())
        .ok_or("collision vertex count overflow")?;
    if new_vertex_count > 65536 {
        return Err("collision cut exceeds 16-bit vertex index capacity".into());
    }
    let old_vertex_end = extent(VERTEX_OFFSET, original_vertex_count, 12)?;
    let original_triangle_offset = old_vertex_end
        .checked_add(15)
        .ok_or("collision alignment overflow")?
        & !15;
    let new_vertex_end = extent(VERTEX_OFFSET, new_vertex_count, 12)?;
    let new_triangle_offset = new_vertex_end
        .checked_add(15)
        .ok_or("collision alignment overflow")?
        & !15;
    let new_padding_len = new_triangle_offset - new_vertex_end;
    if mesh.padding.len() != new_padding_len && mesh.padding.iter().any(|&byte| byte != 0) {
        return Err("collision cut would resize nonzero opaque alignment bytes".into());
    }
    let new_triangle_end = extent(new_triangle_offset, mesh.triangles.len(), 10)?;
    let new_size = new_triangle_end
        .checked_add(mesh.trailer.len())
        .ok_or("collision payload size overflow")?;
    if new_size > MAX_PAYLOAD_BYTES {
        return Err("collision cut exceeds 64 MiB payload budget".into());
    }
    let mut output = Vec::with_capacity(new_size);
    output.extend_from_slice(&payload[..old_vertex_end]);
    output[0x100..0x104].copy_from_slice(&(new_vertex_count as u32).to_le_bytes());
    let mut moved = false;
    for &old in &old_vertex_ids {
        for (axis, &axis_delta) in delta.iter().enumerate() {
            let before = mesh.vertices[old][axis];
            let after = if axis_delta == 0.0 {
                before
            } else {
                before + axis_delta
            };
            if !after.is_finite() {
                return Err(format!(
                    "collision translated vertex {old} axis {axis} is non-finite (overflow)"
                ));
            }
            moved |= before.to_bits() != after.to_bits();
            output.extend_from_slice(&after.to_le_bytes());
        }
    }
    if !moved {
        return Err("collision cut delta rounds away at every selected vertex".into());
    }
    if mesh.padding.len() == new_padding_len {
        output.extend_from_slice(mesh.padding);
    } else {
        output.resize(new_triangle_offset, 0);
    }
    let selected_set: BTreeSet<_> = selected_triangle_ids.iter().copied().collect();
    for (face, triangle) in mesh.triangles.iter().enumerate() {
        if selected_set.contains(&face) {
            for index in triangle.indices {
                let position = old_vertex_ids
                    .binary_search(&(index as usize))
                    .map_err(|_| "collision cut remap missing vertex")?;
                let remapped = u16::try_from(original_vertex_count + position)
                    .map_err(|_| "collision cut index overflow")?;
                output.extend_from_slice(&remapped.to_le_bytes());
            }
            output.extend_from_slice(&triangle.material.to_le_bytes());
            output.extend_from_slice(&triangle.flags.to_le_bytes());
        } else {
            let start = original_triangle_offset + face * 10;
            output.extend_from_slice(&payload[start..start + 10]);
        }
    }
    output.extend_from_slice(mesh.trailer);
    if output.len() != new_size {
        return Err("collision cut output size receipt mismatch".into());
    }
    // Reuse every parser bound/index/finite-value guard on the final representation.
    decode(&output)?;
    Ok(CutTranslation {
        payload: output,
        old_vertex_ids,
        selected_triangle_ids,
        original_vertex_count,
        original_triangle_offset,
        new_triangle_offset,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    pub faces: usize,
    pub used_vertices: usize,
    pub shared_vertices: usize,
    pub boundary_edges: usize,
    pub non_manifold_edges: usize,
    pub degenerate_faces: usize,
}

pub fn topology(triangles: &[Triangle], selected: &[usize]) -> Result<Topology, String> {
    let mut faces = BTreeSet::new();
    let mut used = BTreeSet::new();
    let mut edges = BTreeMap::new();
    let mut degenerate_faces = 0;
    for &index in selected {
        let triangle = triangles
            .get(index)
            .ok_or_else(|| format!("collision face {index} out of range"))?;
        if !faces.insert(index) {
            return Err(format!("duplicate collision face {index}"));
        }
        let [first, second, third] = triangle.indices;
        if first == second || second == third || first == third {
            degenerate_faces += 1;
        }
        used.extend(triangle.indices);
        for (start, end) in [(first, second), (second, third), (third, first)] {
            *edges
                .entry((start.min(end), start.max(end)))
                .or_insert(0usize) += 1;
        }
    }
    let others: BTreeSet<_> = triangles
        .iter()
        .enumerate()
        .filter(|(index, _)| !faces.contains(index))
        .flat_map(|(_, triangle)| triangle.indices)
        .collect();
    Ok(Topology {
        faces: faces.len(),
        used_vertices: used.len(),
        shared_vertices: used.intersection(&others).count(),
        boundary_edges: edges.values().filter(|&&count| count == 1).count(),
        non_manifold_edges: edges.values().filter(|&&count| count > 2).count(),
        degenerate_faces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn sample() -> Vec<u8> {
        let mut data = vec![0; 0x190 + 10];
        for (offset, value) in [
            (0, 0x1234),
            (4, 1),
            (0x10, 5),
            (0x14, 0x1234),
            (0xc0, 3),
            (0xc4, 0x1234),
            (0xf0, 5),
            (0xf4, 1),
            (0x100, 3),
            (0x108, 1),
        ] {
            put(&mut data, offset, value);
        }
        for offset in [0x6c, 0x8c, 0x90, 0xa0, 0xb0] {
            put(&mut data, offset, 1.0f32.to_bits());
        }
        put(&mut data, VERTEX_OFFSET + 12, 1.0f32.to_bits());
        put(&mut data, VERTEX_OFFSET + 28, 1.0f32.to_bits());
        data[0x184..0x190].fill(0xa5);
        for (index, value) in [0u16, 1, 2, 17, 29].iter().enumerate() {
            data[0x190 + index * 2..0x192 + index * 2].copy_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(b"opaque trailer");
        data
    }

    #[test]
    fn aligned_geometry_keeps_metadata_and_opaque_bytes() {
        let data = sample();
        let original = data.clone();
        let Payload::Baked(mesh) = decode(&data).unwrap() else {
            panic!()
        };
        assert_eq!(
            mesh.vertices,
            [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
        );
        assert_eq!(
            mesh.triangles[0],
            Triangle {
                indices: [0, 1, 2],
                material: 17,
                flags: 29
            }
        );
        assert_eq!(mesh.padding, &[0xa5; 12]);
        assert_eq!(mesh.trailer, b"opaque trailer");
        assert_eq!(mesh.header, &data[..VERTEX_OFFSET]);
        assert!(mesh.reference_frame_supported());
        assert_eq!(data, original);
    }

    fn mesh_sample(vertices: &[[f32; 3]], triangles: &[[u16; 3]]) -> Vec<u8> {
        let vertex_end = VERTEX_OFFSET + vertices.len() * 12;
        let triangle_offset = (vertex_end + 15) & !15;
        let mut data = sample()[..VERTEX_OFFSET].to_vec();
        data.resize(triangle_offset + triangles.len() * 10, 0);
        put(&mut data, 0x20, 0xa1b2c3d4);
        put(&mut data, 0x100, vertices.len() as u32);
        put(&mut data, 0x108, triangles.len() as u32);
        for (vertex, coordinates) in vertices.iter().enumerate() {
            for (axis, coordinate) in coordinates.iter().enumerate() {
                put(
                    &mut data,
                    VERTEX_OFFSET + vertex * 12 + axis * 4,
                    coordinate.to_bits(),
                );
            }
        }
        data[vertex_end..triangle_offset].fill(0xa5);
        for (index, indices) in triangles.iter().enumerate() {
            for (field, value) in indices
                .iter()
                .copied()
                .chain([0x9138 + index as u16, 0xfedc])
                .enumerate()
            {
                let offset = triangle_offset + index * 10 + field * 2;
                data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
            }
        }
        data.extend_from_slice(b"\0opaque trailer\xff\x80");
        data
    }

    #[test]
    fn components_match_stable_size_sort_and_first_triangle_ties() {
        let triangles = [
            [0, 1, 2],
            [10, 11, 12],
            [20, 21, 22],
            [22, 23, 24],
            [30, 31, 32],
            [12, 13, 14],
            [2, 3, 4],
            [24, 25, 26],
        ]
        .map(|indices| Triangle {
            indices,
            material: 0,
            flags: 0,
        });
        let expected = vec![vec![2, 3, 7], vec![0, 6], vec![1, 5], vec![4]];
        for _ in 0..3 {
            assert_eq!(triangle_components(&triangles).unwrap(), expected);
        }
        assert!(triangle_components(&[]).unwrap().is_empty());
        let repeated = [Triangle {
            indices: [u16::MAX, u16::MAX, 0],
            material: 0,
            flags: 0,
        }];
        assert_eq!(triangle_components(&repeated).unwrap(), vec![vec![0]]);
    }

    #[test]
    fn diagnostic_cut_detaches_shared_seam_and_preserves_other_geometry() {
        let vertices = [
            [0.0, -0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let mut data = mesh_sample(&vertices, &[[0, 1, 2], [0, 2, 3]]);
        let before = data.clone();
        assert!(translate_vertices(&data, &[0], [3.0, 0.0, -2.0]).is_err());
        let result = diagnostic_cut_and_translate(&data, &[0], [3.0, 0.0, -2.0]).unwrap();
        assert_eq!(result.old_vertex_ids, [0, 1, 2]);
        assert_eq!(result.selected_triangle_ids, [0]);
        assert_eq!(result.original_vertex_count, 4);
        assert_eq!(result.original_triangle_offset, 0x190);
        assert_eq!(result.new_triangle_offset, 0x1c0);
        assert_eq!(result.payload.len(), data.len() + 48);
        let Payload::Baked(after) = decode(&result.payload).unwrap() else {
            panic!()
        };
        let Payload::Baked(original) = decode(&data).unwrap() else {
            panic!()
        };
        assert_eq!(after.vertices.len(), 7);
        assert_eq!(
            &result.payload[VERTEX_OFFSET..VERTEX_OFFSET + 48],
            &data[VERTEX_OFFSET..VERTEX_OFFSET + 48]
        );
        assert_eq!(after.vertices[4], [3.0, -0.0, -2.0]);
        assert_eq!(after.vertices[4][1].to_bits(), (-0.0f32).to_bits());
        assert_eq!(after.vertices[5], [4.0, 0.0, -2.0]);
        assert_eq!(after.vertices[6], [4.0, 1.0, -2.0]);
        assert_eq!(after.triangles[0].indices, [4, 5, 6]);
        assert_eq!(after.triangles[1], original.triangles[1]);
        assert_eq!(after.triangles[0].material, original.triangles[0].material);
        assert_eq!(after.triangles[0].flags, original.triangles[0].flags);
        assert_eq!(after.trailer, original.trailer);
        assert_eq!(&after.header[..0x100], &original.header[..0x100]);
        assert_eq!(&after.header[0x104..], &original.header[0x104..]);
        assert_eq!(after.padding, [0; 12]);
        assert_eq!(data, before);
        // Reordering a multi-face request cannot change append order or bytes.
        // Four vertices append 48 bytes, retaining original alignment extent.
        data = mesh_sample(&[[0.0; 3]; 4], &[[0, 1, 2], [0, 2, 3]]);
        assert_eq!(
            diagnostic_cut_and_translate(&data, &[1, 0], [1.0, 0.0, 0.0]).unwrap(),
            diagnostic_cut_and_translate(&data, &[0, 1], [1.0, 0.0, 0.0]).unwrap()
        );
    }

    #[test]
    fn diagnostic_cut_padding_is_preserved_or_rejected_when_opaque() {
        let data = mesh_sample(&[[0.0; 3]; 5], &[[0, 1, 2], [0, 2, 3]]);
        let result = diagnostic_cut_and_translate(&data, &[0, 1], [1.0, 0.0, 0.0]).unwrap();
        let Payload::Baked(after) = decode(&result.payload).unwrap() else {
            panic!()
        };
        assert_eq!(after.padding, [0xa5; 4]);
        assert!(diagnostic_cut_and_translate(&data, &[0], [1.0, 0.0, 0.0])
            .unwrap_err()
            .contains("opaque alignment"));
        let mut zero_padding = data;
        zero_padding[VERTEX_OFFSET + 5 * 12..0x1a0].fill(0);
        let result = diagnostic_cut_and_translate(&zero_padding, &[0], [1.0, 0.0, 0.0]).unwrap();
        let Payload::Baked(after) = decode(&result.payload).unwrap() else {
            panic!()
        };
        assert!(after.padding.is_empty());
    }

    #[test]
    fn diagnostic_cut_rejects_invalid_requests_and_rounding_away() {
        let data = mesh_sample(&[[0.0; 3]; 4], &[[0, 1, 2], [0, 2, 3]]);
        for selected in [vec![], vec![0, 0], vec![2], vec![usize::MAX]] {
            assert!(diagnostic_cut_and_translate(&data, &selected, [1.0, 0.0, 0.0]).is_err());
        }
        for delta in [[0.0; 3], [f32::NAN, 0.0, 0.0], [0.0, f32::INFINITY, 0.0]] {
            assert!(diagnostic_cut_and_translate(&data, &[0], delta).is_err());
        }
        let mut bad_frame = data;
        put(&mut bad_frame, 0x50, 2.0f32.to_bits());
        assert!(
            diagnostic_cut_and_translate(&bad_frame, &[0], [1.0, 0.0, 0.0])
                .unwrap_err()
                .contains("reference frame")
        );
        let degenerate = mesh_sample(&[[0.0; 3]; 4], &[[0, 0, 2]]);
        assert!(
            diagnostic_cut_and_translate(&degenerate, &[0], [1.0, 0.0, 0.0])
                .unwrap_err()
                .contains("degenerate")
        );
        let extreme = mesh_sample(&[[f32::MAX; 3]; 4], &[[0, 1, 2]]);
        assert!(
            diagnostic_cut_and_translate(&extreme, &[0], [f32::MAX, 0.0, 0.0])
                .unwrap_err()
                .contains("overflow")
        );
        assert!(
            diagnostic_cut_and_translate(&extreme, &[0], [f32::from_bits(1), 0.0, 0.0])
                .unwrap_err()
                .contains("rounds away")
        );
        let saturated = mesh_sample(&vec![[0.0; 3]; 65536], &[[0, 1, 2]]);
        assert!(
            diagnostic_cut_and_translate(&saturated, &[0], [1.0, 0.0, 0.0])
                .unwrap_err()
                .contains("16-bit")
        );
    }

    #[test]
    fn translation_preserves_every_byte_outside_selected_float_words() {
        let vertices = [
            [1.0, 2.0, 3.0],
            [4.0, -0.0, 6.0],
            [7.0, 8.0, 9.0],
            [10.0, 11.0, 12.0],
            [13.0, 14.0, 15.0],
            [16.0, 17.0, 18.0],
            [19.0, 20.0, 21.0],
        ];
        let data = mesh_sample(&vertices, &[[0, 2, 5], [1, 4, 6], [4, 1, 6]]);
        let original = data.clone();
        let result = translate_vertices(&data, &[2, 1], [1.25, -0.0, -2.5]).unwrap();
        assert_eq!(result.vertex_ids, vec![1, 4, 6]);
        let mut expected = original.clone();
        for vertex in [1, 4, 6] {
            put(
                &mut expected,
                VERTEX_OFFSET + vertex * 12,
                (vertices[vertex][0] + 1.25).to_bits(),
            );
            put(
                &mut expected,
                VERTEX_OFFSET + vertex * 12 + 8,
                (vertices[vertex][2] - 2.5).to_bits(),
            );
        }
        assert_eq!(result.payload, expected);
        let actual_offsets: Vec<usize> = original
            .iter()
            .zip(&result.payload)
            .enumerate()
            .filter_map(|(offset, (before, after))| (before != after).then_some(offset))
            .collect();
        assert!(!actual_offsets.is_empty());
        assert_eq!(result.changed_offsets, actual_offsets);
        assert!(result
            .changed_offsets
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        for &offset in &result.changed_offsets {
            let relative = offset - VERTEX_OFFSET;
            assert!([1, 4, 6].contains(&(relative / 12)));
            assert!(relative % 12 < 4 || relative % 12 >= 8);
        }
        let Payload::Baked(before) = decode(&data).unwrap() else {
            panic!()
        };
        let Payload::Baked(after) = decode(&result.payload).unwrap() else {
            panic!()
        };
        assert_eq!(before.header, after.header);
        assert_eq!(before.padding, after.padding);
        assert_eq!(after.padding, &[0xa5; 12]);
        assert_eq!(before.triangles, after.triangles);
        assert_eq!(before.trailer, after.trailer);
        assert_eq!(after.trailer, b"\0opaque trailer\xff\x80");
        assert_eq!(after.vertices[1][1].to_bits(), (-0.0f32).to_bits());
        assert_eq!(data, original);
        assert_eq!(
            result,
            translate_vertices(&data, &[1, 2], [1.25, 0.0, -2.5]).unwrap()
        );
    }

    #[test]
    fn translation_rejects_empty_duplicate_and_out_of_range_selections() {
        let data = mesh_sample(&[[0.0; 3]; 6], &[[0, 1, 2], [3, 4, 5]]);
        for (selected, message) in [
            (vec![], "empty"),
            (vec![0, 0], "duplicate"),
            (vec![0, 1, 0], "duplicate"),
            (vec![2], "out of range"),
            (vec![usize::MAX], "out of range"),
        ] {
            let error = translate_vertices(&data, &selected, [1.0, 0.0, 0.0]).unwrap_err();
            assert!(error.contains(message), "{selected:?}: {error}");
        }
        let combined = translate_vertices(&data, &[1, 0], [1.0, 0.0, 0.0]).unwrap();
        assert_eq!(combined.vertex_ids, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn translation_rejects_a_single_shared_vertex_but_accepts_whole_components() {
        let data = mesh_sample(&[[0.0; 3]; 7], &[[0, 2, 4], [4, 5, 6]]);
        for selected in [[0], [1]] {
            let error = translate_vertices(&data, &selected, [0.0, 1.0, 0.0]).unwrap_err();
            assert!(error.contains("shared vertices"));
        }
        let result = translate_vertices(&data, &[1, 0], [0.0, 1.0, 0.0]).unwrap();
        assert_eq!(result.vertex_ids, vec![0, 2, 4, 5, 6]);
    }

    #[test]
    fn translation_rejects_each_repeated_index_degeneracy() {
        for indices in [[0, 0, 1], [0, 1, 0], [1, 0, 0], [0, 0, 0]] {
            let data = mesh_sample(&[[0.0; 3]; 3], &[indices]);
            let error = translate_vertices(&data, &[0], [1.0, 0.0, 0.0]).unwrap_err();
            assert!(error.contains("repeated-index degenerate"), "{error}");
        }
    }

    #[test]
    fn translation_requires_finite_nonzero_delta() {
        let data = sample();
        for delta in [[0.0; 3], [-0.0, 0.0, -0.0]] {
            assert!(translate_vertices(&data, &[0], delta)
                .unwrap_err()
                .contains("nonzero"));
        }
        for axis in 0..3 {
            for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut delta = [1.0; 3];
                delta[axis] = value;
                assert!(translate_vertices(&data, &[0], delta)
                    .unwrap_err()
                    .contains("finite"));
            }
        }
    }

    #[test]
    fn translation_rejects_finite_overflow_without_mutating_input() {
        for axis in 0..3 {
            for extreme in [f32::MAX, -f32::MAX] {
                let mut vertices = [[0.0; 3]; 3];
                vertices[1][axis] = extreme;
                let data = mesh_sample(&vertices, &[[0, 1, 2]]);
                let original = data.clone();
                let mut delta = [0.0; 3];
                delta[axis] = extreme;
                let error = translate_vertices(&data, &[0], delta).unwrap_err();
                assert!(error.contains("overflow"), "{error}");
                assert_eq!(data, original);
            }
        }
    }

    #[test]
    fn translation_reports_exact_empty_diff_when_nonzero_delta_rounds_away() {
        let data = mesh_sample(&[[f32::MAX; 3]; 3], &[[0, 1, 2]]);
        let result = translate_vertices(&data, &[0], [f32::from_bits(1), 0.0, 0.0]).unwrap();
        assert_eq!(result.payload, data);
        assert_eq!(result.vertex_ids, vec![0, 1, 2]);
        assert!(result.changed_offsets.is_empty());
    }

    #[test]
    fn translation_reuses_decoder_guards_and_requires_supported_reference_frame() {
        let data = sample();
        for length in [0, 15, 0xff, 0x15f, 0x180, 0x199] {
            assert!(translate_vertices(&data[..length], &[0], [1.0, 0.0, 0.0]).is_err());
        }
        for (offset, value) in [
            (4, 2),
            (8, 1),
            (12, 1),
            (0x10, 7),
            (0x14, 0),
            (0x104, 1),
            (0x100, u32::MAX),
            (0x108, u32::MAX),
            (VERTEX_OFFSET, f32::NAN.to_bits()),
            (VERTEX_OFFSET + 4, f32::INFINITY.to_bits()),
        ] {
            let mut bad = data.clone();
            put(&mut bad, offset, value);
            assert!(translate_vertices(&bad, &[0], [1.0, 0.0, 0.0]).is_err());
        }
        let mut bad_index = data.clone();
        bad_index[0x190..0x192].copy_from_slice(&3u16.to_le_bytes());
        assert!(translate_vertices(&bad_index, &[0], [1.0, 0.0, 0.0]).is_err());
        for offset in (0x50..0xc0).step_by(4) {
            let mut bad_frame = data.clone();
            put(&mut bad_frame, offset, 2.0f32.to_bits());
            assert!(translate_vertices(&bad_frame, &[0], [1.0, 0.0, 0.0])
                .unwrap_err()
                .contains("reference frame"));
        }
        let edges = vec![0; 16];
        assert!(translate_vertices(&edges, &[0], [1.0, 0.0, 0.0])
            .unwrap_err()
            .contains("profile"));
    }

    #[test]
    fn truncated_and_unsupported_profiles_are_errors() {
        let data = sample();
        for length in [0, 15, 0xff, 0x15f, 0x180, 0x199] {
            assert!(decode(&data[..length]).is_err(), "accepted {length} bytes");
        }
        for (offset, value) in [
            (4, 2),
            (8, 1),
            (12, 1),
            (0x10, 7),
            (0x14, 0),
            (0x104, 1),
            (0x100, u32::MAX),
            (0x108, u32::MAX),
        ] {
            let mut bad = data.clone();
            put(&mut bad, offset, value);
            assert!(decode(&bad).is_err(), "accepted field {offset:x}");
        }
    }

    #[test]
    fn invalid_vertices_indices_and_reference_frames_are_not_drawn() {
        let mut data = sample();
        put(&mut data, VERTEX_OFFSET, f32::NAN.to_bits());
        assert!(decode(&data).unwrap_err().contains("non-finite"));
        data = sample();
        data[0x190..0x192].copy_from_slice(&3u16.to_le_bytes());
        assert!(decode(&data).unwrap_err().contains("out-of-range"));
        data = sample();
        put(&mut data, 0x50, 1.0f32.to_bits());
        let Payload::Baked(mesh) = decode(&data).unwrap() else {
            panic!()
        };
        assert!(!mesh.reference_frame_supported());
    }

    #[test]
    fn edge_items_are_opaque_not_invented_triangles() {
        let mut data = vec![0; 96];
        put(&mut data, 12, 1);
        put(&mut data, 80, f32::NAN.to_bits());
        let Payload::Edges(edges) = decode(&data).unwrap() else {
            panic!()
        };
        assert_eq!(edges.count, 1);
        assert_eq!(edges.items, &data[16..]);
        data.push(0);
        assert!(decode(&data).is_err());
    }

    #[test]
    fn compressed_record_is_bounded_and_exact() {
        let data = sample();
        let compressed = crate::zlib::deflate(&data, 9);
        assert_eq!(read_record(&compressed, data.len()).unwrap(), data);
        assert_eq!(read_record(&data, data.len()).unwrap(), data);
        assert!(read_record(&compressed, 20).is_err());
        assert!(read_record(&compressed, data.len() + 1).is_err());
        assert!(read_record(&compressed, MAX_PAYLOAD_BYTES + 1).is_err());
        let mut padded = compressed;
        padded.push(0);
        assert!(read_record(&padded, data.len()).is_err());
    }

    #[test]
    fn subset_reports_shared_vertices_and_open_shells() {
        let triangles = [[0, 1, 2], [2, 1, 3]].map(|indices| Triangle {
            indices,
            material: 0,
            flags: 0,
        });
        let subset = topology(&triangles, &[0]).unwrap();
        assert_eq!(
            (
                subset.used_vertices,
                subset.shared_vertices,
                subset.boundary_edges
            ),
            (3, 2, 3)
        );
        let whole = topology(&triangles, &[0, 1]).unwrap();
        assert_eq!((whole.shared_vertices, whole.boundary_edges), (0, 4));
        assert!(topology(&triangles, &[2]).is_err());
        assert!(topology(&triangles, &[0, 0]).is_err());
    }

    #[test]
    fn closed_and_nonmanifold_geometry_stay_distinct() {
        let triangles = [[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]].map(|indices| Triangle {
            indices,
            material: 0,
            flags: 0,
        });
        let closed = topology(&triangles, &[0, 1, 2, 3]).unwrap();
        assert_eq!((closed.boundary_edges, closed.non_manifold_edges), (0, 0));
        let mut invalid = triangles.to_vec();
        invalid.push(triangles[0].clone());
        let result = topology(&invalid, &[0, 1, 2, 3, 4]).unwrap();
        assert_eq!(result.non_manifold_edges, 3);
        invalid.push(Triangle {
            indices: [0, 0, 1],
            material: 0,
            flags: 0,
        });
        assert_eq!(topology(&invalid, &[5]).unwrap().degenerate_faces, 1);
    }
}
