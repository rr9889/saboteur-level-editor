use sab_terrain::{geometry::TerrainMesh, retail::ChunkSource};
use sab_terrain_query::{Builder, Ray, TerrainIndex};

fn source(cell: u32) -> ChunkSource {
    ChunkSource {
        pack: "synthetic".into(),
        cell,
        crc: cell + 10,
        offset: u64::from(cell) * 100,
    }
}

fn mesh(vertices: Vec<[f32; 3]>, triangles: Vec<[usize; 3]>) -> TerrainMesh {
    TerrainMesh {
        name: "synthetic".into(),
        streams: vec![],
        vertices,
        triangles,
        bounds: [[0.0; 3]; 2],
    }
}

fn floor(world_x: f32, height: f32, world_z: f32) -> TerrainMesh {
    let mut result = mesh(
        vec![
            [world_x, height, world_z],
            [world_x + 10.0, height, world_z],
            [world_x, height, world_z + 10.0],
        ],
        vec![[0, 1, 2]],
    );
    result.bounds = [
        [world_x, height, world_z],
        [world_x + 10.0, height, world_z + 10.0],
    ];
    result
}

fn scene(meshes: &[TerrainMesh]) -> TerrainIndex {
    let mut builder = Builder::default();
    for (index, mesh) in meshes.iter().enumerate() {
        builder.add(&source(index as u32), mesh).unwrap();
    }
    builder.finish()
}

#[test]
fn overlapping_surfaces_are_sorted_and_provenance_is_preserved() {
    let index = scene(&[
        floor(-10.0, 8.0, -10.0),
        floor(-10.0, 2.0, -10.0),
        floor(-10.0, 8.0, -10.0),
    ]);
    let query = index.heights_at(-8.0, -8.0).unwrap();
    assert_eq!(
        query.value.iter().map(|hit| hit.height).collect::<Vec<_>>(),
        [2.0, 8.0, 8.0]
    );
    assert_eq!(
        query
            .value
            .iter()
            .map(|hit| hit.key.source)
            .collect::<Vec<_>>(),
        [1, 0, 2]
    );
    let source = index.source(query.value[0].key).unwrap();
    assert_eq!(
        (
            source.location.cell,
            source.location.crc,
            source.location.offset
        ),
        (1, 11, 100)
    );
}

#[test]
fn geometry_not_owner_cell_or_stored_mesh_box_controls_lookup() {
    let mut geometry = floor(450.0, 70.0, -2970.0);
    geometry.bounds = [[0.0; 3]; 2];
    let index = scene(&[geometry]);
    assert_eq!(
        index.heights_at(451.0, -2969.0).unwrap().value[0].height,
        70.0
    );
    assert!(index.heights_at(451.0, 2969.0).unwrap().value.is_empty());
}

#[test]
fn index_prunes_and_matches_linear_height_queries_on_edges_and_near_edges() {
    let meshes: Vec<_> = (0..500)
        .map(|index| floor(index as f32 * 20.0 - 5000.0, index as f32, -20.0))
        .collect();
    let index = scene(&meshes);
    for world_x in [
        -5000.0,
        -4995.0,
        -4990.0,
        -5000.00000005,
        0.0,
        4990.0,
        10000.0,
    ] {
        for world_z in [-20.0, -15.0, -10.0, -20.00000005, 0.0] {
            let actual = index.heights_at(world_x, world_z).unwrap();
            let expected: Vec<_> = meshes
                .iter()
                .enumerate()
                .flat_map(|(source, mesh)| {
                    mesh.heights_at(world_x, world_z)
                        .into_iter()
                        .map(move |height| (height, source as u32))
                })
                .collect();
            assert_eq!(
                actual
                    .value
                    .iter()
                    .map(|hit| (hit.height, hit.key.source))
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(actual.stats.triangles_tested < 30);
        }
    }
}

#[test]
fn ray_directions_are_normalized_and_distance_limits_are_in_metres() {
    let index = scene(&[floor(0.0, 5.0, 0.0), floor(0.0, 2.0, 0.0)]);
    let hit = index
        .raycast(Ray::new([2.0, 10.0, 2.0], [0.0, -500.0, 0.0], 5.0).unwrap())
        .value
        .unwrap();
    assert_eq!(hit.distance, 5.0);
    assert_eq!(hit.position, [2.0, 5.0, 2.0]);
    assert_eq!(hit.key.source, 0);
    assert!(index
        .raycast(Ray::new([2.0, 10.0, 2.0], [0.0, -1.0, 0.0], 4.999).unwrap())
        .value
        .is_none());
}

#[test]
fn raycasting_is_double_sided_and_handles_vertical_geometry() {
    let wall = mesh(
        vec![[0.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
        vec![[0, 1, 2]],
    );
    let index = scene(&[wall]);
    assert!(index.heights_at(0.0, 2.0).unwrap().value.is_empty());
    for (origin, direction) in [
        ([-5.0, 2.0, 2.0], [1.0, 0.0, 0.0]),
        ([5.0, 2.0, 2.0], [-1.0, 0.0, 0.0]),
    ] {
        let hit = index
            .raycast(Ray::new(origin, direction, 5.0).unwrap())
            .value
            .unwrap();
        assert_eq!(hit.distance, 5.0);
        assert_eq!(hit.position, [0.0, 2.0, 2.0]);
        assert_eq!(hit.normal, [1.0, 0.0, 0.0]);
    }
}

#[test]
fn nearest_and_exact_tie_identity_are_deterministic_across_tree_branches() {
    let meshes: Vec<_> = (0..100).map(|_| floor(0.0, 5.0, 0.0)).collect();
    let index = scene(&meshes);
    let ray = Ray::new([2.0, 10.0, 2.0], [0.0, -1.0, 0.0], 20.0).unwrap();
    for _ in 0..10 {
        let hit = index.raycast(ray).value.unwrap();
        assert_eq!((hit.key.source, hit.key.triangle), (0, 0));
    }
}

#[test]
fn zero_distance_ties_ignore_signed_zero_from_opposite_winding() {
    let mut reversed = floor(0.0, 5.0, 0.0);
    reversed.triangles[0] = [0, 2, 1];
    let index = scene(&[reversed, floor(0.0, 5.0, 0.0)]);
    let hit = index
        .raycast(Ray::new([2.0, 5.0, 2.0], [0.0, -1.0, 0.0], 0.0).unwrap())
        .value
        .unwrap();
    assert_eq!(hit.key.source, 0);
    assert!(!hit.distance.is_sign_negative());
}

#[test]
fn degenerate_parallel_coplanar_and_empty_queries_are_explicit_misses() {
    let empty = Builder::default().finish();
    let ray = Ray::new([2.0, 10.0, 2.0], [0.0, -1.0, 0.0], 20.0).unwrap();
    assert!(empty.raycast(ray).value.is_none());
    assert_eq!(
        empty.heights_at(0.0, 0.0).unwrap().stats.triangles_tested,
        0
    );
    let index = scene(&[mesh(vec![[2.0, 5.0, 2.0]; 3], vec![[0, 1, 2]])]);
    assert!(index.raycast(ray).value.is_none());
    assert!(index.heights_at(2.0, 2.0).unwrap().value.is_empty());
    let index = scene(&[floor(0.0, 5.0, 0.0)]);
    for height in [5.0, 6.0] {
        assert!(index
            .raycast(Ray::new([2.0, height, 2.0], [1.0, 0.0, 0.0], 20.0).unwrap())
            .value
            .is_none());
    }
}

#[test]
fn surface_origin_and_triangle_edges_are_inclusive() {
    let index = scene(&[floor(0.0, 5.0, 0.0)]);
    for point in [[0.0, 5.0, 0.0], [5.0, 5.0, 5.0], [10.0, 5.0, 0.0]] {
        let hit = index
            .raycast(Ray::new(point, [0.0, 1.0, 0.0], 0.0).unwrap())
            .value
            .unwrap();
        assert_eq!(hit.distance, 0.0);
        assert_eq!(hit.position, point);
    }
}

#[test]
fn bad_meshes_are_rejected_atomically_and_bad_queries_do_not_poison_the_scene() {
    let mut builder = Builder::default();
    for invalid in [
        mesh(vec![[0.0; 3]], vec![[0, 1, 2]]),
        mesh(vec![[f32::NAN; 3]], vec![]),
    ] {
        assert!(builder.add(&source(0), &invalid).is_err());
    }
    builder.add(&source(10), &floor(0.0, 5.0, 0.0)).unwrap();
    let index = builder.finish();
    assert_eq!(index.sources().len(), 1);
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(index.heights_at(value, 0.0).is_err());
        assert!(index.heights_at(0.0, value).is_err());
        assert!(Ray::new([value, 0.0, 0.0], [1.0, 0.0, 0.0], 1.0).is_err());
        assert!(Ray::new([0.0; 3], [value, 0.0, 0.0], 1.0).is_err());
        assert!(Ray::new([0.0; 3], [1.0, 0.0, 0.0], value).is_err());
    }
    assert!(Ray::new([0.0; 3], [0.0; 3], 1.0).is_err());
    assert!(Ray::new([0.0; 3], [1.0, 0.0, 0.0], -1.0).is_err());
    assert_eq!(index.heights_at(2.0, 2.0).unwrap().value.len(), 1);
}

#[test]
fn distant_finite_ray_does_not_overflow_its_distance_numerator() {
    let index = scene(&[mesh(
        vec![[0.0; 3], [2.0, 0.0, 0.0], [0.0, 0.0, 2.0]],
        vec![[0, 1, 2]],
    )]);
    let hit = index
        .raycast(Ray::new([0.5, 5e307, 0.5], [0.0, -1.0, 0.0], 5e307).unwrap())
        .value
        .unwrap();
    assert_eq!(hit.distance, 5e307);
    assert_eq!(hit.position, [0.5, 0.0, 0.5]);
}

#[test]
fn a_later_visited_lower_key_wins_an_exact_distance_tie() {
    let lower_key = mesh(
        vec![[-1.0, 5.0, -1.0], [20.0, 5.0, -1.0], [-1.0, 5.0, 20.0]],
        vec![[0, 1, 2]],
    );
    let earlier_box = mesh(
        vec![[-10.0, 5.0, -10.0], [10.0, 5.0, -10.0], [-10.0, 5.0, 10.0]],
        vec![[0, 1, 2]],
    );
    let mut meshes = vec![lower_key];
    meshes.extend(std::iter::repeat_n(earlier_box, 32));
    let index = scene(&meshes);
    let hit = index
        .raycast(Ray::new([0.0, 10.0, 0.0], [0.0, -1.0, 0.0], 20.0).unwrap())
        .value
        .unwrap();
    assert_eq!(hit.key.source, 0);
    assert_eq!(hit.distance, 5.0);
}

#[test]
fn oblique_ray_selects_the_nearest_surface_not_insertion_order() {
    let wall = |world_x| {
        mesh(
            vec![
                [world_x, 0.0, 0.0],
                [world_x, 10.0, 0.0],
                [world_x, 0.0, 10.0],
            ],
            vec![[0, 1, 2]],
        )
    };
    let index = scene(&[wall(5.0), wall(0.0)]);
    let hit = index
        .raycast(Ray::new([-10.0, 2.0, 2.0], [1.0, 0.05, 0.1], 30.0).unwrap())
        .value
        .unwrap();
    assert_eq!(hit.key.source, 1);
    assert!((hit.position[0]).abs() < 1e-12);
    assert!((hit.position[1] - 2.5).abs() < 1e-12);
    assert!((hit.position[2] - 3.0).abs() < 1e-12);
}

#[test]
fn loading_from_a_non_directory_fails_instead_of_returning_an_empty_scene() {
    assert!(TerrainIndex::load_base(&std::env::current_exe().unwrap()).is_err());
}

#[test]
fn extreme_finite_direction_magnitudes_do_not_overflow_or_underflow_normalization() {
    let index = scene(&[floor(0.0, 5.0, 0.0)]);
    for magnitude in [1e-320, 1e308] {
        let hit = index
            .raycast(Ray::new([2.0, 10.0, 2.0], [0.0, -magnitude, 0.0], 5.0).unwrap())
            .value
            .unwrap();
        assert_eq!(hit.distance, 5.0);
    }
}
