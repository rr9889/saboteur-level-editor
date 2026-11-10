#![cfg(feature = "retail")]

use std::path::Path;

use sab_formats::game_paths::local_config_retail_dir;
use sab_tools::collision;

#[test]
fn m6b_core_only_diagnostic_preserves_other_records_and_ground() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mounts = sab_tools::cells::france_mounts(&game).unwrap();
    let cell = 66_974_465;
    let (mount, entry) =
        sab_tools::cells::resolve_entry(&mounts, sab_tools::cells::cell_crc(cell)).unwrap();
    let source = sab_formats::megapack::entry_slice(&mount.map, entry).unwrap();
    assert_eq!(
        sab_tools::stage::sha256(source),
        "57CCE6ABE2200A473DAE98088973A78433CB3614C9B34A3C6074B8EEB2622C58"
    );
    let request = sab_tools::collision_stage::plan_subpack(
        source,
        cell,
        &mount.rel,
        0x99b3dfc9,
        12,
        [0.0, 10.0, 0.0],
    )
    .unwrap();
    assert_eq!(request["record"], 495);
    assert_eq!(request["triangle_ids"].as_array().unwrap().len(), 127);
    assert_eq!(request["selection_evidence"]["component_count"], 589);
    let pinned: serde_json::Value = serde_json::from_str(include_str!(
        "../../../docs/level_editor/m6b_core_dry_run.json"
    ))
    .unwrap();
    assert_eq!(request["triangle_ids"], pinned["triangle_ids"]);
    let result = sab_tools::collision_stage::dry_run_subpack(source, &pinned).unwrap();
    assert_eq!(
        result.manifest["selected_vertex_ids"]
            .as_array()
            .unwrap()
            .len(),
        85
    );
    let outer = sab_formats::megapack::parse(&result.pack).unwrap();
    let after = sab_formats::megapack::entry_slice(&result.pack, &outer.entries[0]).unwrap();
    let before_pack = sab_formats::sbla::parse(source, None).unwrap();
    let after_pack = sab_formats::sbla::parse(after, None).unwrap();
    let before_payload = sab_formats::collision::read_record(
        before_pack.blobs[495],
        before_pack.records[495].uncomp as usize,
    )
    .unwrap();
    let after_payload = sab_formats::collision::read_record(
        after_pack.blobs[495],
        after_pack.records[495].uncomp as usize,
    )
    .unwrap();
    let sab_formats::collision::Payload::Baked(before_mesh) =
        sab_formats::collision::decode(&before_payload).unwrap()
    else {
        panic!("expected baked")
    };
    let sab_formats::collision::Payload::Baked(after_mesh) =
        sab_formats::collision::decode(&after_payload).unwrap()
    else {
        panic!("expected baked")
    };
    let components = sab_formats::collision::triangle_components(&before_mesh.triangles).unwrap();
    assert_eq!(components[6].len(), 222);
    for &triangle in &components[6] {
        for vertex in before_mesh.triangles[triangle].indices {
            assert_eq!(
                before_mesh.vertices[vertex as usize].map(f32::to_bits),
                after_mesh.vertices[vertex as usize].map(f32::to_bits)
            );
        }
    }
    assert_eq!(before_mesh.trailer, after_mesh.trailer);
}

#[test]
fn documented_baked_geometry_is_decoded_and_placed_read_only() {
    let game = local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    for cell in [1281, 1_700_865, 66_974_465] {
        let inspected = collision::inspect_cell(&game, cell).unwrap();
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&collision::report(&inspected, false)).unwrap()
        );
        assert!(
            !inspected.surfaces.is_empty(),
            "cell {cell}: {:?}",
            inspected.issues
        );
        for surface in &inspected.surfaces {
            assert!(surface
                .vertices
                .iter()
                .flatten()
                .all(|value| value.is_finite()));
            assert!(surface
                .triangles
                .iter()
                .flatten()
                .all(|&index| (index as usize) < surface.vertices.len()));
        }
        if cell == 1281 {
            let target = inspected
                .surfaces
                .iter()
                .find(|surface| surface.hash == 0x8b3cef60)
                .expect("documented baked mesh");
            assert_eq!(
                (target.vertices.len(), target.triangles.len()),
                (4290, 6068)
            );
            let triangles: Vec<_> = target
                .triangles
                .iter()
                .zip(&target.triangle_metadata)
                .map(|(&indices, metadata)| sab_formats::collision::Triangle {
                    indices,
                    material: metadata[0],
                    flags: metadata[1],
                })
                .collect();
            let subset =
                sab_formats::collision::topology(&triangles, &[1253, 1254, 1255, 1256, 3547])
                    .unwrap();
            assert_eq!(
                (
                    subset.used_vertices,
                    subset.boundary_edges,
                    subset.non_manifold_edges
                ),
                (7, 7, 0)
            );
            assert!(
                subset.shared_vertices > 0,
                "building candidate is not an independent closed collider"
            );
        }
    }
}
