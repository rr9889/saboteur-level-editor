use sab_terrain::{height::HeightGrid, retail};
use std::{collections::BTreeMap, fmt::Write as _, io::Write as _, path::Path};

fn run() -> Result<(), String> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.len() < 2 {
        return Err("usage: sab_terrain survey RETAIL_ROOT | sample RETAIL_ROOT WORLD_X WORLD_Z | obj RETAIL_ROOT MIN_X MIN_Z OUTPUT.obj".into());
    }
    let exporting = arguments[0] == "obj";
    let query = match arguments[0].as_str() {
        "survey" if arguments.len() == 2 => None,
        "sample" | "obj"
            if (arguments[0] == "sample" && arguments.len() == 4)
                || (exporting && arguments.len() == 5) =>
        {
            let world_x = arguments[2]
                .parse::<f64>()
                .map_err(|error| error.to_string())?;
            let world_z = arguments[3]
                .parse::<f64>()
                .map_err(|error| error.to_string())?;
            if !world_x.is_finite() || !world_z.is_finite() {
                return Err("coordinates must be finite".into());
            }
            if exporting
                && (world_x.rem_euclid(60.0) != 0.0
                    || world_z.rem_euclid(60.0) != 0.0
                    || world_x + 60.0 <= world_x
                    || world_z + 60.0 <= world_z)
            {
                return Err("OBJ minimum must lie on the finite 60m world grid".into());
            }
            Some((world_x, world_z))
        }
        _ => return Err("invalid command or arguments".into()),
    };
    let mut shared = 0;
    let mut shared_mismatches = 0;
    let mut outside_boxes = 0;
    let mut chunk_sizes = BTreeMap::new();
    let mut hits = 0;
    let mut obj = String::new();
    let mut obj_vertices = 0;
    let mut obj_triangles = 0;
    let survey = retail::visit(Path::new(&arguments[1]), |source, mesh| {
        *chunk_sizes
            .entry((
                (mesh.bounds[1][0] - mesh.bounds[0][0]).round() as i32,
                (mesh.bounds[1][2] - mesh.bounds[0][2]).round() as i32,
            ))
            .or_insert(0usize) += 1;
        for stream in &mesh.streams {
            for position in &stream.positions {
                if (0..3).any(|axis| {
                    position[axis] < stream.bounds[0][axis] - 0.002 - stream.quantization
                        || position[axis] > stream.bounds[1][axis] + 0.002 + stream.quantization
                }) {
                    outside_boxes += 1;
                }
            }
        }
        for packed in mesh
            .streams
            .iter()
            .filter(|stream| stream.format & 0x8000_0000 != 0)
        {
            let float_positions: Vec<_> = mesh
                .streams
                .iter()
                .filter(|stream| stream.format & 0x8000_0000 == 0)
                .flat_map(|stream| &stream.positions)
                .collect();
            for position in &packed.positions {
                let matches: Vec<_> = float_positions
                    .iter()
                    .filter(|other| {
                        (other[0] - position[0]).abs() < 0.0005
                            && (other[2] - position[2]).abs() < 0.0005
                    })
                    .collect();
                if !matches.is_empty() {
                    shared += 1;
                    if !matches
                        .iter()
                        .any(|other| (other[1] - position[1]).abs() <= packed.quantization + 0.0005)
                    {
                        shared_mismatches += 1;
                        eprintln!(
                            "seam outlier {} packed={position:?} float_y={:?} quantization={}",
                            mesh.name,
                            matches.iter().map(|other| other[1]).collect::<Vec<_>>(),
                            packed.quantization
                        );
                    }
                }
            }
        }
        if let Some((world_x, world_z)) = query.filter(|_| !exporting) {
            for height in mesh.heights_at(world_x, world_z) {
                println!(
                    "terrain_y={height:.6} mesh={} pack={} cell={} crc={:08x} msha_offset=0x{:x}",
                    mesh.name, source.pack, source.cell, source.crc, source.offset
                );
                hits += 1;
            }
        }
        if let Some((min_x, min_z)) = query.filter(|_| exporting) {
            let triangles = mesh
                .clipped_triangles(min_x, min_z, min_x + 60.0, min_z + 60.0)
                .expect("validated finite rectangle");
            if !triangles.is_empty() {
                writeln!(obj, "o {}", mesh.name).unwrap();
                for triangle in triangles {
                    for position in triangle {
                        writeln!(
                            obj,
                            "v {:.9} {:.9} {:.9}",
                            position[0], position[1], position[2]
                        )
                        .unwrap();
                    }
                    writeln!(
                        obj,
                        "f {} {} {}",
                        obj_vertices + 1,
                        obj_vertices + 2,
                        obj_vertices + 3
                    )
                    .unwrap();
                    obj_vertices += 3;
                    obj_triangles += 1;
                }
            }
        }
    })?;
    println!("base_only=true cells={} HEI1_dimensions={:?} chunks={} vertices={} triangles={} decode_failures={}", survey.cells.len(), survey.dimensions, survey.chunks, survey.vertices, survey.triangles, survey.failures.len());
    let formats: BTreeMap<_, _> = survey
        .formats
        .iter()
        .map(|(format, count)| (format!("{format:08x}"), count))
        .collect();
    println!("formats={formats:?} chunk_sizes={chunk_sizes:?} vertices_outside_stream_boxes={outside_boxes} packed_float_shared_xz={shared} shared_height_mismatches={shared_mismatches}");
    for failure in survey.failures.iter().take(8) {
        eprintln!("unsupported: {failure}");
    }
    if !survey.failures.is_empty() {
        return Err("partial terrain coverage: unsupported chunks".into());
    }
    if let Some((world_x, world_z)) = query.filter(|_| !exporting) {
        if hits == 0 {
            println!("no decoded terrain triangle at query; no height fabricated");
        }
        for (cell, field) in &survey.cells {
            if world_x < f64::from(field.tile.a[0])
                || world_x >= f64::from(field.tile.b[0])
                || world_z < f64::from(field.tile.a[2])
                || world_z >= f64::from(field.tile.b[2])
            {
                continue;
            }
            let grid = HeightGrid::new(&field.heightfield, field.tile.a, field.tile.b)?;
            println!("HEI1_proxy_cell={cell} bilinear_y={:.6} engine_surface_y={:.6} endpoints=({}, {}) spacing=6.666667; coarse proxy, NOT exact/collision ground", grid.bilinear(world_x, world_z).unwrap(), grid.engine_surface(world_x, world_z).unwrap(), field.heightfield.f0, field.heightfield.f1);
        }
    }
    if exporting {
        if obj_triangles == 0 {
            return Err("no terrain triangles in square; no OBJ fabricated".into());
        }
        let destination = Path::new(&arguments[4]);
        let root = Path::new(&arguments[1])
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let root_text = root.to_string_lossy().to_ascii_lowercase();
        let parent_text = parent.to_string_lossy().to_ascii_lowercase();
        if parent_text == root_text
            || parent_text.starts_with(&(root_text + std::path::MAIN_SEPARATOR_STR))
        {
            return Err("refusing to write inside retail installation".into());
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .map_err(|error| error.to_string())?;
        output
            .write_all(obj.as_bytes())
            .map_err(|error| error.to_string())?;
        println!("OBJ={} vertices={obj_vertices} triangles={obj_triangles}; actual clipped terrain only, no filled holes", destination.display());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
