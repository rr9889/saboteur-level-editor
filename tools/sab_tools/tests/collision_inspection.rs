use sab_formats::editnodes::tags;
use sab_formats::{map6, pandemic_hash, sbla};
use sab_tools::collision::{inspect_subpack, report};

const NAME: &str = "France_Streamblock_1281_baked_collision0";

#[test]
fn diagnostic_writer_proves_nested_roundtrips_and_only_vertex_byte_changes() {
    let mut payload = baked();
    payload.extend_from_slice(&[0x38, 0x38, 0x91, 0, 0xff, 0x42]);
    let source = fixture(&params(), &payload, NAME, false);
    let request = sab_tools::collision_stage::plan_subpack(
        &source,
        1281,
        "synthetic",
        pandemic_hash(NAME),
        0,
        [0.0, 10.0, 0.0],
    )
    .unwrap();
    let result = sab_tools::collision_stage::dry_run_subpack(&source, &request).unwrap();
    assert_eq!(result.manifest["installable"], false);
    assert_eq!(result.manifest["runtime_verified"], false);
    assert_eq!(
        result.manifest["selected_vertex_ids"],
        serde_json::json!([0, 1, 2])
    );
    assert_eq!(result.manifest["trailer"]["bytes"], 6);
    assert_eq!(
        result.manifest["trailer"]["sha256"],
        sab_tools::stage::sha256(&payload[0x19a..])
    );
    let outer = sab_formats::megapack::parse(&result.pack).unwrap();
    let edited = sab_formats::megapack::entry_slice(&result.pack, &outer.entries[0]).unwrap();
    let decoded = inspect_subpack(edited, 1281, "synthetic").unwrap();
    assert_eq!(
        decoded.surfaces[0].vertices,
        [
            [30.0, 10.0, 330.0],
            [31.0, 10.0, 330.0],
            [30.0, 11.0, 330.0]
        ]
    );
    assert_eq!(
        inspect_subpack(&source, 1281, "synthetic")
            .unwrap()
            .surfaces[0]
            .vertices[0],
        [30.0, 0.0, 330.0]
    );
}

#[test]
fn diagnostic_requests_reject_stale_identity_and_invalid_selections() {
    let source = fixture(&params(), &baked(), NAME, false);
    let request = sab_tools::collision_stage::plan_subpack(
        &source,
        1281,
        "synthetic",
        pandemic_hash(NAME),
        0,
        [0.0, 10.0, 0.0],
    )
    .unwrap();
    for (key, value) in [
        ("selection_evidence", serde_json::json!({"component":1})),
        ("source_subpack_sha256", serde_json::json!("stale")),
        ("tag", serde_json::json!("00000000")),
        ("record", serde_json::json!(10000)),
        ("triangle_ids", serde_json::json!([])),
        ("triangle_ids", serde_json::json!([0, 0])),
        ("triangle_ids", serde_json::json!([500])),
        ("delta_world", serde_json::json!([0, 0, 0])),
    ] {
        let mut invalid = request.clone();
        invalid[key] = value;
        assert!(
            sab_tools::collision_stage::dry_run_subpack(&source, &invalid).is_err(),
            "accepted invalid {key}"
        );
    }
    assert!(sab_tools::collision_stage::plan_subpack(
        &source,
        1281,
        "synthetic",
        pandemic_hash(NAME),
        1,
        [0.0, 10.0, 0.0]
    )
    .is_err());
}

#[test]
fn diagnostic_world_translation_uses_inverse_basis() {
    let mut fields = params();
    for (tag, vector) in [
        (tags::X_AXIS, [0.0f32, 0.0, -2.0]),
        (tags::Y_AXIS, [0.0, 3.0, 0.0]),
        (tags::Z_AXIS, [4.0, 0.0, 0.0]),
    ] {
        fields
            .iter_mut()
            .find(|(field, _)| *field == tag)
            .unwrap()
            .1 = vector.into_iter().flat_map(f32::to_le_bytes).collect();
    }
    let source = fixture(&fields, &baked(), NAME, false);
    let request = sab_tools::collision_stage::plan_subpack(
        &source,
        1281,
        "synthetic",
        pandemic_hash(NAME),
        0,
        [8.0, 9.0, -4.0],
    )
    .unwrap();
    let result = sab_tools::collision_stage::dry_run_subpack(&source, &request).unwrap();
    assert_eq!(
        result.manifest["delta_local"],
        serde_json::json!([2.0, 3.0, 2.0])
    );
    assert_eq!(result.manifest["checks"]["maximum_world_error_m"], 0.0);
}

#[test]
fn diagnostic_writer_supports_raw_record_and_preserves_signed_zero_axes() {
    let mut payload = baked();
    put(&mut payload, 0x160, (-0.0f32).to_bits());
    let compressed_source = fixture(&params(), &payload, NAME, false);
    let parsed = sbla::parse(&compressed_source, None).unwrap();
    let mut blobs = parsed.blobs();
    blobs[1].data = &payload;
    let source = sbla::write(parsed.descriptor.as_ref(), &blobs).unwrap();
    let request = sab_tools::collision_stage::plan_subpack(
        &source,
        1281,
        "synthetic",
        pandemic_hash(NAME),
        0,
        [0.0, 10.0, 0.0],
    )
    .unwrap();
    let result = sab_tools::collision_stage::dry_run_subpack(&source, &request).unwrap();
    let outer = sab_formats::megapack::parse(&result.pack).unwrap();
    let edited = sab_formats::megapack::entry_slice(&result.pack, &outer.entries[0]).unwrap();
    let after = sbla::parse(edited, None).unwrap();
    assert_eq!(after.records[1].comp, after.records[1].uncomp);
    assert_eq!(&after.blobs[1][0x160..0x164], &(-0.0f32).to_le_bytes());
}

#[test]
fn diagnostic_request_cannot_relabel_another_component_or_union() {
    let mut payload = vec![0; 0x1c4];
    payload[..0x160].copy_from_slice(&baked()[..0x160]);
    put(&mut payload, 0x100, 6);
    put(&mut payload, 0x108, 2);
    for (vertex, point) in [
        [0.0f32, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [2.0, 0.0, 0.0],
        [3.0, 0.0, 0.0],
        [2.0, 1.0, 0.0],
    ]
    .iter()
    .enumerate()
    {
        for axis in 0..3 {
            put(
                &mut payload,
                0x160 + 12 * vertex + 4 * axis,
                point[axis].to_bits(),
            );
        }
    }
    for (triangle, indices) in [[0u16, 1, 2], [3, 4, 5]].iter().enumerate() {
        for axis in 0..3 {
            let offset = 0x1b0 + 10 * triangle + 2 * axis;
            payload[offset..offset + 2].copy_from_slice(&indices[axis].to_le_bytes());
        }
    }
    let source = fixture(&params(), &payload, NAME, false);
    let request = sab_tools::collision_stage::plan_subpack(
        &source,
        1281,
        "synthetic",
        pandemic_hash(NAME),
        0,
        [0.0, 10.0, 0.0],
    )
    .unwrap();
    assert!(request["selection_evidence"].get("components").is_none());
    for faces in [serde_json::json!([1]), serde_json::json!([0, 1])] {
        let mut invalid = request.clone();
        invalid["triangle_ids"] = faces;
        assert!(
            sab_tools::collision_stage::dry_run_subpack(&source, &invalid)
                .unwrap_err()
                .contains("declared complete component")
        );
    }
}

#[test]
fn diagnostic_output_refuses_game_directory_and_existing_destinations() {
    let root = std::env::temp_dir().join(format!(
        "sab-collision-stage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let game = root.join("game");
    std::fs::create_dir(&game).unwrap();
    let inside_error = sab_tools::collision_stage::stage_dry_run(
        &game,
        &serde_json::json!({}),
        &game.join("new-output"),
    )
    .unwrap_err();
    assert!(inside_error.contains("outside the game directory"));
    assert!(!game.join("new-output").exists());
    let existing = root.join("existing");
    std::fs::create_dir(&existing).unwrap();
    let existing_error =
        sab_tools::collision_stage::stage_dry_run(&game, &serde_json::json!({}), &existing)
            .unwrap_err();
    assert!(existing_error.contains("must not exist"));
    std::fs::remove_dir(&existing).unwrap();
    std::fs::remove_dir(&game).unwrap();
    std::fs::remove_dir(&root).unwrap();
}

fn put(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn baked() -> Vec<u8> {
    let hash = pandemic_hash(NAME);
    let mut data = vec![0; 0x19a];
    for (offset, value) in [
        (0, hash),
        (4, 1),
        (0x10, 5),
        (0x14, hash),
        (0xc0, 3),
        (0xc4, hash),
        (0xf0, 5),
        (0xf4, 1),
        (0x100, 3),
        (0x108, 1),
    ] {
        put(&mut data, offset, value);
    }
    for offset in [0x6c, 0x8c, 0x90, 0xa0, 0xb0, 0x16c, 0x17c] {
        put(&mut data, offset, 1.0f32.to_bits());
    }
    data[0x192..0x194].copy_from_slice(&1u16.to_le_bytes());
    data[0x194..0x196].copy_from_slice(&2u16.to_le_bytes());
    data
}

fn params() -> Vec<(u32, Vec<u8>)> {
    let mut fields = vec![(tags::CLASS_NAME, format!("{NAME}\0").into_bytes())];
    for (tag, point) in [
        (tags::POSITION, [30.0f32, 0.0, 330.0]),
        (tags::X_AXIS, [1.0, 0.0, 0.0]),
        (tags::Y_AXIS, [0.0, 1.0, 0.0]),
        (tags::Z_AXIS, [0.0, 0.0, 1.0]),
    ] {
        fields.push((
            tag,
            point.iter().flat_map(|value| value.to_le_bytes()).collect(),
        ));
    }
    fields
}

fn fixture(fields: &[(u32, Vec<u8>)], payload: &[u8], name: &str, duplicate: bool) -> Vec<u8> {
    let hash = pandemic_hash(NAME);
    let mut model = vec![0; 276];
    model[..4].copy_from_slice(b"AHSM");
    model[20..20 + name.len()].copy_from_slice(name.as_bytes());
    let mut list = Vec::new();
    for word in [0u32, 1, 0x12345678, fields.len() as u32] {
        list.extend_from_slice(&word.to_le_bytes());
    }
    for (tag, data) in fields {
        list.extend_from_slice(&tag.to_le_bytes());
        list.extend_from_slice(&(data.len() as u32).to_le_bytes());
        list.extend_from_slice(data);
    }
    let compressed = sab_formats::zlib::deflate(payload, 9);
    let mut blobs = vec![
        sbla::Blob {
            kind: 0,
            hash,
            uncomp: model.len() as u32,
            f10: 0,
            f14: 0,
            data: &model,
            empty_offset: 0,
        },
        sbla::Blob {
            kind: 2,
            hash,
            uncomp: payload.len() as u32,
            f10: 0,
            f14: 0,
            data: &compressed,
            empty_offset: 0,
        },
    ];
    if duplicate {
        blobs.push(blobs[1]);
    }
    blobs.push(sbla::Blob {
        kind: 8,
        hash: 99,
        uncomp: list.len() as u32,
        f10: 0,
        f14: 0xabcdef01,
        data: &list,
        empty_offset: 0,
    });
    let mut counts = [0; 9];
    counts[0] = 1;
    counts[2] = if duplicate { 2 } else { 1 };
    counts[8] = 1;
    let descriptor = sbla::Descriptor {
        heightfield: None,
        tail: map6::Tail {
            list1: vec![],
            list2: vec![],
            x: 0,
            counts,
            a: vec![],
            nb: 0,
            b: vec![],
            c: vec![],
        },
    };
    sbla::write(Some(&descriptor), &blobs).unwrap()
}

#[test]
fn world_overlay_preserves_exact_identity_layer_and_source_bytes() {
    let source = fixture(&params(), &baked(), NAME, false);
    let original = source.clone();
    let inspected = inspect_subpack(&source, 1281, "synthetic.megapack").unwrap();
    assert_eq!(inspected.surfaces.len(), 1, "{:?}", inspected.issues);
    let surface = &inspected.surfaces[0];
    assert_eq!(
        surface.vertices,
        [[30.0, 0.0, 330.0], [31.0, 0.0, 330.0], [30.0, 1.0, 330.0]]
    );
    assert_eq!(
        (surface.list, surface.tag, surface.layer),
        (0, 0x12345678, 0xabcdef01)
    );
    assert_eq!(surface.local_topology.boundary_edges, 3);
    assert_eq!(inspected.subpack_sha256, sab_tools::stage::sha256(&source));
    assert_eq!(source, original);
    let summary = report(&inspected, false);
    assert_eq!(summary["runtime_verified"], false);
    assert!(summary["surfaces"][0].get("world_vertices").is_none());
    assert_eq!(
        report(&inspected, true)["surfaces"][0]["world_vertices"][1][0],
        31.0
    );
}

#[test]
fn explicit_rotated_scaled_basis_is_applied_without_normalizing() {
    let mut fields = params();
    fields
        .iter_mut()
        .find(|(tag, _)| *tag == tags::X_AXIS)
        .unwrap()
        .1 = [0.0f32, 0.0, -2.0]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    fields
        .iter_mut()
        .find(|(tag, _)| *tag == tags::Z_AXIS)
        .unwrap()
        .1 = [1.0f32, 0.0, 0.0]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let source = fixture(&fields, &baked(), NAME, false);
    let inspected = inspect_subpack(&source, 1281, "synthetic").unwrap();
    assert_eq!(inspected.surfaces[0].vertices[1], [30.0, 0.0, 328.0]);
}

#[test]
fn missing_duplicate_malformed_nonfinite_and_singular_transforms_fail_closed() {
    let mut cases = Vec::new();
    for tag in [tags::POSITION, tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS] {
        let mut fields = params();
        fields.retain(|(field, _)| *field != tag);
        cases.push(fields);
        let mut fields = params();
        fields.push(
            fields
                .iter()
                .find(|(field, _)| *field == tag)
                .unwrap()
                .clone(),
        );
        cases.push(fields);
    }
    let mut fields = params();
    fields.retain(|(tag, _)| *tag == tags::POSITION || *tag == tags::CLASS_NAME);
    cases.push(fields);
    for replacement in [
        vec![0; 8],
        vec![0; 12],
        [f32::NAN, 0.0, 0.0]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect(),
    ] {
        let mut fields = params();
        fields
            .iter_mut()
            .find(|(tag, _)| *tag == tags::X_AXIS)
            .unwrap()
            .1 = replacement;
        cases.push(fields);
    }
    for fields in cases {
        let inspected =
            inspect_subpack(&fixture(&fields, &baked(), NAME, false), 1281, "synthetic").unwrap();
        assert!(inspected.surfaces.is_empty());
        assert_eq!(inspected.unsupported_records, 1);
    }
}

#[test]
fn duplicate_resource_hashes_and_unmatched_names_are_not_guessed() {
    for (name, duplicate) in [
        (NAME, true),
        ("France_Streamblock_1281_baked_collision_other", false),
    ] {
        let inspected = inspect_subpack(
            &fixture(&params(), &baked(), name, duplicate),
            1281,
            "synthetic",
        )
        .unwrap();
        assert!(inspected.surfaces.is_empty());
        assert!(inspected.unsupported_records > 0);
    }
    let mut fields = params();
    fields[0].1 = b"OtherClass\0".to_vec();
    assert!(
        inspect_subpack(&fixture(&fields, &baked(), NAME, false), 1281, "synthetic")
            .unwrap()
            .surfaces
            .is_empty()
    );
}

#[test]
fn opaque_edge_items_are_not_shown_even_under_a_baked_class_name() {
    let mut edge = vec![0; 96];
    put(&mut edge, 0, pandemic_hash(NAME));
    put(&mut edge, 12, 1);
    let inspected =
        inspect_subpack(&fixture(&params(), &edge, NAME, false), 1281, "synthetic").unwrap();
    assert!(inspected.surfaces.is_empty());
    assert_eq!(
        (inspected.edge_records, inspected.unsupported_records),
        (1, 1)
    );
}

#[test]
fn unsupported_internal_frame_and_payload_hash_mismatch_are_explicit_gaps() {
    for (offset, value) in [(0x50, 1.0f32.to_bits()), (0, 1)] {
        let mut payload = baked();
        put(&mut payload, offset, value);
        let inspected = inspect_subpack(
            &fixture(&params(), &payload, NAME, false),
            1281,
            "synthetic",
        )
        .unwrap();
        assert!(inspected.surfaces.is_empty());
        assert_eq!(inspected.unsupported_records, 1);
    }
}

#[test]
fn inspection_uses_effective_patch_bytes_and_leaves_sources_unchanged() {
    let root = std::env::temp_dir().join(format!(
        "sab_collision_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("France")).unwrap();
    let base = fixture(&params(), &baked(), NAME, false);
    let mut fields = params();
    fields
        .iter_mut()
        .find(|(tag, _)| *tag == tags::POSITION)
        .unwrap()
        .1 = [100.0f32, 0.0, 330.0]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let patched = fixture(&fields, &baked(), NAME, false);
    let pack = |data: &[u8], index| {
        sab_formats::megapack::write(&[sab_formats::megapack::Item {
            crc: sab_tools::cells::cell_crc(1281),
            index,
            data,
        }])
    };
    let base_file = root.join("France/Mega0.megapack");
    let patch_file = root.join("France/patchmega0.megapack");
    let base_bytes = pack(&base, 1281);
    let patch_bytes = pack(&patched, 0);
    std::fs::write(&base_file, &base_bytes).unwrap();
    std::fs::write(&patch_file, &patch_bytes).unwrap();
    let inspected = sab_tools::collision::inspect_cell(&root, 1281).unwrap();
    assert_eq!(inspected.source_pack, r"France\patchmega0.megapack");
    assert_eq!(inspected.subpack_sha256, sab_tools::stage::sha256(&patched));
    assert_eq!(inspected.surfaces[0].vertices[0], [100.0, 0.0, 330.0]);
    assert_eq!(std::fs::read(&base_file).unwrap(), base_bytes);
    assert_eq!(std::fs::read(&patch_file).unwrap(), patch_bytes);
    std::fs::remove_dir_all(root).unwrap();
}
