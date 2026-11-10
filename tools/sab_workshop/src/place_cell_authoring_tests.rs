#![cfg(feature = "retail")]

use super::*;
use sab_formats::{cellplace, megapack, mesh, pandemic_hash, sbla};
use sab_tools::cells;
use sab_tools::stage::sha256;

/// Edit size; defaults match the accepted M4 package (+2 m, 15 deg). Override with M5_MOVE_Y / M5_YAW_DEG for a big, obvious in-game test.
fn edit_y() -> f32 { std::env::var("M5_MOVE_Y").ok().and_then(|v| v.parse().ok()).unwrap_or(2.0) }
fn edit_yaw() -> f64 { std::env::var("M5_YAW_DEG").ok().and_then(|v| v.parse().ok()).unwrap_or(15.0) }

fn basis(object: &Node) -> [[f32; 3]; 3] {
    [tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS].map(|tag| object.vec3_param(tag).expect("building basis"))
}


fn verify_staged_cell(
    original: &[u8],
    staged: &[u8],
    target: &crate::cell_props::CellTarget,
    object_index: usize,
    authored: &Node,
    expected_position: [f32; 3],
    original_basis: [[f32; 3]; 3],
) {
    let original_pack = sbla::parse(original, None).expect("independent original ALBS decode");
    let staged_pack = sbla::parse(staged, None).expect("independent staged ALBS decode");
    assert_eq!(staged.len(), original.len(), "same-size transform edit");
    assert_eq!(staged_pack.descriptor, original_pack.descriptor);
    assert_eq!(staged_pack.counts, original_pack.counts);
    assert_eq!(staged_pack.records, original_pack.records, "all ALBS record metadata unchanged");
    let original_layout = cellplace::read(original).unwrap().unwrap();
    let staged_layout = cellplace::read(staged).unwrap().unwrap();
    assert_eq!(staged_layout.lists.len(), original_layout.lists.len());
    let original_list = &original_layout.lists[target.list];
    let original_nodes = editnodes::parse_entry(&original[original_list.loc.off..original_list.loc.off + original_list.loc.comp]).unwrap();
    assert_eq!(original_nodes[object_index].tag(), target.tag);
    assert_eq!(original_nodes[object_index].position(), Some(target.pos));
    assert_eq!(basis(&original_nodes[object_index]), original_basis);
    let mut allowed = vec![false; original.len()];
    for param_tag in [tags::POSITION, tags::X_AXIS, tags::Y_AXIS, tags::Z_AXIS] {
        let matching: Vec<_> = original_list.objects[object_index].params.iter().filter(|param| param.tag == param_tag).collect();
        assert_eq!(matching.len(), 1, "one unambiguous transform leaf");
        let param = matching[0];
        assert_eq!(param.len, 12);
        let start = original_list.loc.off + param.data_off;
        allowed[start..start + 12].fill(true);
        for prefix in &original_layout.prefixes {
            if prefix.mirrors == Some(target.list) && param.data_off + 12 <= prefix.loc.comp {
                let start = prefix.loc.off + param.data_off;
                allowed[start..start + 12].fill(true);
                assert_eq!(&staged[start..start + 12], &staged[original_list.loc.off + param.data_off..original_list.loc.off + param.data_off + 12], "type-4 mirror matches edited type-8 leaf");
            }
        }
    }
    let radians = edit_yaw().to_radians();
    let expected_basis = original_basis.map(|axis| [
        (radians.cos() * f64::from(axis[0]) + radians.sin() * f64::from(axis[2])) as f32,
        axis[1],
        (-radians.sin() * f64::from(axis[0]) + radians.cos() * f64::from(axis[2])) as f32,
    ]);
    for (list_index, list) in staged_layout.lists.iter().enumerate() {
        let source = &original_layout.lists[list_index];
        assert_eq!(list.loc, source.loc);
        assert_eq!((list.f10, list.f14), (source.f10, source.f14));
        let before = editnodes::parse_entry(&original[source.loc.off..source.loc.off + source.loc.comp]).unwrap();
        let after = editnodes::parse_entry(&staged[list.loc.off..list.loc.off + list.loc.comp]).expect("independent type-8 EditNode decode");
        assert_eq!(after.len(), before.len(), "object order and count unchanged");
        for (index, (old, new)) in before.iter().zip(&after).enumerate() {
            if list_index == target.list && index == object_index {
                assert_eq!(new, authored, "decoded target exactly matches the reopened page transform");
                assert_eq!(new.tag(), target.tag);
                assert_eq!(new.class_name().as_deref(), Some(target.class_name.as_str()));
                assert_eq!(new.position(), Some(expected_position));
                for (actual_axis, expected_axis) in basis(new).iter().zip(expected_basis) {
                    for (actual, expected) in actual_axis.iter().zip(expected_axis) {
                        assert!((*actual - expected).abs() <= 1e-5, "independently computed yaw: actual {actual}, expected {expected}");
                    }
                }
            } else {
                assert_eq!(new, old, "unrelated object {index} in list {list_index} byte-identical");
            }
        }
    }
    let mut changed_bytes = 0;
    for (offset, (&before, &after)) in original.iter().zip(staged).enumerate() {
        if before != after {
            assert!(allowed[offset], "unrelated sub-pack byte changed at {offset:#x}");
            changed_bytes += 1;
        }
    }
    assert!(changed_bytes > 0);
    for (index, (before, after)) in original_pack.blobs.iter().zip(&staged_pack.blobs).enumerate() {
        let start = before.as_ptr() as usize - original.as_ptr() as usize;
        if !allowed[start..start + before.len()].iter().any(|allowed| *allowed) {
            assert_eq!(before, after, "unrelated ALBS blob {index} unchanged, including type-0 meshes and type-2 collision");
        }
    }
}



fn successful(page: &Places) {
    let (message, failed) = page.status.as_ref().expect("page action reports a result");
    assert!(!failed, "{message}");
}

fn current_object(page: &Places, selected: ObjRef) -> &Node {
    &page.session.as_ref().unwrap().entry(selected.0).unwrap().objects()[selected.1]
}

fn preserved_state(page: &Places) -> serde_json::Value {
    let session = page.session.as_ref().unwrap();
    let objects: Vec<_> = session.containers().iter().map(|container| {
        (container.hash, sha256(&sab_tools::cellsession::list_body(session.entry(container.hash).unwrap().objects())))
    }).collect();
    let edit = page.cell_edit.as_ref().unwrap();
    serde_json::json!({
        "session": session.to_project_json(), "objects": objects,
        "cell": edit.cell, "source": edit.source_rel, "source_sha256": edit.source_sha,
        "selected": page.selected(), "project": page.project, "staged_dir": page.staged_dir,
        "report": page.report, "saved_rev": page.saved_rev,
        "game_dir": page.game_dir, "cell_input": page.cell_input,
    })
}

fn acceptance_directory() -> PathBuf {
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap().join("output");
    std::fs::create_dir_all(&output).unwrap();
    let root = output.join("m4_outdoor_building_20261105");
    for attempt in 0..10_000 {
        let candidate = if attempt == 0 { root.clone() } else { root.join(format!("acceptance_{attempt:04}")) };
        match std::fs::create_dir(&candidate) {
            Ok(()) => return candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("{}: {error}", candidate.display()),
        }
    }
    panic!("no unused acceptance directory under {}", root.display());
}

fn building_target(mounts: &[cells::Mount]) -> (crate::cell_props::CellTarget, Vec<u8>, serde_json::Value, Option<crate::cell_props::CellTarget>) {
    let (placements, _) = cells::scan_cells(mounts).expect("scan effective retail cells");
    let mut candidates: Vec<_> = placements.iter().filter(|placement| {
        let class = placement.class_name.to_ascii_lowercase();
        placement.cell != 132_609 && placement.list_f14 == 0 && placement.axes.is_some()
            && class.starts_with("city1_") && !class.contains("_hide")
            && (class.contains("_base") || class.contains("shop"))
    }).collect();
    // Optional override for in-game tests: M5_TARGET_TAG=<8 hex digits> picks one specific qualifying building.
    if let Ok(want) = std::env::var("M5_TARGET_TAG") {
        let want = want.trim().to_ascii_lowercase();
        candidates.retain(|placement| format!("{:08x}", placement.tag) == want);
    }
    candidates.sort_by_key(|placement| {
        let class = placement.class_name.to_ascii_lowercase();
        let preference = if class.starts_with("city1_basea") { 0 } else if class.contains("shop") { 1 } else { 2 };
        (preference, placement.cell, placement.list, placement.tag)
    });
    for placement in candidates {
        let crc = cells::cell_crc(placement.cell);
        let owners: Vec<_> = mounts.iter().filter(|mount| mount.find(crc).is_some()).collect();
        if owners.len() != 1 || !matches!(owners[0].rel.as_str(),
            r"France\Mega.megapack" | r"France\Mega0.megapack" | r"France\Mega1.megapack" |
            r"France\Mega2.megapack" | r"France\Mega3.megapack") {
            continue;
        }
        let owner = owners[0];
        let entry = owner.find(crc).unwrap();
        let sub = megapack::entry_slice(&owner.map, entry).unwrap();
        let pack = sbla::parse(sub, None).unwrap();
        let matching_meshes: Vec<_> = pack.records.iter().zip(&pack.blobs).enumerate()
            .filter(|(_, (record, _))| record.kind == 0 && record.hash == pandemic_hash(&placement.class_name)).collect();
        if matching_meshes.len() != 1 {
            continue;
        }
        let (record_index, (record, blob)) = matching_meshes[0];
        let header = mesh::parse_msha(blob).expect("same-cell type-0 MSHA");
        assert_eq!(pandemic_hash(&header.name), record.hash);
        assert!(header.name.eq_ignore_ascii_case(&placement.class_name));
        if header.c0 == 0 || header.unc0 == 0 || header.c1 == 0 || header.unc1 == 0 {
            continue;
        }
        let (body, dat) = mesh::read_body_and_dat(blob, &header).expect("inflate same-cell MSHA body and geometry data");
        assert!(!body.is_empty() && !dat.is_empty());
        let layout = cellplace::read(sub).unwrap().unwrap();
        let list = &layout.lists[placement.list];
        assert_eq!(list.f14, 0);
        assert_eq!(list.objects.iter().filter(|object| object.tag == placement.tag).count(), 1);
        let target = crate::cell_props::CellTarget {
            cell: placement.cell, list: placement.list, tag: placement.tag,
            class_name: placement.class_name.clone(), pos: placement.pos, layer: placement.list_f14,
        };
        let proof = serde_json::json!({
            "source": owner.rel, "entry_crc": format!("{:08x}", entry.crc), "entry_index": entry.index,
            "same_cell": placement.cell, "record_kind": record.kind, "record_index": record_index,
            "record_hash": format!("{:08x}", record.hash), "name": header.name,
            "blob_sha256": sha256(blob), "body_sha256": sha256(&body), "dat_sha256": sha256(&dat),
            "body_bytes": body.len(), "dat_bytes": dat.len(), "source_cell_sha256": sha256(sub),
        });
        let dlc_target = placements.iter().find(|p| p.cell == 132_609 && p.axes.is_some() && !p.class_name.is_empty()).map(|p| crate::cell_props::CellTarget {
            cell: p.cell, list: p.list, tag: p.tag, class_name: p.class_name.clone(), pos: p.pos, layer: p.list_f14,
        });
        return (target, sub.to_vec(), proof, dlc_target);
    }
    panic!("no uniquely base-owned, base-layer city1_ base/shop building with a same-cell type-0 MSHA; DLC cell 132609 is deliberately excluded");
}

#[cfg(feature = "retail")]
#[test]
#[ignore = "long retail offline authoring acceptance; needs .sab-local.toml, scans France cells, preserves READY-TO-TEST artifacts"]
fn outdoor_building_authoring_milestone() {
    let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    let game_string = game.to_string_lossy().into_owned();
    let mounts = cells::france_mounts(&game).unwrap();
    let (target, original_sub, mesh_proof, dlc_target) = building_target(&mounts);
    let crc = cells::cell_crc(target.cell);
    let (owner, original_entry) = cells::resolve_entry(&mounts, crc).unwrap();
    assert_ne!(target.cell, 132_609);
    assert_eq!(target.layer, 0);
    let original_entry = *original_entry;
    let source_rel = owner.rel.clone();
    let (dlc_owner, _) = cells::resolve_entry(&mounts, cells::cell_crc(132_609)).expect("known DLC refusal fixture");
    assert_eq!(dlc_owner.rel, cells::DLC_MEGAPACK);
    drop(mounts);

    let context = egui::Context::default();
    let mut page = Places::new();
    page.cells_enabled = true;
    page.open_cell_target(&game_string, target.clone());
    successful(&page);
    let edit = page.cell_edit.as_ref().expect("target opens a cell editing session");
    assert_eq!(edit.cell, target.cell);
    assert_eq!(edit.source_rel, source_rel);
    let source_sha = edit.source_sha.clone();
    let selected = (edit.containers[target.list], page.sel_object.unwrap());
    assert_eq!(page.selected(), vec![selected]);
    let before = current_object(&page, selected).clone();
    assert_eq!(before.tag(), target.tag);
    assert_eq!(before.class_name().as_deref(), Some(target.class_name.as_str()));
    assert_eq!(before.position(), Some(target.pos));
    let before_basis = basis(&before);
    let after_position = [target.pos[0], target.pos[1] + edit_y(), target.pos[2]];

    page.move_many(&[(selected.0, selected.1, after_position)]);
    successful(&page);
    assert!(page.is_dirty());
    assert_eq!(page.session.as_ref().unwrap().journal().1, 1);
    let moved = current_object(&page, selected).clone();
    assert_eq!(moved.position(), Some(after_position));
    assert_eq!(basis(&moved), before_basis);
    assert!(page.undo().is_some());
    successful(&page);
    assert_eq!(current_object(&page, selected), &before);
    assert_eq!(page.session.as_ref().unwrap().journal().1, 0);
    assert!(page.redo().is_some());
    successful(&page);
    assert_eq!(current_object(&page, selected), &moved);

    let (sine, cosine) = (edit_yaw() as f32).to_radians().sin_cos();
    page.rotate_many(&context, &[selected], [[cosine, 0.0, sine], [0.0, 1.0, 0.0], [-sine, 0.0, cosine]], after_position);
    successful(&page);
    let authored = current_object(&page, selected).clone();
    assert_ne!(basis(&authored), before_basis);
    assert_eq!(authored.position(), Some(after_position));
    let rotated_cursor = page.session.as_ref().unwrap().journal().1;
    assert!(rotated_cursor > 1);
    let groups: std::collections::BTreeSet<_> = page.session.as_ref().unwrap().groups().iter().copied().collect();
    assert_eq!(groups.len(), 2, "exactly one move group and one rotation group");
    assert!(page.undo().is_some());
    successful(&page);
    assert_eq!(page.session.as_ref().unwrap().journal().1, 1, "one undo removes the entire rotation group");
    assert_eq!(current_object(&page, selected), &moved);
    assert!(page.redo().is_some());
    successful(&page);
    assert_eq!(page.session.as_ref().unwrap().journal().1, rotated_cursor);
    assert_eq!(current_object(&page, selected), &authored);

    page.test_discard_answer = Some(false);
    let retained = preserved_state(&page);
    page.open_cell_target(&game_string, target.clone());
    assert_eq!(preserved_state(&page), retained, "cancelled replacement preserves dirty cell session");
    assert!(page.is_dirty());
    page.test_discard_answer = Some(true);
    let mut stale = target.clone();
    stale.pos[1] += 1.0;
    page.open_cell_target(&game_string, stale);
    assert!(page.status.as_ref().unwrap().1);
    assert!(page.status.as_ref().unwrap().0.contains("changed since"));
    assert_eq!(preserved_state(&page), retained, "stale target preserves session");
    let dlc = dlc_target.expect("DLC cell 132609 has at least one exact placement target");
    assert_eq!(dlc.cell, 132_609);
    page.open_cell_target(&game_string, dlc);
    assert!(page.status.as_ref().unwrap().1);
    assert!(page.status.as_ref().unwrap().0.contains("refusing DLC authoring"));
    assert_eq!(preserved_state(&page), retained, "DLC refusal preserves session");

    let directory = acceptance_directory();
    eprintln!("outdoor building acceptance: cell {} list {} tag {:08x} {}; output {}", target.cell, target.list, target.tag, target.class_name, directory.display());
    let project = directory.join("outdoor_building.sabproject.json");
    page.save_project_to(&project);
    successful(&page);
    assert!(!page.is_dirty());
    let journal = page.session.as_ref().unwrap().to_project_json();
    let mut reopened = Places::new();
    reopened.cells_enabled = true;
    reopened.open_project_from(&project);
    successful(&reopened);
    assert_eq!(reopened.project.as_ref(), Some(&project));
    assert_eq!(reopened.selected(), vec![selected], "fresh page focuses the authored building");
    assert_eq!(std::path::Path::new(&reopened.game_dir).canonicalize().unwrap(), game.canonicalize().unwrap(), "fresh page restores game_dir from the terrain-cell project");
    assert!(!reopened.is_dirty());
    assert_eq!(reopened.session.as_ref().unwrap().to_project_json(), journal, "fresh page restores exact journal, groups and undo cursor");
    assert_eq!(current_object(&reopened, selected), &authored, "fresh page restores exact transform bytes");
    for container in page.session.as_ref().unwrap().containers() {
        assert_eq!(reopened.session.as_ref().unwrap().entry(container.hash).unwrap().objects(), page.session.as_ref().unwrap().entry(container.hash).unwrap().objects());
    }
    assert_eq!(reopened.cell_edit.as_ref().unwrap().source_sha, source_sha);
    reopened.stage(&directory.to_string_lossy());
    successful(&reopened);
    let staged = reopened.staged_dir.as_ref().unwrap();
    assert!(staged.starts_with(&directory));
    assert!(reopened.report.as_ref().unwrap().contains("ALL STATIC CHECKS PASS: true"));
    assert!(reopened.report.as_ref().unwrap().contains(sab_tools::cellsession::COLLISION_CAVEAT));
    for name in ["install.ps1", "restore.ps1", "manifest.json", "report.txt"] {
        assert!(staged.join(name).is_file(), "existing staging pipeline supplies {name}");
    }
    let report_on_disk = std::fs::read_to_string(staged.join("report.txt")).unwrap();
    assert!(report_on_disk.contains("ALL STATIC CHECKS PASS: true"), "shipped report.txt must carry the final status, not only the in-memory report");
    assert!(report_on_disk.contains(sab_tools::cellsession::COLLISION_CAVEAT), "shipped report.txt must carry the collision caveat");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(staged.join("manifest.json")).unwrap()).unwrap();
    let artifacts: Vec<_> = std::fs::read_dir(staged).unwrap().map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "megapack")).collect();
    assert_eq!(artifacts.len(), 1);
    let artifact_path = &artifacts[0];
    let artifact = std::fs::read(artifact_path).unwrap();
    let artifact_sha = sha256(&artifact);
    assert_eq!(manifest["files"][0]["installed_sha256"], serde_json::Value::String(artifact_sha.clone()), "manifest records the shipped hash");
    assert_eq!(manifest["provenance"]["source_sha256"], serde_json::Value::String(source_sha.clone()), "manifest records the source hash");
    assert!(manifest["preconditions"].as_array().unwrap().iter().any(|pre|
        pre["file"] == serde_json::Value::String(source_rel.clone()) && pre["sha256"] == serde_json::Value::String(source_sha.clone())
    ), "manifest preconditions include the source archive");
    assert_eq!(sha256(&std::fs::read(game.join(&source_rel)).unwrap()), source_sha, "staging did not modify the retail source archive");
    let install = std::fs::read_to_string(staged.join("install.ps1")).unwrap();
    let restore = std::fs::read_to_string(staged.join("restore.ps1")).unwrap();
    assert!(install.contains("Get-FileHash") && install.contains(&source_rel), "install script checks hashes/source");
    let destination = manifest["files"][0]["dest"].as_str().expect("manifest destination");
    assert!(restore.contains("Get-FileHash") && restore.contains(destination), "restore script checks destination hash/state");
    let outer = megapack::parse(&artifact).expect("independent MP00 decode");
    assert_eq!(outer.entries.len(), 1);
    assert_eq!(outer.entries[0].crc, original_entry.crc);
    assert_eq!(outer.entries[0].index, original_entry.index);
    let staged_sub = megapack::entry_slice(&artifact, &outer.entries[0]).unwrap();
    verify_staged_cell(&original_sub, staged_sub, &target, selected.1, &authored, after_position, before_basis);

    let radians = edit_yaw().to_radians();
    let expected_basis = before_basis.map(|axis| [
        (radians.cos() * f64::from(axis[0]) + radians.sin() * f64::from(axis[2])) as f32,
        axis[1],
        (-radians.sin() * f64::from(axis[0]) + radians.cos() * f64::from(axis[2])) as f32,
    ]);
    let expectations = serde_json::json!({
        "status": "READY-TO-TEST",
        "acceptance_unix_seconds": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        "cell": target.cell, "list": target.list, "tag": format!("{:08x}", target.tag),
        "class": target.class_name, "layer": target.layer, "source": source_rel,
        "source_sha256": source_sha, "source_cell_sha256": sha256(&original_sub),
        "source_inputs": journal["inputs"], "same_cell_type0_msha": mesh_proof,
        "before": { "position": target.pos, "basis_xyz": before_basis },
        "after": { "position": after_position, "basis_xyz": basis(&authored) },
        "independent_expected": { "position": after_position, "basis_xyz": expected_basis, "basis_tolerance": 0.00001 },
        "intent": { "move_y_metres": edit_y(), "world_yaw_degrees": edit_yaw(), "undo_groups": 2 },
        "project_path": project, "project_sha256": sha256(&std::fs::read(&project).unwrap()),
        "artifact_path": artifact_path, "artifact_sha256": artifact_sha,
        "staged_cell_sha256": sha256(staged_sub), "stage_path": staged,
        "runtime_verified": false, "collision_rebuilt": false, "navigation_rebuilt": false,
        "static_checks_pass": true, "all_unrelated_objects_and_blobs_unchanged": true,
        "cancelled_open_preserved_session": true, "stale_target_preserved_session": true,
        "dlc_refusal_preserved_session": true, "collision_caveat": sab_tools::cellsession::COLLISION_CAVEAT,
    });
    std::fs::write(directory.join("expectations.json"), serde_json::to_string_pretty(&expectations).unwrap()).unwrap();
    let checklist = format!(
        "# READY-TO-TEST: outdoor building authoring\n\nOffline acceptance passed. No UI/game launched; nothing installed.\n\n- Cell {}, list {}, tag {:08x}, class {} (base layer 0).\n- Expected move: +2 metres Y only; expected rotation: 15 degrees world yaw about the building origin.\n- Artifact: {}\n- SHA-256: {}\n- Project: {}\n\n## Manual runtime acceptance (NOT performed)\n\n1. Verify expectations.json and artifact hash; retain this exact artifact for A/B.\n2. With the game closed, review and use the generated install.ps1 alongside the artifact; do not copy over retail archives manually.\n3. Visit the recorded before-position in the same base layer, compare baseline and edited screenshots, and verify the raised building and 15-degree yaw.\n4. Check collision at both the old and new building locations. Collision and navigation were NOT rebuilt; record mismatches rather than assuming they follow the visual mesh.\n5. Close the game; use the generated restore.ps1, then verify the baseline appearance/collision is restored.\n6. Record runtime evidence separately; runtime_verified remains false until a real A/B is completed.\n",
        target.cell, target.list, target.tag, target.class_name, artifact_path.display(), artifact_sha, project.display(),
    );
    std::fs::write(directory.join("READY-TO-TEST.md"), checklist).unwrap();
    eprintln!("READY-TO-TEST artifact: {}\nsha256: {}\nexpectations: {}", artifact_path.display(), artifact_sha, directory.join("expectations.json").display());
}
