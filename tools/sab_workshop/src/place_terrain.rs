//! Terrain heights and mouse picking for the places page, from Codex's read-only `sab_terrain_query` index
//! (docs/formats/terrain_query_milestone.md). The index is loaded once, on demand, on a background thread (about 2-5 s and ~160 MB on
//! retail) and then only queried. Nothing here edits terrain, placements or the scene: it reports what the terrain index says.
//!
//! Semantics kept from the index: a terrain HOLE (no terrain triangle at that X/Z) is reported as a hole, never as invented ground. Holes are
//! common where buildings, roads or interiors stand (e.g. the La Villette butcher hall); the surface there is not terrain.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use sab_terrain_query::{Ray, RayHit, TerrainIndex};

/// Where the terrain index is.
pub enum TerrainState {
    Idle,
    Loading,
    Ready(Arc<TerrainIndex>),
    Failed(String),
}

/// The page's handle on the terrain index: start a background load, poll it each frame, query it once ready.
pub struct TerrainLink {
    state: TerrainState,
    rx: Option<Receiver<Result<TerrainIndex, String>>>,
    started: Option<std::time::Instant>,
    load_ms: Option<u128>,
}

impl Default for TerrainLink {
    fn default() -> Self {
        TerrainLink { state: TerrainState::Idle, rx: None, started: None, load_ms: None }
    }
}

impl TerrainLink {
    /// Load the base terrain of the install at `game` on a background thread. A second call while loading or ready does nothing.
    pub fn start_load(&mut self, game: PathBuf) {
        if matches!(self.state, TerrainState::Loading | TerrainState::Ready(_)) {
            return;
        }
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(TerrainIndex::load_base(&game));
        });
        self.rx = Some(rx);
        self.started = Some(std::time::Instant::now());
        self.state = TerrainState::Loading;
    }

    /// Install an already-built index (tests build small synthetic ones).
    #[cfg(test)]
    pub fn set_ready(&mut self, index: TerrainIndex) {
        self.state = TerrainState::Ready(Arc::new(index));
        self.rx = None;
    }

    /// Take the background result if it has arrived. Returns true when the state changed.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(r) => {
                self.load_ms = self.started.map(|t| t.elapsed().as_millis());
                self.state = match r {
                    Ok(i) => TerrainState::Ready(Arc::new(i)),
                    Err(e) => TerrainState::Failed(e),
                };
                self.rx = None;
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.state = TerrainState::Failed("the terrain loader thread ended without a result".into());
                self.rx = None;
                true
            }
        }
    }

    pub fn index(&self) -> Option<&TerrainIndex> {
        match &self.state {
            TerrainState::Ready(i) => Some(i),
            _ => None,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, TerrainState::Idle | TerrainState::Failed(_))
    }

    pub fn status_line(&self) -> String {
        match &self.state {
            TerrainState::Idle => "terrain index not loaded".into(),
            TerrainState::Loading => format!("loading the terrain index... {:.0} s", self.started.map_or(0.0, |t| t.elapsed().as_secs_f64())),
            TerrainState::Ready(i) => format!(
                "terrain index ready: {} triangles{}",
                i.triangle_count(),
                self.load_ms.map(|ms| format!(", loaded in {:.1} s", ms as f64 / 1000.0)).unwrap_or_default()
            ),
            TerrainState::Failed(e) => format!("terrain index failed to load: {e}"),
        }
    }
}

/// What the terrain index says under a point: every surface at its X/Z (sorted low to high), the highest one at or below the point,
/// and the lowest one above it. `surfaces` empty = a terrain hole there.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundReport {
    pub surfaces: Vec<f64>,
    pub below: Option<f64>,
    pub above: Option<f64>,
}

/// Tolerance when deciding whether a surface is below the point (metres): objects often sit exactly on the ground.
pub const ON_GROUND_TOLERANCE: f64 = 0.05;

pub fn ground_under(index: &TerrainIndex, p: [f32; 3]) -> Result<GroundReport, String> {
    let q = index.heights_at(p[0] as f64, p[2] as f64)?;
    let mut surfaces: Vec<f64> = q.value.iter().map(|h| h.height).collect();
    surfaces.sort_by(|a, b| a.total_cmp(b));
    surfaces.dedup_by(|a, b| (*a - *b).abs() < 1e-5);
    let y = p[1] as f64;
    let below = surfaces.iter().copied().filter(|h| *h <= y + ON_GROUND_TOLERANCE).last();
    let above = surfaces.iter().copied().find(|h| *h > y + ON_GROUND_TOLERANCE);
    Ok(GroundReport { surfaces, below, above })
}

/// One line for the inspector.
pub fn describe_ground(r: &GroundReport, p: [f32; 3]) -> String {
    if r.surfaces.is_empty() {
        return format!("terrain: hole at ({:.1}, {:.1}): no terrain triangle here (building, road or interior floor; not covered by the terrain index)", p[0], p[2]);
    }
    let y = p[1] as f64;
    let n = r.surfaces.len();
    match (r.below, r.above) {
        (Some(b), _) if (y - b).abs() <= ON_GROUND_TOLERANCE => format!("terrain: on the terrain (surface {b:.3} m; {n} surface(s) here)"),
        (Some(b), None) => format!("terrain: {:.2} m above the terrain (surface {b:.3} m; {n} surface(s) here)", y - b),
        (Some(b), Some(a)) => format!("terrain: {:.2} m above a surface at {b:.3} m, another surface above at {a:.3} m ({n} here)", y - b),
        (None, Some(a)) => format!("terrain: {:.2} m BELOW the terrain (lowest surface {a:.3} m; {n} surface(s) here)", a - y),
        (None, None) => "terrain: no surface found".into(),
    }
}

/// Nearest terrain hit along a ray (double-sided), within `max_metres`. `None` for a miss or an invalid ray.
pub fn pick(index: &TerrainIndex, origin: [f64; 3], direction: [f64; 3], max_metres: f64) -> Option<RayHit> {
    let ray = Ray::new(origin, direction, max_metres).ok()?;
    index.raycast(ray).value
}

#[cfg(test)]
mod tests {
    use super::*;
    use sab_terrain::{geometry::TerrainMesh, retail::ChunkSource};
    use sab_terrain_query::Builder;

    /// A 10 m x 10 m triangle at height `y` (the same construction as Codex's synthetic tests).
    fn floor(x: f32, y: f32, z: f32) -> TerrainMesh {
        TerrainMesh {
            name: "synthetic".into(),
            streams: vec![],
            vertices: vec![[x, y, z], [x + 10.0, y, z], [x, y, z + 10.0]],
            triangles: vec![[0, 1, 2]],
            bounds: [[x, y, z], [x + 10.0, y, z + 10.0]],
        }
    }

    fn index(meshes: &[TerrainMesh]) -> TerrainIndex {
        let mut b = Builder::default();
        for (i, m) in meshes.iter().enumerate() {
            b.add(&ChunkSource { pack: "synthetic".into(), cell: i as u32, crc: i as u32, offset: 0 }, m).unwrap();
        }
        b.finish()
    }

    /// Retail (needs `.sab-local.toml`): the index loaded through the page's own background loader reproduces Codex's known height and keeps the
    /// butcher-hall stand (776.78, -686.09) a hole; a downward pick at the known point lands on that height.
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; run with --ignored (loads the full terrain, a few seconds)"]
    fn retail_index_through_the_page_loader_keeps_known_heights_and_holes() {
        let game = sab_formats::game_paths::local_config_retail_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let mut l = TerrainLink::default();
        l.start_load(game);
        let t = std::time::Instant::now();
        while !l.poll() && t.elapsed().as_secs() < 300 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let idx = l.index().unwrap_or_else(|| panic!("{}", l.status_line()));
        assert_eq!(idx.triangle_count(), 2_254_289, "the triangle count Codex reported");
        let known = ground_under(idx, [18.75, 41.0, 46.875]).unwrap();
        assert!(known.surfaces.iter().any(|h| (h - 40.05082321166992).abs() < 1e-9), "Codex's known height: {known:?}");
        let hole = ground_under(idx, [776.78, 55.04, -686.09]).unwrap();
        assert!(hole.surfaces.is_empty(), "the butcher-hall stand is a terrain hole: {hole:?}");
        assert!(describe_ground(&hole, [776.78, 55.04, -686.09]).starts_with("terrain: hole"));
        let hit = pick(idx, [18.75, 100.0, 46.875], [0.0, -1.0, 0.0], 1000.0).expect("a downward pick hits");
        assert!((hit.position[1] - 40.05082321166992).abs() < 1e-6, "{hit:?}");
        eprintln!("{}", l.status_line());
    }

    #[test]
    fn ground_under_reports_on_above_below_and_two_surfaces() {
        let idx = index(&[floor(0.0, 50.0, 0.0), floor(0.0, 60.0, 0.0)]);
        let on = ground_under(&idx, [2.0, 50.0, 2.0]).unwrap();
        assert_eq!(on.surfaces, vec![50.0, 60.0]);
        assert_eq!((on.below, on.above), (Some(50.0), Some(60.0)));
        assert!(describe_ground(&on, [2.0, 50.0, 2.0]).contains("above a surface") || describe_ground(&on, [2.0, 50.0, 2.0]).contains("on the terrain"));
        let up = ground_under(&idx, [2.0, 64.0, 2.0]).unwrap();
        assert_eq!((up.below, up.above), (Some(60.0), None));
        assert!(describe_ground(&up, [2.0, 64.0, 2.0]).contains("4.00 m above the terrain"));
        let under = ground_under(&idx, [2.0, 40.0, 2.0]).unwrap();
        assert_eq!((under.below, under.above), (None, Some(50.0)));
        assert!(describe_ground(&under, [2.0, 40.0, 2.0]).contains("BELOW"));
    }

    #[test]
    fn a_hole_is_reported_as_a_hole_never_as_ground() {
        let idx = index(&[floor(0.0, 50.0, 0.0)]);
        let hole = ground_under(&idx, [100.0, 50.0, 100.0]).unwrap();
        assert!(hole.surfaces.is_empty() && hole.below.is_none() && hole.above.is_none());
        assert!(describe_ground(&hole, [100.0, 50.0, 100.0]).starts_with("terrain: hole"));
    }

    #[test]
    fn pick_finds_the_nearest_surface_along_a_ray_and_misses_holes() {
        let idx = index(&[floor(0.0, 50.0, 0.0), floor(0.0, 60.0, 0.0)]);
        let hit = pick(&idx, [2.0, 100.0, 2.0], [0.0, -1.0, 0.0], 1000.0).expect("a downward ray hits");
        assert!((hit.position[1] - 60.0).abs() < 1e-9 && (hit.distance - 40.0).abs() < 1e-9, "nearest surface first: {hit:?}");
        assert!(pick(&idx, [100.0, 100.0, 100.0], [0.0, -1.0, 0.0], 1000.0).is_none(), "a hole is a miss");
        assert!(pick(&idx, [2.0, 100.0, 2.0], [0.0, 0.0, 0.0], 1000.0).is_none(), "an invalid ray is a miss, not a panic");
        assert!(pick(&idx, [2.0, 100.0, 2.0], [0.0, -1.0, 0.0], 10.0).is_none(), "beyond the distance limit");
    }

    #[test]
    fn the_link_reports_its_state_and_ignores_a_second_load() {
        let mut l = TerrainLink::default();
        assert!(l.is_idle() && l.index().is_none() && l.status_line().contains("not loaded"));
        l.set_ready(index(&[floor(0.0, 50.0, 0.0)]));
        assert!(l.index().is_some() && l.status_line().contains("ready: 1 triangles"));
        l.start_load(PathBuf::from("does-not-matter"));
        assert!(l.index().is_some(), "a ready index is not replaced by a second load");
    }

    #[test]
    fn a_failed_background_load_is_reported_not_hidden() {
        let mut l = TerrainLink::default();
        l.start_load(std::env::temp_dir().join("no-saboteur-install-here-for-sure"));
        let t = std::time::Instant::now();
        while !l.poll() && t.elapsed().as_secs() < 30 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(matches!(l.state, TerrainState::Failed(_)), "{}", l.status_line());
        assert!(l.status_line().starts_with("terrain index failed to load"));
        assert!(l.is_idle(), "a failed load can be retried");
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------
// Terrain PREVIEW patch and sculpt brushes. PREVIEW ONLY: the offsets live in memory, are never saved, never exported and never reach the game.
// ---------------------------------------------------------------------------------------------------------------------------------------------

use crate::place_mesh::{Geom, GeomRange};

/// A square grid of terrain heights around a centre, sampled from the index, plus a sculpt offset per node.
pub struct Patch {
    pub cx: f32,
    pub cz: f32,
    pub step: f32,
    /// Nodes per side.
    pub n: usize,
    /// Base height per node (row-major, z rows), `None` where the index has no terrain (a hole): those nodes are never drawn or sculpted.
    pub base: Vec<Option<f32>>,
    /// Sculpt preview offset per node (metres).
    pub offset: Vec<f32>,
    /// Bumped on every change; part of the GPU cache key.
    pub rev: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brush {
    Raise,
    Lower,
    Smooth,
}

impl Patch {
    /// Sample a `radius`-metre square half-width of terrain around (cx, cz) every `step` metres. Where several surfaces share an X/Z (a bridge over a road,
    /// stacked floors) the one nearest `ref_y` is taken, so the patch follows the surface the user is looking at.
    pub fn sample(index: &TerrainIndex, cx: f32, cz: f32, radius: f32, step: f32, ref_y: f32) -> Result<Patch, String> {
        if !(step > 0.0 && radius >= step && radius.is_finite() && cx.is_finite() && cz.is_finite()) {
            return Err("terrain patch: radius and step must be positive and finite, radius >= step".into());
        }
        let half = (radius / step).floor() as i64;
        let n = (2 * half + 1) as usize;
        if n * n > 250_000 {
            return Err(format!("terrain patch: {n}x{n} nodes is more than the 250,000 limit; raise the step or lower the radius"));
        }
        let mut base = Vec::with_capacity(n * n);
        for iz in 0..n {
            for ix in 0..n {
                let x = cx + (ix as i64 - half) as f32 * step;
                let z = cz + (iz as i64 - half) as f32 * step;
                let h = index.heights_at(x as f64, z as f64)?.value.iter().map(|h| h.height).min_by(|a, b| (a - ref_y as f64).abs().total_cmp(&(b - ref_y as f64).abs())).map(|h| h as f32);
                base.push(h);
            }
        }
        Ok(Patch { cx, cz, step, n, offset: vec![0.0; n * n], base, rev: 0 })
    }

    fn half(&self) -> i64 {
        (self.n as i64 - 1) / 2
    }

    pub fn node_xz(&self, ix: usize, iz: usize) -> (f32, f32) {
        let h = self.half();
        (self.cx + (ix as i64 - h) as f32 * self.step, self.cz + (iz as i64 - h) as f32 * self.step)
    }

    /// Final height of a node (base + sculpt), `None` for a hole.
    pub fn height(&self, ix: usize, iz: usize) -> Option<f32> {
        self.base[iz * self.n + ix].map(|b| b + self.offset[iz * self.n + ix])
    }

    pub fn has_edits(&self) -> bool {
        self.offset.iter().any(|o| o.abs() > 1e-6)
    }

    pub fn reset(&mut self) {
        self.offset.iter_mut().for_each(|o| *o = 0.0);
        self.rev += 1;
    }

    /// Apply one brush dab centred at world (x, z): smooth cosine falloff to zero at `radius`. `strength` is the peak change in metres for Raise/Lower and the
    /// blend factor (0..1) for Smooth. Holes are untouched. Returns the number of nodes changed.
    pub fn dab(&mut self, brush: Brush, x: f32, z: f32, radius: f32, strength: f32) -> usize {
        if !(radius > 0.0 && radius.is_finite() && strength.is_finite() && x.is_finite() && z.is_finite()) {
            return 0;
        }
        let n = self.n;
        let mut delta = vec![0.0f32; n * n];
        let mut changed = 0usize;
        // smooth targets are computed from the state before the dab
        let cur: Vec<Option<f32>> = (0..n * n).map(|i| self.base[i].map(|b| b + self.offset[i])).collect();
        for iz in 0..n {
            for ix in 0..n {
                let i = iz * n + ix;
                if self.base[i].is_none() {
                    continue;
                }
                let (nx, nz) = self.node_xz(ix, iz);
                let d = ((nx - x).powi(2) + (nz - z).powi(2)).sqrt();
                if d >= radius {
                    continue;
                }
                let fall = 0.5 * (1.0 + (std::f32::consts::PI * d / radius).cos());
                let dv = match brush {
                    Brush::Raise => strength * fall,
                    Brush::Lower => -strength * fall,
                    Brush::Smooth => {
                        let mut sum = 0.0;
                        let mut cnt = 0;
                        for (dx, dz) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                            let (jx, jz) = (ix as i64 + dx, iz as i64 + dz);
                            if jx >= 0 && jz >= 0 && (jx as usize) < n && (jz as usize) < n {
                                if let Some(v) = cur[jz as usize * n + jx as usize] {
                                    sum += v;
                                    cnt += 1;
                                }
                            }
                        }
                        if cnt == 0 {
                            0.0
                        } else {
                            (sum / cnt as f32 - cur[i].unwrap()) * (strength.clamp(0.0, 1.0) * fall)
                        }
                    }
                };
                if dv != 0.0 {
                    delta[i] = dv;
                    changed += 1;
                }
            }
        }
        for (o, d) in self.offset.iter_mut().zip(&delta) {
            *o += d;
        }
        if changed > 0 {
            self.rev += 1;
        }
        changed
    }

    /// The patch as triangles (two per cell whose four corners all have terrain), with per-triangle normals. Holes leave gaps: no ground is invented there.
    pub fn geom(&self) -> Geom {
        let n = self.n;
        let mut tris: Vec<[[f32; 3]; 3]> = Vec::new();
        for iz in 0..n.saturating_sub(1) {
            for ix in 0..n.saturating_sub(1) {
                let corners = [(ix, iz), (ix + 1, iz), (ix, iz + 1), (ix + 1, iz + 1)];
                let mut p = [[0.0f32; 3]; 4];
                let mut ok = true;
                for (k, (jx, jz)) in corners.iter().enumerate() {
                    match self.height(*jx, *jz) {
                        Some(h) => {
                            let (x, z) = self.node_xz(*jx, *jz);
                            p[k] = [x, h, z];
                        }
                        None => ok = false,
                    }
                }
                if ok {
                    tris.push([p[0], p[2], p[1]]);
                    tris.push([p[1], p[2], p[3]]);
                }
            }
        }
        let normals: Vec<[f32; 3]> = tris
            .iter()
            .map(|t| {
                let (a, b) = ([t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]], [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]]);
                let c = [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
                let l = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt().max(1e-12);
                [c[0] / l, c[1] / l, c[2] / l]
            })
            .collect();
        let count = tris.len() as u32;
        Geom { uvs: vec![[[0.0; 2]; 3]; tris.len()], tris, normals, ranges: vec![GeomRange { first: 0, count, material: 0 }], provenance: format!("terrain preview patch ({n}x{n} nodes, {} m step, rev {})", self.step, self.rev) }
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;
    use sab_terrain::{geometry::TerrainMesh, retail::ChunkSource};
    use sab_terrain_query::Builder;

    /// A flat 2 km terrain at height 50 with a rectangular hole removed: two triangles per 100 m cell, cells overlapping the hole left out.
    fn flat_with_hole() -> TerrainIndex {
        let mut b = Builder::default();
        let mut id = 0;
        for gx in -10..10 {
            for gz in -10..10 {
                // a 100 m hole at x in [0, 100), z in [0, 100)
                if gx == 0 && gz == 0 {
                    continue;
                }
                let (x, z) = (gx as f32 * 100.0, gz as f32 * 100.0);
                let mesh = TerrainMesh {
                    name: "synthetic".into(),
                    streams: vec![],
                    vertices: vec![[x, 50.0, z], [x + 100.0, 50.0, z], [x + 100.0, 50.0, z + 100.0], [x, 50.0, z + 100.0]],
                    triangles: vec![[0, 1, 2], [0, 2, 3]],
                    bounds: [[x, 50.0, z], [x + 100.0, 50.0, z + 100.0]],
                };
                b.add(&ChunkSource { pack: "synthetic".into(), cell: id, crc: id, offset: 0 }, &mesh).unwrap();
                id += 1;
            }
        }
        b.finish()
    }

    #[test]
    fn a_patch_samples_the_index_and_leaves_holes_as_gaps() {
        let idx = flat_with_hole();
        let p = Patch::sample(&idx, 0.0, 0.0, 200.0, 10.0, 50.0).unwrap();
        assert_eq!(p.n, 41);
        assert_eq!(p.height(0, 0), Some(50.0), "the corner (-200,-200) is terrain");
        let centre = p.n / 2;
        assert_eq!(p.height(centre + 2, centre + 2), None, "(20,20) is inside the hole: no invented ground");
        let g = p.geom();
        assert!(!g.tris.is_empty());
        assert!(g.tris.iter().all(|t| t.iter().all(|v| (v[1] - 50.0).abs() < 1e-4)), "flat terrain gives flat triangles");
        // no triangle may cover the hole: every triangle's centroid is outside [0,100) x [0,100)
        assert!(g.tris.iter().all(|t| {
            let (cx, cz) = ((t[0][0] + t[1][0] + t[2][0]) / 3.0, (t[0][2] + t[1][2] + t[2][2]) / 3.0);
            !(cx > 0.0 && cx < 100.0 && cz > 0.0 && cz < 100.0)
        }), "the hole stays empty");
        assert!(g.normals.iter().all(|n| (n[1].abs() - 1.0).abs() < 1e-4), "flat ground has vertical normals");
    }

    #[test]
    fn raise_makes_a_smooth_hill_peaking_at_the_dab_and_zero_at_its_edge() {
        let idx = flat_with_hole();
        let mut p = Patch::sample(&idx, -500.0, -500.0, 100.0, 5.0, 50.0).unwrap();
        let c = p.n / 2;
        let changed = p.dab(Brush::Raise, -500.0, -500.0, 40.0, 12.0);
        assert!(changed > 0 && p.has_edits() && p.rev == 1);
        assert!((p.height(c, c).unwrap() - 62.0).abs() < 1e-4, "peak = base + strength");
        let at = |dx: usize| p.height(c + dx, c).unwrap();
        assert!(at(0) > at(2) && at(2) > at(5) && at(5) > at(7), "monotone falloff");
        assert!((at(8) - 50.0).abs() < 1e-4, "no change at the brush radius (40 m = 8 nodes)");
        assert!((p.height(0, 0).unwrap() - 50.0).abs() < 1e-6, "far nodes untouched");
        // the hill shows up in the mesh: some triangle normal is no longer vertical
        assert!(p.geom().normals.iter().any(|n| n[1] < 0.99 && n[1] > 0.0), "slopes exist");
    }

    #[test]
    fn lower_digs_smooth_flattens_and_reset_restores() {
        let idx = flat_with_hole();
        let mut p = Patch::sample(&idx, -500.0, -500.0, 100.0, 5.0, 50.0).unwrap();
        let c = p.n / 2;
        p.dab(Brush::Raise, -500.0, -500.0, 40.0, 12.0);
        let peak = p.height(c, c).unwrap();
        for _ in 0..6 {
            p.dab(Brush::Smooth, -500.0, -500.0, 40.0, 0.8);
        }
        assert!(p.height(c, c).unwrap() < peak - 1.0, "smoothing lowers the peak");
        p.dab(Brush::Lower, -450.0, -500.0, 20.0, 8.0);
        assert!(p.height(c + 10, c).unwrap() < 50.0 - 7.9, "lower digs below the base");
        let rev = p.rev;
        p.reset();
        assert!(!p.has_edits() && p.rev == rev + 1);
        assert!((0..p.n).all(|i| (0..p.n).all(|j| p.height(i, j) == Some(50.0))));
    }

    #[test]
    fn brushes_leave_holes_alone_and_reject_bad_input() {
        let idx = flat_with_hole();
        let mut p = Patch::sample(&idx, 0.0, 0.0, 100.0, 10.0, 50.0).unwrap();
        let centre = p.n / 2;
        p.dab(Brush::Raise, 20.0, 20.0, 60.0, 5.0);
        assert_eq!(p.height(centre + 2, centre + 2), None, "a hole node is not sculpted into existence");
        assert_eq!(p.dab(Brush::Raise, f32::NAN, 0.0, 10.0, 1.0), 0);
        assert_eq!(p.dab(Brush::Raise, 0.0, 0.0, -1.0, 1.0), 0);
        assert!(Patch::sample(&idx, 0.0, 0.0, 5.0, 10.0, 50.0).is_err(), "radius below step");
        assert!(Patch::sample(&idx, 0.0, 0.0, 100000.0, 1.0, 50.0).is_err(), "too many nodes");
    }

    #[test]
    fn multiple_surfaces_choose_the_one_nearest_the_reference_height() {
        let mut b = Builder::default();
        for (i, y) in [50.0f32, 70.0].iter().enumerate() {
            let mesh = TerrainMesh {
                name: "synthetic".into(),
                streams: vec![],
                vertices: vec![[0.0, *y, 0.0], [100.0, *y, 0.0], [100.0, *y, 100.0], [0.0, *y, 100.0]],
                triangles: vec![[0, 1, 2], [0, 2, 3]],
                bounds: [[0.0, *y, 0.0], [100.0, *y, 100.0]],
            };
            b.add(&ChunkSource { pack: "synthetic".into(), cell: i as u32, crc: i as u32, offset: 0 }, &mesh).unwrap();
        }
        let idx = b.finish();
        assert_eq!(Patch::sample(&idx, 50.0, 50.0, 20.0, 10.0, 52.0).unwrap().height(2, 2), Some(50.0));
        assert_eq!(Patch::sample(&idx, 50.0, 50.0, 20.0, 10.0, 68.0).unwrap().height(2, 2), Some(70.0));
    }
}
