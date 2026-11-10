use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use sab_formats::{megapack, mesh, pandemic_hash, sbla};
use sab_tools::cells::{self, Mount};

use crate::meshload::MeshEntry;
use crate::place_mesh::{Geom, Slot};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub cell: u32,
    pub class: String,
}

impl Key {
    pub fn new(cell: u32, class: &str) -> Self {
        Self { cell, class: class.to_ascii_lowercase() }
    }

    pub fn gpu_key(&self, game: &Path) -> String {
        format!("cell|{}|{}|{}", game.display(), self.cell, self.class)
    }
}

pub enum Lookup {
    Absent,
    Mesh(Slot),
}

pub struct CellMeshes {
    pub game: PathBuf,
    cache: HashMap<Key, Lookup>,
    request: Sender<Key>,
    reply: Receiver<(Key, Result<Option<Geom>, String>)>,
}

impl CellMeshes {
    pub fn new(game: &Path) -> Self {
        let (request, requests) = channel::<Key>();
        let (replies, reply) = channel();
        let root = game.to_path_buf();
        std::thread::spawn(move || {
            let mut source = CellSource::open(&root);
            while let Ok(key) = requests.recv() {
                let result = match &mut source {
                    Ok(source) => source.load(&key),
                    Err(error) => Err(error.clone()),
                };
                if replies.send((key, result)).is_err() {
                    break;
                }
            }
        });
        Self { game: game.to_path_buf(), cache: HashMap::new(), request, reply }
    }

    pub fn get(&mut self, key: Key) -> &Lookup {
        if !self.cache.contains_key(&key) {
            let lookup = match self.request.send(key.clone()) {
                Ok(()) => Lookup::Mesh(Slot::Pending),
                Err(_) => Lookup::Mesh(Slot::None("cell mesh loader stopped".into())),
            };
            self.cache.insert(key.clone(), lookup);
        }
        &self.cache[&key]
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.reply.try_recv() {
                Ok((key, result)) => {
                    changed = true;
                    let lookup = match result {
                        Ok(Some(geometry)) => Lookup::Mesh(Slot::Ready(Arc::new(geometry))),
                        Ok(None) => Lookup::Absent,
                        Err(error) => Lookup::Mesh(Slot::None(error)),
                    };
                    self.cache.insert(key, lookup);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    for lookup in self.cache.values_mut() {
                        if matches!(lookup, Lookup::Mesh(Slot::Pending)) {
                            *lookup = Lookup::Mesh(Slot::None("cell mesh loader stopped".into()));
                            changed = true;
                        }
                    }
                    break;
                }
            }
        }
        changed
    }

    pub fn pending(&self) -> bool {
        self.cache.values().any(|lookup| matches!(lookup, Lookup::Mesh(Slot::Pending)))
    }

    pub fn materials(&self) -> impl Iterator<Item = u32> + '_ {
        self.cache.values().filter_map(|lookup| match lookup {
            Lookup::Mesh(Slot::Ready(geometry)) => Some(geometry),
            _ => None,
        }).flat_map(|geometry| geometry.ranges.iter().map(|range| range.material).filter(|material| *material != 0))
    }

    pub fn loaded_cells(&self) -> std::collections::BTreeSet<u32> {
        self.cache.iter().filter_map(|(key, lookup)| matches!(lookup, Lookup::Mesh(Slot::Ready(_))).then_some(key.cell)).collect()
    }
}

struct CellIndex {
    mount: usize,
    crc: u32,
    meshes: HashMap<u32, MeshEntry>,
}

struct CellSource {
    mounts: Vec<Mount>,
    cells: HashMap<u32, Result<Option<CellIndex>, String>>,
}

impl CellSource {
    fn open(game: &Path) -> Result<Self, String> {
        Ok(Self { mounts: cells::france_mounts(game)?, cells: HashMap::new() })
    }

    fn index(&self, cell: u32) -> Result<Option<CellIndex>, String> {
        let crc = cells::cell_crc(cell);
        let Some((winner, entry)) = cells::resolve_entry(&self.mounts, crc) else {
            return Ok(None);
        };
        let mount = self.mounts.iter().position(|mount| std::ptr::eq(mount, winner)).unwrap();
        let bytes = megapack::entry_slice(&self.mounts[mount].map, entry)?;
        let pack = sbla::parse(bytes, None)?;
        let mut meshes = HashMap::new();
        for (record, blob) in pack.records.iter().zip(&pack.blobs).filter(|(record, _)| record.kind == 0) {
            let mut header = mesh::parse_msha(blob)?;
            if pandemic_hash(&header.name) != record.hash {
                return Err(format!("cell {cell}: MSHA {} does not match record {:08x}", header.name, record.hash));
            }
            if header.c0 == 0 || header.unc0 == 0 {
                continue;
            }
            header.off = entry.offset as usize + blob.as_ptr() as usize - bytes.as_ptr() as usize;
            if meshes.insert(record.hash, MeshEntry::from(header)).is_some() {
                return Err(format!("cell {cell}: ambiguous type-0 hash {:08x}", record.hash));
            }
        }
        Ok(Some(CellIndex { mount, crc, meshes }))
    }

    fn load(&mut self, key: &Key) -> Result<Option<Geom>, String> {
        if !self.cells.contains_key(&key.cell) {
            let index = self.index(key.cell);
            self.cells.insert(key.cell, index);
        }
        let index = self.cells[&key.cell].as_ref().map_err(Clone::clone)?;
        let Some(index) = index else { return Ok(None) };
        let Some(entry) = index.meshes.get(&pandemic_hash(&key.class)) else { return Ok(None) };
        if !entry.name.eq_ignore_ascii_case(&key.class) {
            return Err(format!("cell {}: class hash collision for {}", key.cell, key.class));
        }
        let mount = &self.mounts[index.mount];
        let loaded = crate::meshload::load(&mount.map, entry)?;
        let flat = crate::place_mesh::flatten(&loaded.mesh, true);
        if flat.tris.is_empty() {
            return Err(format!("cell {}: {} has no intact triangles", key.cell, entry.name));
        }
        let provenance = format!("same-cell {} in {} entry {:08x}, cell {} | LOD{}, {} tris", entry.name, mount.rel, index.crc, key.cell, flat.lod, flat.tris.len());
        Ok(Some(flat.into_geom(provenance)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_case_folded_but_cell_and_install_scoped() {
        let key = Key::new(132609, "City1_FoundationA_X8Y4A");
        assert_eq!(key, Key::new(132609, "city1_foundationa_x8y4a"));
        assert_ne!(key, Key::new(132610, &key.class));
        assert_ne!(key.gpu_key(Path::new("one")), key.gpu_key(Path::new("two")));
        assert_ne!(key.gpu_key(Path::new("one")), Key::new(132610, &key.class).gpu_key(Path::new("one")));
        assert_eq!(pandemic_hash(&key.class), 0x5716_c601);
    }

    #[test]
    fn disconnected_worker_does_not_leave_pending_requests() {
        let (request, requests) = channel();
        let (replies, reply) = channel();
        let key = Key::new(1, "test");
        let mut cache = HashMap::new();
        cache.insert(key.clone(), Lookup::Mesh(Slot::Pending));
        let mut cells = CellMeshes { game: PathBuf::new(), cache, request, reply };
        drop(replies);
        drop(requests);
        assert!(cells.poll());
        assert!(!cells.pending());
        assert!(matches!(cells.get(key), Lookup::Mesh(Slot::None(_))));
        assert!(matches!(cells.get(Key::new(2, "new")), Lookup::Mesh(Slot::None(_))));
    }

    #[cfg(feature = "retail")]
    #[test]
    fn retail_effective_cell_buildings_decode_with_materials() {
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mut source = CellSource::open(&game).unwrap();
        let index = source.index(132609).unwrap().expect("cell 132609");
        assert!(source.mounts[index.mount].rel.eq_ignore_ascii_case(cells::DLC_MEGAPACK));
        let entries: Vec<String> = index.meshes.values().filter(|entry| entry.name.to_ascii_lowercase().starts_with("city1_")).map(|entry| entry.name.clone()).collect();
        assert!(entries.len() > 10, "{} city meshes", entries.len());
        let mut triangles = 0;
        for name in &entries {
            let geometry = source.load(&Key::new(132609, name)).unwrap().expect("same-cell mesh");
            triangles += geometry.tris.len();
            assert_eq!(geometry.tris.len(), geometry.uvs.len());
            assert_eq!(geometry.tris.len(), geometry.normals.len());
            assert!(geometry.tris.iter().flatten().flatten().all(|value| value.is_finite()));
            assert!(geometry.ranges.iter().any(|range| range.material != 0));
            assert!(geometry.provenance.contains(cells::DLC_MEGAPACK));
        }
        assert!(triangles > 1000, "{triangles} triangles");
        assert!(source.load(&Key::new(132609, "DefinitelyMissingMesh")).unwrap().is_none());
        println!("effective cell 132609: {} building meshes, {triangles} intact coarse triangles", entries.len());
    }

    #[cfg(feature = "retail")]
    #[test]
    #[ignore = "requires a local GPU adapter; renders retail geometry without a window"]
    fn retail_building_offscreen_gpu_smoke() {
        let game = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mut source = CellSource::open(&game).unwrap();
        let index = source.index(132609).unwrap().unwrap();
        let geometry = source.load(&Key::new(132609, "city1_baseb_x8y4b")).unwrap().expect("retail street building module");
        let mut low = glam::Vec3::splat(f32::INFINITY);
        let mut high = glam::Vec3::splat(f32::NEG_INFINITY);
        for point in geometry.tris.iter().flatten() {
            low = low.min(glam::Vec3::from(*point));
            high = high.max(glam::Vec3::from(*point));
        }
        let centre = (low + high) * 0.5;
        let radius = (high - low).length().max(1.0);
        let camera = glam::Mat4::perspective_rh(50f32.to_radians(), 1.0, 0.1, radius * 10.0) * glam::Mat4::look_at_rh(centre + glam::Vec3::new(-0.8, 0.6, -1.0) * radius, centre, glam::Vec3::Y);
        let materials = geometry.ranges.iter().map(|range| range.material).filter(|hash| *hash != 0).collect();
        let textures = crate::place_tex::resolve_cells(&game, &[132609].into_iter().collect(), &materials);
        println!("{}", textures.report);
        assert!(textures.is_useful(), "{}", textures.report);
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::PRIMARY, ..Default::default() });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: None, force_fallback_adapter: false,
        })).expect("GPU adapter required for this opt-in smoke test");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("building smoke test"), required_features: wgpu::Features::empty(), required_limits: wgpu::Limits::default(),
        }, None)).unwrap();
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut renderer = crate::place_gpu::SceneGpu::new(&device, &queue);
        let provenance = geometry.provenance.clone();
        let mut draw = crate::place_gpu::DrawList {
            size: [512, 512], view_proj: camera.to_cols_array_2d(),
            items: vec![crate::place_gpu::Item { key: "retail-building-smoke".into(), geom: Arc::new(geometry), model: glam::Mat4::IDENTITY.to_cols_array_2d(), colour: [1.0; 3] }],
            textures: Some(Arc::new(crate::place_tex::TextureSet::empty("pending"))),
        };
        renderer.render(&device, &queue, &draw);
        let pending = renderer.readback(&device, &queue);
        draw.textures = Some(Arc::new(textures));
        renderer.render(&device, &queue, &draw);
        let pixels = renderer.readback(&device, &queue);
        assert!(pixels != pending, "late texture bindings must replace the cached untextured draw");
        assert!(pollster::block_on(device.pop_error_scope()).is_none(), "GPU validation error");
        let saved_items = std::mem::take(&mut draw.items);
        renderer.render(&device, &queue, &draw);
        let background = renderer.readback(&device, &queue);
        draw.items = saved_items;
        let drawn = pixels.chunks_exact(4).zip(background.chunks_exact(4)).filter(|(pixel, sky)| pixel != sky).count();
        assert!(drawn > 1000 && drawn < 512 * 512, "{drawn} foreground pixels");
        if let Some(path) = std::env::var_os("SAB_WORKSHOP_RENDER_EVIDENCE") {
            let file = std::fs::File::create(path).unwrap();
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 512, 512);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&pixels).unwrap();
        }
        println!("GPU {:?}: {drawn} foreground pixels; {provenance}", adapter.get_info());
    }
}
