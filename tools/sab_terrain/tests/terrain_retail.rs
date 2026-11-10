use sab_formats::game_paths;
use sab_terrain::{geometry::TerrainMesh, height::HeightGrid, retail};
use std::{collections::BTreeSet, path::Path};

#[test]
fn retail_terrain_is_structurally_complete_and_independently_correlated() {
    let root = game_paths::retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mut outside = 0;
    let mut shared = 0;
    let mut outliers = 0;
    let mut outlier_locations = BTreeSet::new();
    let mut selected: Vec<TerrainMesh> = Vec::new();
    let mut owners = Vec::new();
    let regions = [
        (0.0, 0.0),
        (0.0, 240.0),
        (720.0, -720.0),
        (420.0, -3000.0),
        (480.0, 0.0),
    ];
    let survey = retail::visit(&root, |source, mesh| {
        owners.push((source.cell, mesh.bounds));
        for stream in &mesh.streams {
            for position in &stream.positions {
                if (0..3).any(|axis| {
                    position[axis] < stream.bounds[0][axis] - 0.002 - stream.quantization
                        || position[axis] > stream.bounds[1][axis] + 0.002 + stream.quantization
                }) {
                    outside += 1;
                }
            }
        }
        let floats: Vec<_> = mesh
            .streams
            .iter()
            .filter(|stream| stream.format & 0x8000_0000 == 0)
            .flat_map(|stream| &stream.positions)
            .collect();
        for packed in mesh
            .streams
            .iter()
            .filter(|stream| stream.format & 0x8000_0000 != 0)
        {
            for position in &packed.positions {
                let matches: Vec<_> = floats
                    .iter()
                    .filter(|other| {
                        (other[0] - position[0]).abs() < 0.0005
                            && (other[2] - position[2]).abs() < 0.0005
                    })
                    .collect();
                if matches.is_empty() {
                    continue;
                }
                shared += 1;
                let delta = matches
                    .iter()
                    .map(|other| (other[1] - position[1]).abs())
                    .fold(f32::INFINITY, f32::min);
                if delta > packed.quantization + 0.0005 {
                    outliers += 1;
                    outlier_locations.insert((
                        mesh.name.clone(),
                        (position[0] * 10000.0).round() as i32,
                        (position[2] * 10000.0).round() as i32,
                    ));
                    assert!(
                        delta < 0.005,
                        "new seam discrepancy {} at {position:?}: {delta}",
                        mesh.name
                    );
                }
            }
        }
        if regions.iter().any(|(min_x, min_z)| {
            mesh.bounds[0][0] <= min_x + 60.0
                && mesh.bounds[1][0] >= *min_x
                && mesh.bounds[0][2] <= min_z + 60.0
                && mesh.bounds[1][2] >= *min_z
        }) {
            selected.push(mesh.clone());
        }
    })
    .unwrap();
    assert!(survey.failures.is_empty(), "{:?}", survey.failures);
    assert_eq!(survey.cells.len(), 7731);
    assert_eq!(survey.dimensions, [((10, 10), 7731)].into());
    for field in survey.cells.values() {
        HeightGrid::new(&field.heightfield, field.tile.a, field.tile.b).unwrap();
    }
    assert_eq!(
        (survey.chunks, survey.vertices, survey.triangles),
        (6032, 1919414, 2254289)
    );
    assert_eq!(outside, 0);
    assert_eq!(shared, 59193);
    assert_eq!(outliers, 2);
    assert_eq!(
        outlier_locations,
        [("France_Chunk279_001".into(), 10125000, -2953125)].into()
    );
    let intersecting = owners
        .iter()
        .filter(|(cell, bounds)| {
            let tile = &survey.cells[cell].tile;
            bounds[0][0] <= tile.b[0]
                && bounds[1][0] >= tile.a[0]
                && bounds[0][2] <= tile.b[2]
                && bounds[1][2] >= tile.a[2]
        })
        .count();
    println!(
        "terrain chunk / owning 60m cell box intersection: {intersecting}/{}",
        owners.len()
    );
    assert_eq!(intersecting, 6032);
    for (cell, expected_matches, expected_max) in [
        (1, 82, 0.412475),
        (1025, 75, 0.908092),
        (1700865, 58, 1.154501),
        (1035777, 100, 1.292762),
    ] {
        let field = &survey.cells[&cell];
        let grid = HeightGrid::new(&field.heightfield, field.tile.a, field.tile.b).unwrap();
        let mut differences = Vec::new();
        let mut missing = 0;
        for row in 0..10 {
            for column in 0..10 {
                let [world_x, height, world_z] = grid.node(column, row).unwrap();
                let delta = selected
                    .iter()
                    .flat_map(|mesh| mesh.heights_at(world_x, world_z))
                    .map(|other| (other - height).abs())
                    .reduce(f64::min);
                if let Some(delta) = delta {
                    differences.push(delta);
                } else {
                    missing += 1;
                }
            }
        }
        differences.sort_by(f64::total_cmp);
        println!("HEI1 cell={cell} mesh_matches={} missing={missing} median_abs_delta={:?} max_abs_delta={:?}", differences.len(), differences.get(differences.len() / 2), differences.last());
        assert_eq!(differences.len(), expected_matches);
        assert!((differences.last().unwrap() - expected_max).abs() < 0.0001);
    }
    let first = &survey.cells[&1];
    let first_grid = HeightGrid::new(&first.heightfield, first.tile.a, first.tile.b).unwrap();
    let heights: Vec<_> = selected
        .iter()
        .flat_map(|mesh| mesh.heights_at(18.75, 46.875))
        .collect();
    println!(
        "sample(18.75,46.875): terrain={heights:?} HEI1={:?}",
        first_grid.bilinear(18.75, 46.875)
    );
    assert!(heights
        .iter()
        .any(|height| (*height - 40.05082321166992).abs() < 1e-6));
    for (world_x, world_z, expected) in [
        (487.5, 37.5, 42.0),
        (-20.0, -20.0, 40.002837),
        (450.0, -2970.0, 70.720717),
    ] {
        assert!(
            selected
                .iter()
                .flat_map(|mesh| mesh.heights_at(world_x, world_z))
                .any(|height| (height - expected).abs() < 1e-5),
            "missing reference sample at ({world_x},{world_z})"
        );
    }
    assert!(
        selected
            .iter()
            .all(|mesh| mesh.heights_at(30.0, 30.0).is_empty()),
        "known urban terrain hole must not be filled"
    );
    assert!(selected
        .iter()
        .all(|mesh| mesh.heights_at(1e9, 1e9).is_empty()));
    let clipped: Vec<_> = selected
        .iter()
        .flat_map(|mesh| mesh.clipped_triangles(0.0, 0.0, 60.0, 60.0).unwrap())
        .collect();
    assert!(!clipped.is_empty());
    assert!(clipped
        .iter()
        .flatten()
        .all(|position| (0.0..=60.0).contains(&position[0])
            && (0.0..=60.0).contains(&position[2])
            && position[1].is_finite()));
    println!("verified chunks={} vertices={} triangles={} shared_seams={shared} known_outlier_occurrences={outliers} clipped_square_triangles={}", survey.chunks, survey.vertices, survey.triangles, clipped.len());
}
