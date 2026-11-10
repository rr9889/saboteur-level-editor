use sab_terrain_query::{Ray, TerrainIndex, TriangleKey};
use std::{
    io::{self, BufRead},
    path::Path,
    time::Instant,
};

fn identity(index: &TerrainIndex, key: TriangleKey) -> String {
    let source = index.source(key).unwrap();
    format!(
        "mesh={} pack={} cell={} crc={:08x} offset=0x{:x} triangle={}",
        source.mesh,
        source.location.pack,
        source.location.cell,
        source.location.crc,
        source.location.offset,
        key.triangle
    )
}

fn command(index: &TerrainIndex, line: &str) -> Result<(), String> {
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.is_empty() {
        return Ok(());
    }
    let values: Vec<f64> = fields[1..]
        .iter()
        .map(|value| value.parse::<f64>().map_err(|error| error.to_string()))
        .collect::<Result<_, _>>()?;
    let start = Instant::now();
    match (fields[0], values.as_slice()) {
        ("height", [world_x, world_z]) => {
            let query = index.heights_at(*world_x, *world_z)?;
            println!(
                "height x={world_x} z={world_z} hits={} triangles_tested={} nodes_tested={} us={}",
                query.value.len(),
                query.stats.triangles_tested,
                query.stats.nodes_tested,
                start.elapsed().as_micros()
            );
            for hit in query.value {
                println!("surface y={:.9} {}", hit.height, identity(index, hit.key));
            }
        }
        (
            "ray",
            [origin_x, origin_y, origin_z, direction_x, direction_y, direction_z, distance],
        ) => {
            let ray = Ray::new(
                [*origin_x, *origin_y, *origin_z],
                [*direction_x, *direction_y, *direction_z],
                *distance,
            )?;
            let query = index.raycast(ray);
            println!(
                "ray hit={} triangles_tested={} nodes_tested={} us={}",
                query.value.is_some(),
                query.stats.triangles_tested,
                query.stats.nodes_tested,
                start.elapsed().as_micros()
            );
            if let Some(hit) = query.value {
                println!(
                    "surface distance={:.9} position={:?} normal={:?} {}",
                    hit.distance,
                    hit.position,
                    hit.normal,
                    identity(index, hit.key)
                );
            }
        }
        _ => return Err("expected: height X Z | ray OX OY OZ DX DY DZ MAX_METRES".into()),
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 1 {
        return Err("usage: sab_terrain_query RETAIL_ROOT (commands on stdin)".into());
    }
    let start = Instant::now();
    let index = TerrainIndex::load_base(Path::new(&arguments[0]))?;
    eprintln!("base terrain loaded once: sources={} triangles={} nodes={} buffer_bytes={} elapsed_ms={}; no HEI1 fallback, patches, urban geometry or collision claim", index.sources().len(), index.triangle_count(), index.node_count(), index.buffer_bytes(), start.elapsed().as_millis());
    let mut errors = 0;
    for (number, line) in io::stdin().lock().lines().enumerate() {
        let line = line.map_err(|error| error.to_string())?;
        if let Err(error) = command(&index, &line) {
            eprintln!("line {}: {error}", number + 1);
            errors += 1;
        }
    }
    if errors == 0 {
        Ok(())
    } else {
        Err(format!("{errors} invalid command(s)"))
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
