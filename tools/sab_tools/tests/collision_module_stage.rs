use sab_formats::editnodes::tags;
use sab_formats::{collision, map6, pandemic_hash, sbla};
use sab_tools::collision_module_stage::{build_subpack, SCHEMA};
use serde_json::json;

const NAME: &str = "France_Streamblock_1281_baked_collision0";

fn fixture() -> (Vec<u8>, serde_json::Value) {
    let hash = pandemic_hash(NAME);
    let mut payload = vec![0u8; 0x1a4];
    for (offset, value) in [
        (0, hash),
        (4, 1),
        (0x10, 5),
        (0x14, hash),
        (0xc0, 3),
        (0xc4, hash),
        (0xf0, 5),
        (0xf4, 1),
        (0x100, 4),
        (0x108, 2),
    ] {
        payload[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    for offset in [0x6c, 0x8c, 0x90, 0xa0, 0xb0] {
        payload[offset..offset + 4].copy_from_slice(&1f32.to_le_bytes());
    }
    for (i, point) in [[0f32, 0., 0.], [1., 0., 0.], [0., 1., 0.], [1., 1., 0.]]
        .iter()
        .enumerate()
    {
        for axis in 0..3 {
            let off = 0x160 + 12 * i + 4 * axis;
            payload[off..off + 4].copy_from_slice(&point[axis].to_le_bytes());
        }
    }
    for (i, tri) in [[0u16, 1, 2, 7, 9], [1, 3, 2, 11, 13]].iter().enumerate() {
        for axis in 0..5 {
            let off = 0x190 + 10 * i + 2 * axis;
            payload[off..off + 2].copy_from_slice(&tri[axis].to_le_bytes());
        }
    }
    payload.extend_from_slice(b"OPAQUE-TRAILER");
    let mut model = vec![0; 276];
    model[..4].copy_from_slice(b"AHSM");
    model[20..20 + NAME.len()].copy_from_slice(NAME.as_bytes());
    let mut list = Vec::new();
    for word in [0u32, 2] {
        list.extend(word.to_le_bytes());
    }
    for (tag, class, pos) in [
        (0x12345678u32, NAME, [30f32, 0., 330.]),
        (0x87654321, "test_visual", [30., 0., 330.]),
    ] {
        let mut fields = vec![(tags::CLASS_NAME, format!("{class}\0").into_bytes())];
        for (key, point) in [
            (tags::POSITION, pos),
            (tags::X_AXIS, [1., 0., 0.]),
            (tags::Y_AXIS, [0., 1., 0.]),
            (tags::Z_AXIS, [0., 0., 1.]),
        ] {
            fields.push((key, point.into_iter().flat_map(f32::to_le_bytes).collect()));
        }
        list.extend(tag.to_le_bytes());
        list.extend((fields.len() as u32).to_le_bytes());
        for (tag, data) in fields {
            list.extend(tag.to_le_bytes());
            list.extend((data.len() as u32).to_le_bytes());
            list.extend(data);
        }
    }
    let compressed = sab_formats::zlib::deflate(&payload, 9);
    let blobs = [
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
        sbla::Blob {
            kind: 8,
            hash: 99,
            uncomp: list.len() as u32,
            f10: 0,
            f14: 0,
            data: &list,
            empty_offset: 0,
        },
        sbla::Blob {
            kind: 4,
            hash: 100,
            uncomp: list.len() as u32,
            f10: 0,
            f14: 0,
            data: &list,
            empty_offset: 0,
        },
    ];
    let mut counts = [0; 9];
    counts[0] = 1;
    counts[2] = 1;
    counts[8] = 1;
    counts[4] = 1;
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
    let source = sbla::write(Some(&descriptor), &blobs).unwrap();
    let request = json!({"schema":SCHEMA,"diagnostic_cut":true,"rotation_y_deg":0,"cell":1281,"source_pack":"synthetic","source_subpack_sha256":sab_tools::stage::sha256(&source),"record":1,"hash":format!("{hash:08x}"),"list":0,"tag":"12345678","layer":"00000000","class_name":NAME,"triangle_ids":[0],"delta_world":[4,0,-2],"target_visual":{"cell":1281,"list":0,"tag":"87654321","layer":"00000000","class_name":"test_visual","expected_position":[30,0,330]}});
    (source, request)
}

#[test]
fn combined_cut_preserves_shared_neighbor_and_mirrors_visual_position() {
    let (source, request) = fixture();
    let copy = source.clone();
    let result = build_subpack(&source, &request).unwrap();
    assert_eq!(source, copy);
    assert_eq!(result.manifest["vertex_count_before"], 4);
    assert_eq!(result.manifest["vertex_count_after"], 7);
    assert_eq!(
        result.manifest["visual_position_after"],
        json!([34., 0., 328.])
    );
    assert_eq!(result.manifest["changed_visual_records"], json!([2, 3]));
    let before = sbla::parse(&source, None).unwrap();
    let after = sbla::parse(&result.subpack, None).unwrap();
    assert_eq!(after.blobs[2], after.blobs[3]);
    assert_ne!(before.blobs[2], after.blobs[2]);
    assert_eq!(before.blobs[0], after.blobs[0]);
    let original =
        collision::read_record(before.blobs[1], before.records[1].uncomp as usize).unwrap();
    let collision::Payload::Baked(a) = collision::decode(&original).unwrap() else {
        panic!()
    };
    let collision::Payload::Baked(b) = collision::decode(&result.collision_payload).unwrap() else {
        panic!()
    };
    assert_eq!(a.vertices, b.vertices[..4]);
    assert_eq!(a.triangles[1], b.triangles[1]);
    assert_eq!(b.triangles[0].indices, [4, 5, 6]);
    assert_eq!(b.vertices[4], [4., 0., -2.]);
    assert_eq!(a.trailer, b.trailer);
    assert_eq!(a.triangles[0].material, b.triangles[0].material);
    assert_eq!(result.manifest["checks"]["maximum_world_error_m"], 0.);
}

#[test]
fn combined_requests_fail_closed_before_output() {
    let (source, request) = fixture();
    for (key, value) in [
        ("source_subpack_sha256", json!("stale")),
        ("triangle_ids", json!([0, 0])),
        ("rotation_y_deg", json!(90)),
        ("diagnostic_cut", json!(false)),
        ("record", json!(99)),
    ] {
        let mut bad = request.clone();
        bad[key] = value;
        assert!(build_subpack(&source, &bad).is_err(), "{key}");
    }
    for (key, value) in [
        ("cell", json!(5)),
        ("tag", json!("00000000")),
        ("class_name", json!("wrong")),
        ("expected_position", json!([30, 1, 330])),
        ("layer", json!("ffffffff")),
    ] {
        let mut bad = request.clone();
        bad["target_visual"][key] = value;
        assert!(build_subpack(&source, &bad).is_err(), "visual {key}");
    }
}

#[test]
fn repaired_pack_changes_only_acceleration_and_size_with_pinned_receipt() {
    use sab_tools::collision_module_stage::package_replacement;
    let (source, request) = fixture();
    let diagnostic = build_subpack(&source, &request).unwrap();
    let mut replacement = diagnostic.collision_payload.clone();
    replacement[0x10c..0x110].copy_from_slice(&0x1234u32.to_le_bytes());
    replacement.extend_from_slice(b"SYNTHETIC-REPLACEMENT-ACCELERATION");
    let receipt = json!({"verification":{"passed":true},"input_sha256":sab_tools::stage::sha256(&diagnostic.collision_payload),"output_sha256":sab_tools::stage::sha256(&replacement)});
    let repair = json!({"schema":"sab_collision_module_repair_request_v1","translation_request":request,"diagnostic_payload_sha256":sab_tools::stage::sha256(&diagnostic.collision_payload),"replacement_payload_sha256":sab_tools::stage::sha256(&replacement),"compiler_receipt_sha256":sab_tools::stage::sha256(&serde_json::to_vec(&receipt).unwrap()),"compiler_receipt":receipt,"allowed_header_byte_offsets":[0x10c,0x10d,0x10e,0x10f]});
    let result = package_replacement(&diagnostic, &replacement, &repair, &receipt).unwrap();
    let original = sbla::parse(&diagnostic.subpack, None).unwrap();
    let rebuilt = sbla::parse(&result.subpack, None).unwrap();
    for i in [0, 2, 3] {
        assert_eq!(original.blobs[i], rebuilt.blobs[i]);
    }
    assert_eq!(
        collision::read_record(rebuilt.blobs[1], rebuilt.records[1].uncomp as usize).unwrap(),
        replacement
    );
    assert_eq!(result.manifest["runtime_verified"], false);
    assert_eq!(result.manifest["installable"], false);
    assert_eq!(
        result.manifest["geometry_checks"]["all_vertices_bit_identical"],
        true
    );

    let mut changed = replacement.clone();
    changed[0x160] ^= 1;
    let mut changed_receipt = receipt.clone();
    changed_receipt["output_sha256"] = json!(sab_tools::stage::sha256(&changed));
    let mut changed_request = repair.clone();
    changed_request["replacement_payload_sha256"] = changed_receipt["output_sha256"].clone();
    changed_request["compiler_receipt"] = changed_receipt.clone();
    changed_request["compiler_receipt_sha256"] = json!(sab_tools::stage::sha256(
        &serde_json::to_vec(&changed_receipt).unwrap()
    ));
    assert!(
        package_replacement(&diagnostic, &changed, &changed_request, &changed_receipt)
            .unwrap_err()
            .contains("collision vertices")
    );
    let mut unbounded = repair.clone();
    unbounded["allowed_header_byte_offsets"] = json!([0x20]);
    assert!(
        package_replacement(&diagnostic, &replacement, &unbounded, &receipt)
            .unwrap_err()
            .contains("bounded header")
    );
    let mut unpinned = repair.clone();
    unpinned["compiler_receipt"] = json!({"stale":true});
    assert!(
        package_replacement(&diagnostic, &replacement, &unpinned, &receipt)
            .unwrap_err()
            .contains("receipt hash mismatch")
    );
}

#[cfg(feature = "retail")]
#[test]
fn retail_v4_module_translation_builds_combined_diagnostic() {
    let game = sab_formats::game_paths::local_config_retail_dir(std::path::Path::new(env!(
        "CARGO_MANIFEST_DIR"
    )))
    .unwrap();
    let mounts = sab_tools::cells::france_mounts(&game).unwrap();
    let cell = 66974465;
    let (mount, entry) =
        sab_tools::cells::resolve_entry(&mounts, sab_tools::cells::cell_crc(cell)).unwrap();
    let source = sab_formats::megapack::entry_slice(&mount.map, entry).unwrap();
    let request = json!({"schema":SCHEMA,"diagnostic_cut":true,"rotation_y_deg":0,"cell":cell,"source_pack":mount.rel,"source_subpack_sha256":"57CCE6ABE2200A473DAE98088973A78433CB3614C9B34A3C6074B8EEB2622C58","record":495,"hash":"99b3dfc9","list":11,"tag":"99b3dfc9","layer":"00000000","class_name":"France_Streamblock_66974465_baked_collision0","triangle_ids":[5980,5981,6579,6580,6581,6582,6583,6584],"delta_world":[2.070552110671997,0,-7.727407932281494],"target_visual":{"cell":cell,"list":11,"tag":"a0557d90","layer":"00000000","class_name":"city1_baseb_x8y4b","expected_position":[-78.7562026977539,71.1141586303711,-729.2568969726562]}});
    let result = build_subpack(source, &request).unwrap();
    assert_eq!(result.manifest["vertex_count_before"], 7613);
    assert_eq!(result.manifest["vertex_count_after"], 7623);
    assert_eq!(result.collision_payload.len(), 296000);
    assert_eq!(result.manifest["trailer"]["bytes"], 98470);
    assert!(
        result.manifest["checks"]["maximum_world_error_m"]
            .as_f64()
            .unwrap()
            < 0.0001
    );
}
