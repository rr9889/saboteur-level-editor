//! Read-only environment geometry for the places view: the France map objects (interiors such as
//! `hq_int`, the Belle de Nuit club) whose bounding box contains the view centre, drawn as the game
//! places them.
//!
//! A map object is a record of `France.map` (lists 2 and 3: hash, name, box `a`..`b`, the ALBS type
//! counts) and a `France\Mega*` megapack entry whose `index` is that hash (`sab_formats::map6`,
//! `sab_formats::sbla`). Its sub-pack holds MSHA meshes (type 0, one per name) and a type-8 placement
//! list in the EditNodes layout. A placement whose `ClassName` equals a mesh name of the same sub-pack,
//! ignoring ASCII case (the lists store `mn_belledenuit_floor_l` for the mesh
//! `MN_BelleDeNuit_Floor_L`), draws that mesh with `XAxis`/`YAxis`/`ZAxis` as the model matrix's
//! columns and `Position` as its translation. Placements naming anything else (lights, occluders,
//! templates, AI points) are counted as not drawn.
//!
//! Evidence for the transform (`docs/level_editor/release_v0_1.md`, milestone 2): the room meshes are
//! stored around a local origin (walls about ±15 m), the placements put floors at y = 260.0 and the
//! same list puts its small tables at y = 260.0, standing on them. INFERRED: the engine's own
//! placement code for these lists was not traced.
//!
//! Mount precedence follows `sab_tools::cells::france_mounts` (highest priority first); an entry also
//! present in `DLC\01\dlc01mega0.megapack` is resolved by CRC at its verified priority 100100.
//! DLC/01's own `FRANCE.map` records are not read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use sab_formats::editnodes::tags;
use sab_formats::{cellplace, map6, megapack, sbla};

use crate::place_mesh::Geom;

/// One placed environment mesh.
pub struct Piece {
    /// Unique per mesh record: `pack|entry crc|mesh name`.
    pub key: String,
    pub geom: Arc<Geom>,
    /// Column-major: XAxis, YAxis, ZAxis, Position.
    pub model: [[f32; 4]; 4],
}

/// One map object as loaded.
pub struct Object {
    pub name: String,
    pub hash: u32,
    pub pack: String,
    pub crc: u32,
    pub a: [f32; 3],
    pub b: [f32; 3],
    pub pieces: Vec<Piece>,
    /// Placements that name no mesh of the object (lights, occluders, AI points, templates).
    pub not_drawn: usize,
    pub notes: Vec<String>,
    /// The object's diffuse textures (empty when none resolved).
    pub tex: Arc<crate::place_tex::TextureSet>,
}

fn no_textures() -> Arc<crate::place_tex::TextureSet> {
    Arc::new(crate::place_tex::TextureSet::empty("textures: none resolved"))
}

impl Object {
    pub fn contains(&self, p: [f32; 3], margin: f32) -> bool {
        (0..3).all(|k| p[k] >= self.a[k].min(self.b[k]) - margin && p[k] <= self.a[k].max(self.b[k]) + margin)
    }

    /// One line of provenance.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "{} (map object {:08x}) from {} entry {:08x}: {} meshes placed, {} placements without a mesh",
            self.name,
            self.hash,
            self.pack,
            self.crc,
            self.pieces.len(),
            self.not_drawn
        );
        for n in &self.notes {
            s.push_str(&format!("; {n}"));
        }
        if !self.pieces.is_empty() {
            s.push_str(&format!("; {}", self.tex.report));
        }
        s
    }
}

/// Objects whose box is wider than this on any axis are not loaded: several map objects span much of
/// France (`ffs_France`, `noncinematic`, cinematic and lighting sets), and this view draws one room.
pub const MAX_EXTENT: f32 = 150.0;

/// The objects whose box (plus `margin`) contains `p` and spans at most [`MAX_EXTENT`], fully loaded.
pub fn load_at(game: &Path, p: [f32; 3], margin: f32) -> Result<Vec<Object>, String> {
    let lf_path = game.join(r"France\loosefiles_BinPC.pack");
    let lf = std::fs::read(&lf_path).map_err(|e| format!("{}: {e}", lf_path.display()))?;
    let es = sab_formats::loosefiles::entries(&lf)?;
    let fm = sab_formats::loosefiles::find(&es, "France.map").ok_or_else(|| format!("{}: no France.map entry", lf_path.display()))?;
    let rm = map6::parse_region_main(&lf[fm.data.clone()]).map_err(|e| format!("France.map: {e}"))?;
    let mounts = sab_tools::cells::france_mounts(game)?;
    let mut out = Vec::new();
    for rec in rm.list2.iter().chain(&rm.list3) {
        let inside = (0..3).all(|k| p[k] >= rec.a[k].min(rec.b[k]) - margin && p[k] <= rec.a[k].max(rec.b[k]) + margin);
        if !inside || (0..3).any(|k| (rec.a[k] - rec.b[k]).abs() > MAX_EXTENT) {
            continue;
        }
        let seed = mounts.iter().filter(|mount| mount.rel.contains(r"\Mega")).chain(mounts.iter()).find_map(|mount| mount.pack.entries.iter().find(|entry| entry.index == rec.hash));
        let winner = seed.and_then(|entry| sab_tools::cells::resolve_entry(&mounts, entry.crc));
        let Some((m, e)) = winner else {
            out.push(Object { name: rec.name(), hash: rec.hash, pack: "-".into(), crc: 0, a: rec.a, b: rec.b, pieces: Vec::new(), not_drawn: 0, notes: vec!["no mounted pack holds it".into()], tex: no_textures() });
            continue;
        };
        let mut notes = Vec::new();
        let others: Vec<&str> = mounts.iter().filter(|mount| mount.rel != m.rel && mount.find(e.crc).is_some()).map(|mount| mount.rel.as_str()).collect();
        if !others.is_empty() {
            notes.push(format!("also in {} (lower priority)", others.join(", ")));
        }
        let sub = megapack::entry_slice(&m.map, e)?;
        let mut obj = Object { name: rec.name(), hash: rec.hash, pack: m.rel.clone(), crc: e.crc, a: rec.a, b: rec.b, pieces: Vec::new(), not_drawn: 0, notes, tex: no_textures() };
        let sp = match sbla::parse(sub, Some(rec.tail.counts)) {
            Ok(sp) => sp,
            Err(err) => {
                obj.notes.push(format!("sub-pack does not parse: {err}"));
                out.push(obj);
                continue;
            }
        };
        let meshes: HashMap<String, crate::meshload::MeshEntry> =
            crate::meshload::list_meshes_in(sub, e.offset as usize).into_iter().map(|me| (me.name.to_ascii_lowercase(), me)).collect();
        let mut loaded: HashMap<String, Option<Arc<Geom>>> = HashMap::new();
        let base = sub.as_ptr() as usize;
        for (i, r) in sp.records.iter().enumerate().filter(|(_, r)| r.kind == sbla::TYPE_STREAMED) {
            let off = sp.blobs[i].as_ptr() as usize - base;
            if r.comp != r.uncomp {
                obj.notes.push(format!("type-8 record {i} is not stored raw"));
                continue;
            }
            let blob = &sub[off..off + r.comp as usize];
            let objs = match cellplace::index(blob) {
                Ok(o) => o,
                Err(err) => {
                    obj.notes.push(format!("type-8 record {i}: {err}"));
                    continue;
                }
            };
            for o in &objs {
                let get = |t: u32| o.params.iter().find(|q| q.tag == t).map(|q| &blob[q.data_off..q.data_off + q.len]);
                let class = get(tags::CLASS_NAME).map(sab_tools::cells::cstr).unwrap_or_default().to_ascii_lowercase();
                let (Some(me), Some(pos)) = (meshes.get(&class), get(tags::POSITION).and_then(sab_tools::cells::vec3)) else {
                    obj.not_drawn += 1;
                    continue;
                };
                let geom = loaded
                    .entry(class.clone())
                    .or_insert_with(|| {
                        let lm = crate::meshload::load(&m.map, me).ok()?;
                        let flat = crate::place_mesh::flatten(&lm.mesh, false);
                        (!flat.tris.is_empty()).then(|| {
                            let provenance = format!("{} in {} entry {:08x} ({}) · LOD{}, {} tris", me.name, m.rel, e.crc, rec.name(), flat.lod, flat.tris.len());
                            Arc::new(flat.into_geom(provenance))
                        })
                    })
                    .clone();
                let Some(geom) = geom else {
                    obj.not_drawn += 1;
                    continue;
                };
                let axis = |t: u32, d: [f32; 3]| get(t).and_then(sab_tools::cells::vec3).unwrap_or(d);
                let (x, y, z) = (axis(tags::X_AXIS, [1.0, 0.0, 0.0]), axis(tags::Y_AXIS, [0.0, 1.0, 0.0]), axis(tags::Z_AXIS, [0.0, 0.0, 1.0]));
                obj.pieces.push(Piece {
                    key: format!("{}|{:08x}|{}", m.rel, e.crc, me.name),
                    geom,
                    model: [[x[0], x[1], x[2], 0.0], [y[0], y[1], y[2], 0.0], [z[0], z[1], z[2], 0.0], [pos[0], pos[1], pos[2], 1.0]],
                });
            }
        }
        let mut materials = std::collections::BTreeSet::new();
        for pc in &obj.pieces {
            materials.extend(pc.geom.ranges.iter().map(|r| r.material).filter(|&h| h != 0));
        }
        if !materials.is_empty() {
            obj.tex = Arc::new(crate::place_tex::resolve(game, &[sub], &materials));
        }
        out.push(obj);
    }
    Ok(out)
}

/// The environment around the view, loaded on a worker thread.
pub enum World {
    Idle,
    Pending([f32; 3], Receiver<Result<Vec<Object>, String>>),
    Ready([f32; 3], Vec<Object>),
    Failed([f32; 3], String),
}

/// How far outside an object's box the view centre may be and still load it.
pub const MARGIN: f32 = 2.0;

impl World {
    /// Load around `p` unless the loaded set already answers for it (an object contains it, or the
    /// last load at about this spot found none).
    pub fn ensure(&mut self, game: &str, p: [f32; 3]) {
        let near = |q: [f32; 3]| (0..3).all(|k| (q[k] - p[k]).abs() < 20.0);
        let fresh = match self {
            World::Idle => true,
            World::Pending(..) => false,
            World::Ready(q, objs) => !(objs.iter().any(|o| o.contains(p, MARGIN)) || (objs.is_empty() && near(*q))),
            World::Failed(q, _) => !near(*q),
        };
        if fresh && !game.is_empty() {
            let (tx, rx) = channel();
            let g = PathBuf::from(game);
            std::thread::spawn(move || {
                let _ = tx.send(load_at(&g, p, MARGIN));
            });
            *self = World::Pending(p, rx);
        }
    }

    /// Collect a finished load. Returns true when one arrived.
    pub fn poll(&mut self) -> bool {
        if let World::Pending(p, rx) = self {
            if let Ok(r) = rx.try_recv() {
                let p = *p;
                *self = match r {
                    Ok(o) => World::Ready(p, o),
                    Err(e) => World::Failed(p, e),
                };
                return true;
            }
        }
        false
    }

    pub fn objects(&self) -> &[Object] {
        match self {
            World::Ready(_, o) => o,
            _ => &[],
        }
    }

    /// A short status for the view's status line.
    pub fn status(&self) -> String {
        match self {
            World::Idle => "room: off".into(),
            World::Pending(..) => "room: loading…".into(),
            World::Failed(_, e) => format!("room: not available: {e}"),
            World::Ready(_, o) if o.is_empty() => "room: no France map object contains the view centre".into(),
            World::Ready(_, o) => {
                let drawn: Vec<String> = o.iter().filter(|x| !x.pieces.is_empty()).map(Object::describe).collect();
                let empty = o.len() - drawn.len();
                format!("room: {}{}", drawn.join(" | "), if empty > 0 { format!(" | {empty} more object(s) here with no mesh placed") } else { String::new() })
            }
        }
    }
}

#[cfg(all(test, feature = "retail"))]
mod tests {
    use super::*;

    /// The Belle de Nuit room on the user's install: one map object, `hq_int`, from Mega0, whose
    /// placed floors lie at y = 260 around the club's tables.
    #[test]
    fn the_belle_de_nuit_room_loads_with_its_floor_and_walls() {
        let dir = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let objs = load_at(&dir, [-68.31, 258.96, -771.30], MARGIN).unwrap();
        for o in &objs {
            let mut names: Vec<&str> = o.pieces.iter().map(|p| p.key.rsplit('|').next().unwrap()).collect();
            names.sort();
            names.dedup();
            println!("{} a {:?} b {:?}: {}", o.describe(), o.a, o.b, names.join(", "));
        }
        let hq = objs.iter().find(|o| o.name == "hq_int").unwrap_or_else(|| panic!("{:?}", objs.iter().map(|o| o.describe()).collect::<Vec<_>>()));
        assert_eq!((hq.hash, hq.pack.as_str(), hq.crc), (0x470C_E9D6, r"France\Mega0.megapack", 0x731F_FC24));
        let named = |n: &str| hq.pieces.iter().filter(|p| p.key.ends_with(&format!("|{n}"))).collect::<Vec<_>>();
        let floors = named("MN_BelleDeNuit_Floor_L");
        assert_eq!(floors.len(), 1);
        assert_eq!(floors[0].model[3][1], 260.0);
        assert!(named("MN_BelleDeNuit_Wall01").len() >= 4, "the wall pieces are placed");
        assert!(hq.pieces.len() > 300, "{}", hq.describe());
    }

    /// Survey of the room's own meshes, to see whether damage states can overlap in it: per distinct
    /// mesh, how many bones and prims it has, its distinct LODs and parent bones, and whether a
    /// template of that name (case-insensitive) exists to supply damage parts. Prints the totals.
    #[test]
    fn survey_room_mesh_damage_structure() {
        let dir = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mounts = sab_tools::cells::france_mounts(&dir).unwrap();
        let m = mounts.iter().find(|m| m.rel.ends_with("Mega0.megapack")).unwrap();
        let e = m.pack.entries.iter().find(|e| e.crc == 0x731F_FC24).unwrap();
        let sub = megapack::entry_slice(&m.map, e).unwrap();
        let gt = crate::sources::retail_pc::load_gametemplates(&dir).unwrap();
        let by_name: HashMap<String, &sab_formats::gametemplates::Template> = gt.table.templates().map(|(_, t)| (t.name.to_ascii_lowercase(), t)).collect();
        let (mut meshes, mut multi_bone, mut multi_lod, mut with_template, mut with_damage_template) = (0, 0, 0, 0, 0);
        let mut multi_prim_parent = 0;
        let mut examples = Vec::new();
        for me in crate::meshload::list_meshes_in(sub, e.offset as usize) {
            let Ok(lm) = crate::meshload::load(&m.map, &me) else { continue };
            meshes += 1;
            let lods: std::collections::BTreeSet<u8> = lm.mesh.prims.iter().map(|p| p.lod).collect();
            let parents: std::collections::BTreeSet<u16> = lm.prim_parent_bone.iter().copied().collect();
            if lm.bones.len() > 1 {
                multi_bone += 1;
            }
            if lods.len() > 1 {
                multi_lod += 1;
            }
            if parents.len() > 1 {
                multi_prim_parent += 1;
            }
            if let Some(t) = by_name.get(&me.name.to_ascii_lowercase()) {
                with_template += 1;
                let has_parts = t.pairs.iter().any(|p| p.hash == 0x86de_6639);
                if has_parts {
                    with_damage_template += 1;
                }
                if examples.len() < 8 {
                    examples.push(format!("{} (template type {}, damage parts: {has_parts})", me.name, t.ttype));
                }
            }
        }
        println!(
            "hq_int meshes: {meshes}; >1 bone: {multi_bone}; >1 LOD: {multi_lod}; >1 prim parent bone: {multi_prim_parent}; with a same-named template: {with_template}; of those with damage parts: {with_damage_template}; e.g. {examples:?}"
        );
        assert!(meshes > 50, "{meshes} meshes");
    }

    /// Every material of the placed room pieces resolves, through `France.materials`, to a diffuse
    /// texture that was found (room sub-pack, `Dynamic0` or `Palettes0`) and decoded, and each placed
    /// range's UVs exist.
    #[test]
    fn the_rooms_materials_all_resolve_to_decoded_diffuse_textures() {
        let dir = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let objs = load_at(&dir, [-68.31, 258.96, -771.30], MARGIN).unwrap();
        let hq = objs.iter().find(|o| o.name == "hq_int").unwrap();
        println!("{}", hq.tex.report);
        let mut materials = std::collections::BTreeSet::new();
        for pc in &hq.pieces {
            assert_eq!(pc.geom.uvs.len(), pc.geom.tris.len(), "{}: one UV triple per triangle", pc.key);
            assert_eq!(pc.geom.ranges.iter().map(|r| r.count as usize).sum::<usize>(), pc.geom.tris.len(), "{}: ranges cover the triangles", pc.key);
            materials.extend(pc.geom.ranges.iter().map(|r| r.material));
        }
        assert!(hq.tex.is_useful(), "{}", hq.tex.report);
        // Materials whose WSAO record has an empty diffuse slot draw plain; show what slots they do have.
        let wsao = crate::wsao::Wsao::open(&dir.join("France.materials").to_string_lossy()).unwrap();
        let mut plain = Vec::new();
        for m in &materials {
            match hq.tex.mat_tex.get(m) {
                Some(h) => assert!(hq.tex.tex.contains_key(h), "diffuse {h:08x} of material {m:08x} was not found or decoded: {}", hq.tex.report),
                None => plain.push((*m, wsao.textures(*m))),
            }
        }
        println!("{} of {} materials have no diffuse slot; first few slot lists: {:?}", plain.len(), materials.len(), &plain[..plain.len().min(6)]);
        assert!(plain.len() * 5 <= materials.len(), "{} of {} materials without a diffuse", plain.len(), materials.len());
        let with_uvs = hq.pieces.iter().filter(|p| p.geom.uvs.iter().flatten().any(|uv| *uv != [0.0, 0.0])).count();
        assert!(with_uvs * 10 >= hq.pieces.len() * 9, "{with_uvs} of {} pieces have non-zero UVs", hq.pieces.len());
    }

    /// Where do the room's textures live? For every material hash of the room's meshes: does WSAO
    /// (`France.materials`) name a diffuse texture, and is that DTEX record in the room's own sub-pack,
    /// in `Global/Palettes0` or `Global/Dynamic0`, or nowhere we looked. Prints the counts and timings.
    #[test]
    fn survey_room_texture_sources() {
        let dir = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mounts = sab_tools::cells::france_mounts(&dir).unwrap();
        let m = mounts.iter().find(|m| m.rel.ends_with("Mega0.megapack")).unwrap();
        let e = m.pack.entries.iter().find(|e| e.crc == 0x731F_FC24).unwrap();
        let sub = megapack::entry_slice(&m.map, e).unwrap();
        let wsao = crate::wsao::Wsao::open(&dir.join("France.materials").to_string_lossy()).unwrap();
        let mut mats = std::collections::BTreeSet::new();
        let (mut meshes, mut uv_meshes, mut prims, mut prims_without_material) = (0, 0, 0, 0);
        for me in crate::meshload::list_meshes_in(sub, e.offset as usize) {
            let Ok(lm) = crate::meshload::load(&m.map, &me) else { continue };
            meshes += 1;
            if !lm.mesh.uvs.is_empty() && lm.mesh.uvs.len() == lm.mesh.positions.len() {
                uv_meshes += 1;
            }
            for p in &lm.mesh.prims {
                prims += 1;
                if p.material_hash == 0 {
                    prims_without_material += 1;
                } else {
                    mats.insert(p.material_hash);
                }
            }
        }
        let diffuse: std::collections::BTreeSet<u32> = mats.iter().filter_map(|&mh| wsao.textures(mh).and_then(|t| t.first().copied()).filter(|&h| h != 0)).collect();
        let resolved = mats.iter().filter(|&&mh| wsao.textures(mh).is_some()).count();
        println!("room meshes {meshes} ({uv_meshes} with per-vertex UVs), prims {prims} ({prims_without_material} without a material hash); distinct materials {}: WSAO names textures for {resolved}; distinct diffuse textures {}", mats.len(), diffuse.len());
        let t = std::time::Instant::now();
        let mut needed: std::collections::HashSet<u32> = diffuse.iter().copied().collect();
        let mut found = HashMap::new();
        crate::resolve::take_hashes_from_slices(&[sub], &mut needed, &mut found).unwrap();
        println!("in the room's own sub-pack: {} found, {} still needed ({:?})", found.len(), needed.len(), t.elapsed());
        for (name, rel) in [("Palettes0", "Global/Palettes0.megapack"), ("Dynamic0", "Global/Dynamic0.megapack")] {
            if needed.is_empty() {
                break;
            }
            let t = std::time::Instant::now();
            let mp = crate::pack::Megapack::open(&dir.join(rel).to_string_lossy()).unwrap();
            let before = needed.len();
            crate::resolve::take_hashes_from_packs(&[&mp], &mut needed, &mut found).unwrap();
            println!("{name}: {} of {before} found, {} still needed ({:?})", before - needed.len(), needed.len(), t.elapsed());
        }
        assert!(meshes > 50);
    }

    /// The height of the highest room triangle under `p` (a vertical ray from 1 m above), if any.
    fn floor_under(objs: &[Object], p: [f32; 3]) -> Option<f32> {
        let mut best: Option<f32> = None;
        for o in objs {
            for pc in &o.pieces {
                let m = pc.model;
                let w = |v: [f32; 3]| [0, 1, 2].map(|k| v[0] * m[0][k] + v[1] * m[1][k] + v[2] * m[2][k] + m[3][k]);
                for t in &pc.geom.tris {
                    let [a, b, c] = t.map(w);
                    let d = (b[0] - a[0]) * (c[2] - a[2]) - (c[0] - a[0]) * (b[2] - a[2]);
                    if d.abs() < 1e-9 {
                        continue;
                    }
                    let u = ((p[0] - a[0]) * (c[2] - a[2]) - (c[0] - a[0]) * (p[2] - a[2])) / d;
                    let v = ((b[0] - a[0]) * (p[2] - a[2]) - (p[0] - a[0]) * (b[2] - a[2])) / d;
                    if u < 0.0 || v < 0.0 || u + v > 1.0 {
                        continue;
                    }
                    let y = a[1] + u * (b[1] - a[1]) + v * (c[1] - a[1]);
                    if y <= p[1] + 1.0 && best.map_or(true, |bb| y > bb) {
                        best = Some(y);
                    }
                }
            }
        }
        best
    }

    /// Independent check: the EditNodes cinematic container's tables and chairs (another file,
    /// `EditNodes.pack`) stand on the room's placed floor: the highest room surface under each lies
    /// within a few centimetres of its Position.
    #[test]
    fn editnodes_furniture_stands_on_the_room_floor() {
        let dir = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let objs = load_at(&dir, [-68.31, 258.96, -771.30], MARGIN).unwrap();
        let s = sab_tools::session::Session::open(&dir).unwrap();
        let e = s.entry(0x9054_9f20).unwrap();
        let mut gaps = Vec::new();
        for o in e.objects() {
            let (Some(cn), Some(p)) = (o.class_name(), o.position()) else { continue };
            if !(cn.contains("Table") || cn.contains("chair") || cn.contains("Chair")) || cn.contains("Lamp") || cn.contains("Candle") {
                continue;
            }
            let f = floor_under(&objs, p);
            println!("{cn} at {p:?}: floor {f:?} gap {:?}", f.map(|f| p[1] - f));
            gaps.push(f.map(|f| p[1] - f));
        }
        assert!(gaps.len() >= 15, "{} pieces", gaps.len());
        let on = gaps.iter().filter(|g| g.map_or(false, |g| g.abs() < 0.05)).count();
        assert!(on + 1 >= gaps.len(), "{on} of {} stand within 5 cm of the floor: {gaps:?}", gaps.len());
    }
}
