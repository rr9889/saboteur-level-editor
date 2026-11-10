use sab_terrain::{geometry::TerrainMesh, retail};
use sab_terrain_query::{Builder, Ray, TerrainIndex, TriangleKey};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

#[test]
fn batch_cli_keeps_processing_after_invalid_lines_and_exits_nonzero() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_sab_terrain_query"))
        .arg(game_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"height 30 30\nheight NaN 0\nray 0 10 0 0 0 0 20\nunknown\nray 18.75 100 46.875 0 -1 0 200\nheight 487.5 37.5\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success());
    assert_eq!(stderr.matches("base terrain loaded once:").count(), 1);
    assert!(stderr.contains("line 2:") && stderr.contains("line 3:") && stderr.contains("line 4:"));
    assert!(stderr.contains("3 invalid command(s)"));
    assert!(stdout.contains("height x=30 z=30 hits=0"));
    assert!(stdout.contains("ray hit=true") && stdout.contains("mesh=France_Chunk300_000"));
    assert!(
        stdout.contains("height x=487.5 z=37.5 hits=6")
            && stdout.contains("surface y=42.000000000")
    );
}

fn game_root() -> PathBuf {
    std::env::var_os("SAB_GAME_DIR")
        .map(PathBuf::from)
        .expect("retail tests require SAB_GAME_DIR")
}

fn grouped(index: &TerrainIndex, world_x: f64, world_z: f64) -> BTreeMap<u32, Vec<f64>> {
    let mut groups = BTreeMap::<u32, Vec<f64>>::new();
    for hit in index.heights_at(world_x, world_z).unwrap().value {
        groups.entry(hit.key.source).or_default().push(hit.height);
    }
    for heights in groups.values_mut() {
        heights.sort_by(f64::total_cmp);
        heights.dedup_by(|left, right| (*left - *right).abs() < 1e-5);
    }
    groups
}

fn subtract(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|axis| left[axis] - right[axis])
}
fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    (0..3).map(|axis| left[axis] * right[axis]).sum()
}
fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn plane_edge_distance(
    vertices: [[f32; 3]; 3],
    origin: [f64; 3],
    direction: [f64; 3],
    limit: f64,
) -> Option<f64> {
    let [first, second, third] = vertices.map(|vertex| vertex.map(f64::from));
    let normal = cross(subtract(second, first), subtract(third, first));
    let normal_length = normal[0].hypot(normal[1]).hypot(normal[2]);
    if normal_length == 0.0 {
        return None;
    }
    let denominator = dot(normal, direction);
    if (denominator / normal_length).abs() < 1e-12 {
        return None;
    }
    let distance = dot(normal, subtract(first, origin)) / denominator;
    if !distance.is_finite() || distance < 0.0 || distance > limit {
        return None;
    }
    let point = std::array::from_fn(|axis| origin[axis] + distance * direction[axis]);
    let area_squared = dot(normal, normal);
    let weights = [
        dot(
            cross(subtract(second, point), subtract(third, point)),
            normal,
        ) / area_squared,
        dot(
            cross(subtract(third, point), subtract(first, point)),
            normal,
        ) / area_squared,
        dot(
            cross(subtract(first, point), subtract(second, point)),
            normal,
        ) / area_squared,
    ];
    weights
        .into_iter()
        .all(|weight| weight >= -1e-8)
        .then_some(distance)
}

fn linear_ray(
    meshes: &[TerrainMesh],
    origin: [f64; 3],
    direction: [f64; 3],
    limit: f64,
) -> Option<(f64, TriangleKey)> {
    let length = direction[0].hypot(direction[1]).hypot(direction[2]);
    let direction = direction.map(|value| value / length);
    let endpoint = std::array::from_fn::<_, 3, _>(|axis| origin[axis] + direction[axis] * limit);
    let mut closest: Option<(f64, TriangleKey)> = None;
    for (source, mesh) in meshes.iter().enumerate() {
        if (0..3).any(|axis| {
            origin[axis].min(endpoint[axis]) > f64::from(mesh.bounds[1][axis]) + 0.002
                || origin[axis].max(endpoint[axis]) < f64::from(mesh.bounds[0][axis]) - 0.002
        }) {
            continue;
        }
        for (triangle, indices) in mesh.triangles.iter().enumerate() {
            if let Some(distance) = plane_edge_distance(
                indices.map(|index| mesh.vertices[index]),
                origin,
                direction,
                limit,
            ) {
                let key = TriangleKey {
                    source: source as u32,
                    triangle: triangle as u32,
                };
                if closest.is_none_or(|current| {
                    distance
                        .total_cmp(&current.0)
                        .then(key.cmp(&current.1))
                        .is_lt()
                }) {
                    closest = Some((distance, key));
                }
            }
        }
    }
    closest
}

#[test]
fn indexed_retail_queries_match_linear_oracles_and_keep_holes() {
    let start = Instant::now();
    let mut builder = Builder::default();
    let mut meshes = Vec::new();
    let survey = retail::visit(&game_root(), |source, mesh| {
        builder.add(source, mesh).unwrap();
        meshes.push(TerrainMesh {
            name: mesh.name.clone(),
            streams: vec![],
            vertices: mesh.vertices.clone(),
            triangles: mesh.triangles.clone(),
            bounds: mesh.bounds,
        });
    })
    .unwrap();
    assert!(survey.failures.is_empty(), "{:?}", survey.failures);
    assert_eq!(
        (survey.cells.len(), survey.chunks, survey.triangles),
        (7731, 6032, 2_254_289)
    );
    let scan_ms = start.elapsed().as_millis();
    let start = Instant::now();
    let index = builder.finish();
    let build_ms = start.elapsed().as_millis();
    assert_eq!(index.triangle_count(), survey.triangles);
    let mut points = vec![
        [18.75, 46.875],
        [487.5, 37.5],
        [-20.0, -20.0],
        [450.0, -2970.0],
        [30.0, 30.0],
        [776.78, -686.09],
    ];
    for mesh in meshes.iter().step_by(31) {
        if let Some(triangle) = mesh.triangles.first() {
            let centroid = [0, 2].map(|axis| {
                triangle
                    .iter()
                    .map(|vertex| f64::from(mesh.vertices[*vertex][axis]))
                    .sum::<f64>()
                    / 3.0
            });
            points.push(centroid);
        }
    }
    for mesh in meshes.iter().step_by(193) {
        if let Some(vertex) = mesh.vertices.first() {
            for offset in [-1e-7, 0.0, 1e-7] {
                points.push([f64::from(vertex[0]) + offset, f64::from(vertex[2]) + offset]);
            }
        }
    }
    for ordinal in 0..64 {
        points.push([
            f64::from((ordinal * 3571) % 8400) - 4200.0,
            f64::from((ordinal * 1879) % 9000) - 4500.0,
        ]);
    }
    for &[world_x, world_z] in &points {
        let expected: BTreeMap<_, _> = meshes
            .iter()
            .enumerate()
            .filter_map(|(source, mesh)| {
                let heights = mesh.heights_at(world_x, world_z);
                (!heights.is_empty()).then_some((source as u32, heights))
            })
            .collect();
        let actual = grouped(&index, world_x, world_z);
        assert_eq!(actual, expected, "height mismatch at {world_x},{world_z}");
    }
    for point in [[30.0, 30.0], [776.78, -686.09]] {
        assert!(index
            .heights_at(point[0], point[1])
            .unwrap()
            .value
            .is_empty());
        assert!(index
            .raycast(Ray::new([point[0], 1000.0, point[1]], [0.0, -1.0, 0.0], 2000.0).unwrap())
            .value
            .is_none());
    }
    let seam = index.heights_at(450.0, -2970.0).unwrap();
    let cells: Vec<_> = seam
        .value
        .iter()
        .map(|hit| index.source(hit.key).unwrap().location.cell)
        .collect();
    assert!(cells.contains(&1_166_849) && cells.contains(&904_705));
    assert!(seam
        .value
        .iter()
        .all(|hit| (hit.height - 70.7208).abs() < 0.001));

    let mut ray_count = 0;
    let mut ray_hits = 0;
    for &[world_x, world_z] in points.iter().step_by(3) {
        let height = index
            .heights_at(world_x, world_z)
            .unwrap()
            .value
            .last()
            .map_or(100.0, |hit| hit.height);
        for direction in [[0.0, -1.0, 0.0], [0.25, -1.0, 0.125], [-0.25, 1.0, -0.125]] {
            let origin = [
                world_x,
                height + if direction[1] < 0.0 { 25.0 } else { -25.0 },
                world_z,
            ];
            let actual = index
                .raycast(Ray::new(origin, direction, 100.0).unwrap())
                .value;
            let expected = linear_ray(&meshes, origin, direction, 100.0);
            assert_eq!(
                actual.is_some(),
                expected.is_some(),
                "ray miss mismatch {origin:?} {direction:?}"
            );
            if let (Some(actual), Some((expected, _))) = (actual, expected) {
                assert!(
                    (actual.distance - expected).abs() < 1e-6,
                    "ray distance {} != {expected}",
                    actual.distance
                );
                let triangle =
                    &meshes[actual.key.source as usize].triangles[actual.key.triangle as usize];
                let vertices =
                    triangle.map(|vertex| meshes[actual.key.source as usize].vertices[vertex]);
                let length = direction[0].hypot(direction[1]).hypot(direction[2]);
                let own = plane_edge_distance(
                    vertices,
                    origin,
                    direction.map(|value| value / length),
                    100.0,
                )
                .unwrap();
                assert!((own - actual.distance).abs() < 1e-6);
                ray_hits += 1;
            }
            ray_count += 1;
        }
    }
    assert!(ray_hits > 100);
    let sample = index.heights_at(18.75, 46.875).unwrap().value;
    assert!(sample
        .iter()
        .any(|hit| (hit.height - 40.05082321166992).abs() < 1e-8));

    let mut height_candidates = 0;
    let mut ray_candidates = 0;
    let mut max_height_candidates = 0;
    let start = Instant::now();
    for ordinal in 0..10_000 {
        let [world_x, world_z] = points[ordinal % points.len()];
        let query = index.heights_at(world_x, world_z).unwrap();
        height_candidates += query.stats.triangles_tested;
        max_height_candidates = max_height_candidates.max(query.stats.triangles_tested);
        std::hint::black_box(query.value);
    }
    let height_us = start.elapsed().as_micros();
    let start = Instant::now();
    for ordinal in 0..10_000 {
        let [world_x, world_z] = points[ordinal % points.len()];
        let query =
            index.raycast(Ray::new([world_x, 1000.0, world_z], [0.0, -1.0, 0.0], 2000.0).unwrap());
        ray_candidates += query.stats.triangles_tested;
        std::hint::black_box(query.value);
    }
    let ray_us = start.elapsed().as_micros();
    assert!(max_height_candidates < index.triangle_count());
    println!("retail scan_ms={scan_ms} build_ms={build_ms} sources={} triangles={} nodes={} buffer_bytes={}", index.sources().len(), index.triangle_count(), index.node_count(), index.buffer_bytes());
    println!("linear parity heights={} rays={ray_count} ray_hits={ray_hits}; known holes and cross-cell seam preserved", points.len());
    println!("10000 indexed heights: total_us={height_us} avg_us={:.3} avg_triangles={:.3} max_triangles={max_height_candidates}", height_us as f64 / 10000.0, height_candidates as f64 / 10000.0);
    println!(
        "10000 indexed rays: total_us={ray_us} avg_us={:.3} avg_triangles={:.3}",
        ray_us as f64 / 10000.0,
        ray_candidates as f64 / 10000.0
    );
}
