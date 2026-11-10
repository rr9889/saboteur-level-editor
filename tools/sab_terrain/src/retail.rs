use crate::geometry::{self, TerrainMesh};
use sab_formats::{loosefiles, map6, megapack, sbla};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Debug)]
pub struct ChunkSource {
    pub pack: String,
    pub cell: u32,
    pub crc: u32,
    pub offset: u64,
}

pub struct CellHeightfield {
    pub tile: map6::Tile,
    pub heightfield: sbla::Heightfield,
}

#[derive(Default)]
pub struct Survey {
    pub cells: BTreeMap<u32, CellHeightfield>,
    pub dimensions: BTreeMap<(u32, u32), usize>,
    pub chunks: usize,
    pub vertices: usize,
    pub triangles: usize,
    pub formats: BTreeMap<u32, usize>,
    pub failures: Vec<String>,
}

pub fn visit(
    root: &Path,
    mut visitor: impl FnMut(&ChunkSource, &TerrainMesh),
) -> Result<Survey, String> {
    let loose =
        fs::read(root.join("France/loosefiles_BinPC.pack")).map_err(|error| error.to_string())?;
    let entries = loosefiles::entries(&loose)?;
    let entry = loosefiles::find(&entries, "France.map").ok_or("France.map missing")?;
    let map = map6::parse_region_main(&loose[entry.data.clone()])?;
    let tiles: BTreeMap<_, _> = map
        .tiles
        .into_iter()
        .map(|tile| (tile.hash, tile))
        .collect();
    let mut survey = Survey::default();
    for name in ["Mega0.megapack", "Mega1.megapack", "Mega2.megapack"] {
        let relative = format!("France/{name}");
        let file =
            fs::File::open(root.join(&relative)).map_err(|error| format!("{relative}: {error}"))?;
        let bytes =
            unsafe { memmap2::Mmap::map(&file) }.map_err(|error| format!("{relative}: {error}"))?;
        let pack = megapack::parse(&bytes)?;
        for entry in &pack.entries {
            let sub = megapack::entry_slice(&bytes, entry)?;
            if sub.get(8..12) != Some(&sbla::HEIGHTFIELD_TAG.to_le_bytes()) {
                for header in sab_formats::mesh::scan_msha(sub) {
                    if header.name.starts_with("France_Chunk") {
                        survey.failures.push(format!("{relative} cell {} {}: terrain candidate outside HEI1 sub-pack; unsupported source", entry.index, header.name));
                    }
                }
                continue;
            }
            let parsed = sbla::parse(sub, None)?;
            let heightfield = parsed
                .descriptor
                .as_ref()
                .and_then(|descriptor| descriptor.heightfield.clone())
                .ok_or("HEI1 absent")?;
            *survey
                .dimensions
                .entry((heightfield.w, heightfield.h))
                .or_default() += 1;
            let tile = tiles
                .get(&entry.index)
                .ok_or("cell lacks map tile")?
                .clone();
            if survey
                .cells
                .insert(entry.index, CellHeightfield { tile, heightfield })
                .is_some()
            {
                return Err("duplicate base cell id; mount policy needs review".into());
            }
            let mut offset = 8 + sbla::descriptor_len(sub)? as usize + parsed.records.len() * 24;
            for (record, blob) in parsed.records.iter().zip(&parsed.blobs) {
                if record.kind == 0 {
                    let header = sab_formats::mesh::parse_msha(blob)?;
                    if header.name.starts_with("France_Chunk") {
                        let source = ChunkSource {
                            pack: relative.clone(),
                            cell: entry.index,
                            crc: entry.crc,
                            offset: entry.offset + offset as u64,
                        };
                        match geometry::decode(blob) {
                            Ok(mesh) => {
                                survey.chunks += 1;
                                survey.vertices += mesh.vertices.len();
                                survey.triangles += mesh.triangles.len();
                                for stream in &mesh.streams {
                                    *survey.formats.entry(stream.format).or_default() += 1;
                                }
                                visitor(&source, &mesh);
                            }
                            Err(error) => survey.failures.push(format!(
                                "{relative} cell {} {}: {error}",
                                entry.index, header.name
                            )),
                        }
                    }
                }
                offset += blob.len();
            }
        }
    }
    Ok(survey)
}
