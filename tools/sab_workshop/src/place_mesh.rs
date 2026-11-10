//! Real meshes for EditNodes objects and terrain-cell placements in the places view.
//!
//! An object's `ClassName` names a GameTemplates template; when that template is an asset the
//! workshop assembles (`assets::build`: props, vehicles, weapons, characters), its meshes from
//! `Global/Dynamic0.megapack` are the object's geometry. Everything else (locators, triggers, spores,
//! spawners, templates whose meshes this install lacks) has no mesh here and stays a marker.
//! Terrain-cell requests first resolve the same cell's type-0 record by ClassName hash through
//! `place_cell_mesh`, then use the existing template path only when that record is absent.
//!
//! Geometry is the assembly's coarsest level of detail, intact damage state, in the asset's bind
//! space. It is placed with the object's `XAxis`/`YAxis`/`ZAxis` as the model matrix's columns and
//! `Position` as its translation (right-handed, Y up), consistent with the stored row-vector
//! engine transform described in `docs/level_editor/point1_resolution.md`.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use crate::assets::Asset;
use crate::source::GameSource;

/// Object-space triangles of one class, ready to place.
pub struct Geom {
    /// Three corners per triangle.
    pub tris: Vec<[[f32; 3]; 3]>,
    /// One unit normal per triangle (from its corners).
    pub normals: Vec<[f32; 3]>,
    /// The mesh's UV at each corner of each triangle (zero where the mesh has none).
    pub uvs: Vec<[[f32; 2]; 3]>,
    /// Consecutive triangle ranges that share one material, covering `tris` in order.
    pub ranges: Vec<GeomRange>,
    /// Where it came from: template, meshes and their records.
    pub provenance: String,
}

/// A run of triangles drawn with one material (`Prim::material_hash`; 0 when the mesh has no prims).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeomRange {
    pub first: u32,
    pub count: u32,
    pub material: u32,
}

/// A mesh flattened to triangles at one level of detail.
pub struct Flat {
    pub tris: Vec<[[f32; 3]; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[[f32; 2]; 3]>,
    pub ranges: Vec<GeomRange>,
    /// The level of detail taken.
    pub lod: u8,
}

impl Flat {
    pub fn into_geom(self, provenance: String) -> Geom {
        Geom { tris: self.tris, normals: self.normals, uvs: self.uvs, ranges: self.ranges, provenance }
    }
}

/// Why a class has no mesh, or its geometry.
pub enum Slot {
    Pending,
    Ready(Arc<Geom>),
    None(String),
}

/// Resolves `ClassName`s to geometry on a worker thread, caching every answer.
pub struct MeshLib {
    src: Arc<dyn GameSource>,
    by_name: Arc<HashMap<String, usize>>,
    assets: Arc<Vec<Asset>>,
    cache: HashMap<String, Slot>,
    tx: Sender<(String, Result<Geom, String>)>,
    rx: Receiver<(String, Result<Geom, String>)>,
    req: Option<Sender<String>>,
    cells: Option<crate::place_cell_mesh::CellMeshes>,
}

impl MeshLib {
    pub fn new(src: Arc<dyn GameSource>) -> MeshLib {
        let assets = crate::assets::build(src.as_ref());
        let by_name = assets.iter().enumerate().filter(|(_, a)| a.category != crate::assets::WORLD).map(|(i, a)| (a.name.clone(), i)).collect();
        let (tx, rx) = channel();
        MeshLib { src, by_name: Arc::new(by_name), assets: Arc::new(assets), cache: HashMap::new(), tx, rx, req: None, cells: None }
    }

    /// The source label the meshes are read from.
    pub fn source_label(&self) -> String {
        self.src.label()
    }

    /// Collect finished loads. Returns true when anything arrived.
    pub fn poll(&mut self) -> bool {
        let mut any = self.cells.as_mut().map_or(false, |cells| cells.poll());
        while let Ok((name, r)) = self.rx.try_recv() {
            any = true;
            self.cache.insert(name, match r {
                Ok(g) => Slot::Ready(Arc::new(g)),
                Err(e) => Slot::None(e),
            });
        }
        any
    }

    /// Whether any requested class is still loading (the material set is not final yet).
    pub fn pending(&self) -> bool {
        self.cache.values().any(|s| matches!(s, Slot::Pending)) || self.cells.as_ref().map_or(false, |cells| cells.pending())
    }

    /// Every material hash used by the loaded geometry, for one diffuse-texture resolve pass. 0
    /// (meshes without prims) is not a material and is skipped.
    pub fn materials(&self) -> std::collections::BTreeSet<u32> {
        let mut out = std::collections::BTreeSet::new();
        if let Some(cells) = &self.cells {
            out.extend(cells.materials());
        }
        for slot in self.cache.values() {
            if let Slot::Ready(g) = slot {
                out.extend(g.ranges.iter().map(|r| r.material).filter(|m| *m != 0));
            }
        }
        out
    }

    pub fn texture_cells(&self) -> std::collections::BTreeSet<u32> {
        self.cells.as_ref().map_or_else(Default::default, |cells| cells.loaded_cells())
    }

    /// The answer for `class`, queuing a load the first time it is asked.
    pub fn get(&mut self, class: &str) -> &Slot {
        if !self.cache.contains_key(class) {
            let slot = if !self.by_name.contains_key(class) {
                match self.src.templates().find(class) {
                    Some((_, t)) => Slot::None(format!("template {class} ({}) assembles no mesh in this install", t.ttype)),
                    None => Slot::None(format!("no GameTemplates entry named {class}")),
                }
            } else {
                self.queue(class.to_string());
                Slot::Pending
            };
            self.cache.insert(class.to_string(), slot);
        }
        &self.cache[class]
    }

    pub fn get_cell(&mut self, game: &std::path::Path, cell: u32, class: &str) -> &Slot {
        if self.cells.as_ref().map_or(true, |cells| cells.game != game) {
            self.cells = Some(crate::place_cell_mesh::CellMeshes::new(game));
        }
        let key = crate::place_cell_mesh::Key::new(cell, class);
        let absent = matches!(self.cells.as_mut().unwrap().get(key.clone()), crate::place_cell_mesh::Lookup::Absent);
        if absent {
            return self.get(class);
        }
        match self.cells.as_mut().unwrap().get(key) {
            crate::place_cell_mesh::Lookup::Mesh(slot) => slot,
            crate::place_cell_mesh::Lookup::Absent => unreachable!(),
        }
    }

    fn queue(&mut self, class: String) {
        if self.req.is_none() {
            let (rtx, rrx) = channel::<String>();
            let (src, by_name, assets, tx) = (self.src.clone(), self.by_name.clone(), self.assets.clone(), self.tx.clone());
            std::thread::spawn(move || {
                while let Ok(name) = rrx.recv() {
                    let r = match by_name.get(&name) {
                        Some(&i) => load(src.as_ref(), &assets[i]),
                        None => Err(format!("no asset named {name}")),
                    };
                    if tx.send((name, r)).is_err() {
                        break;
                    }
                }
            });
            self.req = Some(rtx);
        }
        if let Some(r) = &self.req {
            let _ = r.send(class);
        }
    }
}

/// Assemble `asset` and keep its coarsest level, intact state, as flat triangles.
pub fn load(src: &dyn GameSource, asset: &Asset) -> Result<Geom, String> {
    let asm = asset.assembly();
    let model = crate::boot::assemble(src, &asset.name, &asm)?;
    let flat = flatten(&model.lm.mesh, true);
    if flat.tris.is_empty() {
        return Err(format!("{}: the assembly has no triangles", asset.name));
    }
    let meshes = src.meshes();
    let parts: Vec<String> = asm.picks.iter().map(|(i, _)| format!("{} ({})", meshes[*i].name, meshes[*i].location)).collect();
    let provenance = format!("template {} → {} · LOD{}, {} tris", asset.name, parts.join(", "), flat.lod, flat.tris.len());
    Ok(flat.into_geom(provenance))
}

/// A mesh's triangles at one level of detail (the coarsest, or else the finest) in the intact
/// damage state, with a normal and UVs per triangle, grouped into material ranges.
pub fn flatten(m: &crate::formats::Smsh, coarsest: bool) -> Flat {
    let lods = m.prims.iter().map(|p| p.lod);
    let lod = if coarsest { lods.max() } else { lods.min() }.unwrap_or(0);
    let mut out = Flat { tris: Vec::new(), normals: Vec::new(), uvs: Vec::new(), ranges: Vec::new(), lod };
    let mut push = |range: std::ops::Range<usize>, material: u32| {
        let first = out.tris.len() as u32;
        for t in m.indices.get(range).unwrap_or(&[]).chunks_exact(3) {
            let ix = [t[0], t[1], t[2]];
            let c = ix.map(|i| m.positions.get(i as usize).copied().unwrap_or([0.0; 3]));
            out.normals.push(tri_normal(&c));
            out.uvs.push(ix.map(|i| m.uvs.get(i as usize).copied().unwrap_or([0.0; 2])));
            out.tris.push(c);
        }
        let count = out.tris.len() as u32 - first;
        if count > 0 {
            out.ranges.push(GeomRange { first, count, material });
        }
    };
    if m.prims.is_empty() {
        push(0..m.indices.len(), 0);
    } else {
        for p in m.prims.iter().filter(|p| p.lod == lod && p.damage.map_or(true, |d| d == 0)) {
            push(p.index_start as usize..(p.index_start + p.index_count) as usize, p.material_hash);
        }
    }
    out
}

fn tri_normal(c: &[[f32; 3]; 3]) -> [f32; 3] {
    let a = glam::Vec3::from(c[1]) - glam::Vec3::from(c[0]);
    let b = glam::Vec3::from(c[2]) - glam::Vec3::from(c[0]);
    a.cross(b).normalize_or_zero().into()
}

/// A world point from an object-space one: `x·XAxis + y·YAxis + z·ZAxis + Position`.
pub fn place(p: [f32; 3], axes: &[[f64; 3]; 3], pos: [f64; 3]) -> [f64; 3] {
    let (x, y, z) = (p[0] as f64, p[1] as f64, p[2] as f64);
    [0, 1, 2].map(|k| x * axes[0][k] + y * axes[1][k] + z * axes[2][k] + pos[k])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flattening keeps one UV triple per triangle and covers the triangles with material ranges in
    /// order, merging nothing across prims.
    #[test]
    fn flatten_carries_uvs_and_material_ranges() {
        use crate::formats::{Prim, Smsh};
        let prim = |start: u32, count: u32, material: u32| Prim { index_start: start, index_count: count, material_hash: material, flags: 0, parent_bone: 0, lod: 0, damage: None };
        let m = Smsh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
            joints: Vec::new(),
            weights: Vec::new(),
            indices: vec![0, 1, 2, 1, 3, 2],
            prims: vec![prim(0, 3, 0xAAAA), prim(3, 3, 0xBBBB)],
        };
        let f = flatten(&m, false);
        assert_eq!(f.tris.len(), 2);
        assert_eq!(f.uvs[1], [[1.0, 0.0], [1.0, 1.0], [0.0, 1.0]], "UVs follow the corner indices");
        assert_eq!(f.ranges, vec![GeomRange { first: 0, count: 1, material: 0xAAAA }, GeomRange { first: 1, count: 1, material: 0xBBBB }]);
    }

    #[test]
    fn placing_uses_the_axes_as_columns_and_position_as_translation() {
        // A yaw of +90° about Y: XAxis = -Z, ZAxis = +X (right-handed).
        let axes = [[0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]];
        assert_eq!(place([1.0, 0.0, 0.0], &axes, [10.0, 2.0, 3.0]), [10.0, 2.0, 2.0]);
        assert_eq!(place([0.0, 0.0, 1.0], &axes, [10.0, 2.0, 3.0]), [11.0, 2.0, 3.0]);
        assert_eq!(place([0.0, 1.0, 0.0], &axes, [0.0; 3]), [0.0, 1.0, 0.0]);
    }

    /// On the user's install, a placed prop class resolves to Dynamic0 geometry with provenance, and
    /// a locator class does not.
    #[cfg(feature = "retail")]
    #[test]
    fn a_placed_prop_class_resolves_to_its_mesh() {
        let src = crate::testenv::retail();
        let all = crate::assets::build(src);
        let chair = all.iter().find(|a| a.name == "P_INT_chair2").expect("P_INT_chair2 asset");
        let g = load(src, chair).unwrap();
        assert!(g.tris.len() > 20, "{} tris", g.tris.len());
        assert!(g.provenance.contains("template P_INT_chair2") && g.provenance.contains("Dynamic0 bundle"), "{}", g.provenance);
        let extent = g.tris.iter().flatten().fold(0f32, |m, p| m.max(p[0].abs()).max(p[1].abs()).max(p[2].abs()));
        assert!(extent > 0.2 && extent < 5.0, "a chair-sized extent, got {extent} m");
        assert!(!all.iter().any(|a| a.name == "Locator"));
    }

    #[cfg(feature = "retail")]
    #[test]
    fn cell_worker_resolves_buildings_and_retains_template_fallback() {
        let game = sab_formats::game_paths::local_config_retail_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let src = Arc::new(crate::sources::retail_pc::RetailPcSource::open(&game, &crate::source::Overrides::default()).unwrap());
        let mut library = MeshLib::new(src);
        let mounts = sab_tools::cells::france_mounts(&game).unwrap();
        let (mount, entry) = sab_tools::cells::resolve_entry(&mounts, sab_tools::cells::cell_crc(132609)).unwrap();
        let bytes = sab_formats::megapack::entry_slice(&mount.map, entry).unwrap();
        let pack = sab_formats::sbla::parse(bytes, None).unwrap();
        let name = pack.records.iter().zip(&pack.blobs).filter(|(record, _)| record.kind == 0).filter_map(|(_, blob)| sab_formats::mesh::parse_msha(blob).ok()).find(|header| header.c0 > 0 && header.name.to_ascii_lowercase().starts_with("city1_")).unwrap().name;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let geometry = loop {
            library.poll();
            match library.get_cell(&game, 132609, &name) {
                Slot::Ready(geometry) => break geometry.clone(),
                Slot::None(error) => panic!("{error}"),
                Slot::Pending => {}
            }
            assert!(std::time::Instant::now() < deadline, "building load timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(geometry.provenance.contains("same-cell"));
        match library.get_cell(&game, 132609, &name.to_ascii_lowercase()) {
            Slot::Ready(other) => assert!(Arc::ptr_eq(&geometry, other)),
            _ => panic!("case variants must share geometry"),
        }
        assert!(!library.materials().is_empty());
        loop {
            library.poll();
            match library.get_cell(&game, 132609, "P_INT_chair2") {
                Slot::Ready(geometry) => {
                    assert!(geometry.provenance.contains("template P_INT_chair2"));
                    break;
                }
                Slot::None(error) => panic!("{error}"),
                Slot::Pending => {}
            }
            assert!(std::time::Instant::now() < deadline, "template fallback timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
