//! Line-of-sight tests against the room's triangles, so ordinary picking in the places view favours
//! objects the viewer can actually see.
//!
//! The room (`place_world`) is drawn by the GPU with a depth buffer, but the editable objects' markers
//! and the picking happen on the CPU over the finished image, so a wall never hides them there. This
//! index answers one question: is the straight segment from the eye to an object's origin blocked by
//! room geometry? Triangles are stored in world space, grouped per placed mesh with a bounding box so
//! most meshes are skipped without touching their triangles. Double-sided (the room's winding is not
//! relied on).

use crate::place_world::Object;

struct Piece {
    min: [f32; 3],
    max: [f32; 3],
    tris: Vec<[[f32; 3]; 3]>,
}

/// The room's triangles in world space.
pub struct Index {
    pieces: Vec<Piece>,
    tri_count: usize,
}

impl Index {
    pub fn build(objects: &[Object]) -> Index {
        let mut pieces = Vec::new();
        let mut tri_count = 0;
        for o in objects {
            for pc in &o.pieces {
                let m = pc.model;
                let w = |v: [f32; 3]| [0, 1, 2].map(|k| v[0] * m[0][k] + v[1] * m[1][k] + v[2] * m[2][k] + m[3][k]);
                let tris: Vec<[[f32; 3]; 3]> = pc.geom.tris.iter().map(|t| t.map(w)).collect();
                if tris.is_empty() {
                    continue;
                }
                let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
                for t in &tris {
                    for v in t {
                        for k in 0..3 {
                            min[k] = min[k].min(v[k]);
                            max[k] = max[k].max(v[k]);
                        }
                    }
                }
                tri_count += tris.len();
                pieces.push(Piece { min, max, tris });
            }
        }
        Index { pieces, tri_count }
    }

    /// A cheap identity of what was built, to notice when the room changes.
    pub fn signature(&self) -> (usize, usize) {
        (self.pieces.len(), self.tri_count)
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// Whether room geometry lies on the segment `from` → `to`, ignoring the last `tolerance`
    /// metres before `to` (an object's origin usually sits on the surface it stands on).
    pub fn blocked(&self, from: [f64; 3], to: [f64; 3], tolerance: f64) -> bool {
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if len <= tolerance {
            return false;
        }
        let dir = [d[0] / len, d[1] / len, d[2] / len];
        let max_t = (len - tolerance) as f32;
        let o = from.map(|v| v as f32);
        let dir = dir.map(|v| v as f32);
        for p in &self.pieces {
            if !ray_hits_box(o, dir, max_t, p.min, p.max) {
                continue;
            }
            if p.tris.iter().any(|t| ray_hits_tri(o, dir, max_t, t)) {
                return true;
            }
        }
        false
    }
}

/// Slab test: does the ray enter the box within `(0, max_t)`?
fn ray_hits_box(o: [f32; 3], d: [f32; 3], max_t: f32, min: [f32; 3], max: [f32; 3]) -> bool {
    let (mut t0, mut t1) = (0.0f32, max_t);
    for k in 0..3 {
        if d[k].abs() < 1e-12 {
            if o[k] < min[k] || o[k] > max[k] {
                return false;
            }
            continue;
        }
        let inv = 1.0 / d[k];
        let (a, b) = ((min[k] - o[k]) * inv, (max[k] - o[k]) * inv);
        t0 = t0.max(a.min(b));
        t1 = t1.min(a.max(b));
        if t0 > t1 {
            return false;
        }
    }
    true
}

/// Möller–Trumbore, double-sided; a hit must lie within `(1e-4, max_t)`.
fn ray_hits_tri(o: [f32; 3], d: [f32; 3], max_t: f32, t: &[[f32; 3]; 3]) -> bool {
    let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let cross = |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let (e1, e2) = (sub(t[1], t[0]), sub(t[2], t[0]));
    let p = cross(d, e2);
    let det = dot(e1, p);
    if det.abs() < 1e-9 {
        return false;
    }
    let inv = 1.0 / det;
    let s = sub(o, t[0]);
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return false;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    let dist = dot(e2, q) * inv;
    dist > 1e-4 && dist < max_t
}

#[cfg(all(test, feature = "retail"))]
mod retail_tests {
    use super::*;
    use std::path::Path;

    /// On the user's install: the Belle de Nuit tables and chairs (EditNodes container 0x90549f20) are
    /// all hidden from an eye 40 m above the club by its roof, and none is hidden from an eye 3 m
    /// above and beside each piece. Timing is printed for the whole check.
    #[test]
    fn the_club_roof_hides_its_furniture_from_above_and_nothing_hides_it_up_close() {
        let dir = sab_formats::game_paths::local_config_retail_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let objs = crate::place_world::load_at(&dir, [-68.31, 258.96, -771.30], crate::place_world::MARGIN).unwrap();
        let t = std::time::Instant::now();
        let index = Index::build(&objs);
        let built = t.elapsed();
        assert!(index.signature().1 > 10_000, "{:?}", index.signature());
        let s = sab_tools::session::Session::open(&dir).unwrap();
        let e = s.entry(0x9054_9f20).unwrap();
        let (mut above, mut close, mut n) = (0, 0, 0);
        let t = std::time::Instant::now();
        for o in e.objects() {
            let (Some(cn), Some(p)) = (o.class_name(), o.position()) else { continue };
            if !(cn.contains("Table") || cn.contains("hair")) || cn.contains("Lamp") || cn.contains("Candle") {
                continue;
            }
            n += 1;
            let p = p.map(|v| v as f64);
            if index.blocked([p[0], p[1] + 40.0, p[2]], p, 0.35) {
                above += 1;
            }
            if !index.blocked([p[0] + 1.5, p[1] + 3.0, p[2] + 1.5], p, 0.35) {
                close += 1;
            }
        }
        println!("index built in {built:?} ({:?} pieces/tris); {} rays in {:?}; hidden from above {above}/{n}; visible up close {close}/{n}", index.signature(), 2 * n, t.elapsed());
        assert!(n >= 15, "{n} pieces");
        assert_eq!(above, n, "the roof hides every piece from 40 m above");
        assert!(close + 2 >= n, "{close} of {n} visible from 3 m (a couple may have furniture beside them)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall(x: f32) -> Piece {
        // A 10 × 10 m wall in the plane x = `x`.
        let tris = vec![[[x, -5.0, -5.0], [x, 5.0, -5.0], [x, -5.0, 5.0]], [[x, 5.0, -5.0], [x, 5.0, 5.0], [x, -5.0, 5.0]]];
        Piece { min: [x, -5.0, -5.0], max: [x, 5.0, 5.0], tris }
    }

    fn idx() -> Index {
        Index { pieces: vec![wall(0.0)], tri_count: 2 }
    }

    #[test]
    fn a_wall_between_the_eye_and_the_object_blocks_the_line() {
        let i = idx();
        assert!(i.blocked([-3.0, 0.0, 0.0], [3.0, 0.0, 0.0], 0.35), "object behind the wall");
        assert!(i.blocked([3.0, 0.0, 1.0], [-3.0, 0.0, -1.0], 0.35), "double-sided: from the other side too");
        assert!(!i.blocked([-3.0, 0.0, 0.0], [-1.0, 0.0, 0.0], 0.35), "object in front of the wall");
        assert!(!i.blocked([-3.0, 0.0, 9.0], [3.0, 0.0, 9.0], 0.35), "a line passing beside the wall");
    }

    #[test]
    fn an_object_on_the_surface_it_stands_on_is_not_blocked_by_it() {
        let i = idx();
        assert!(!i.blocked([-3.0, 0.0, 0.0], [0.0, 0.0, 0.0], 0.35), "origin on the wall's plane");
        assert!(!i.blocked([-3.0, 0.0, 0.0], [0.2, 0.0, 0.0], 0.35), "origin just inside the surface");
        assert!(i.blocked([-3.0, 0.0, 0.0], [0.6, 0.0, 0.0], 0.35), "origin clearly behind it");
    }
}
