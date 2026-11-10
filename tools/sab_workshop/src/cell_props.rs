//! Terrain-cell placements as read-only world context for the places page's 3D view: every placement
//! with a Position in the effective mounted terrain cells (`sab_tools::cells::scan_cells`), bucketed on a
//! 32 m X/Z grid for radius queries. Loaded once on a background thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

const BUCKET: f32 = 32.0;

#[derive(Clone, Debug, PartialEq)]
pub struct CellTarget {
    pub cell: u32,
    pub list: usize,
    pub tag: u32,
    pub class_name: String,
    pub pos: [f32; 3],
    pub layer: u32,
}

pub struct Prop {
    pub pos: [f32; 3],
    pub axes: Option<[[f32; 3]; 3]>,
    pub class_name: String,
    pub type_name: String,
    pub cell: u32,
    pub list: usize,
    pub tag: u32,
    /// `pandemic_hash` of the placement list's static layer tag; 0 = the base layer.
    pub layer: u32,
}

impl Prop {
    pub fn target(&self) -> CellTarget {
        CellTarget { cell: self.cell, list: self.list, tag: self.tag, class_name: self.class_name.clone(), pos: self.pos, layer: self.layer }
    }
}

pub struct CellProps {
    pub props: Vec<Prop>,
    grid: HashMap<(i32, i32), Vec<u32>>,
}

fn bucket(x: f32, z: f32) -> (i32, i32) {
    ((x / BUCKET).floor() as i32, (z / BUCKET).floor() as i32)
}

impl CellProps {
    pub fn from_props(props: Vec<Prop>) -> CellProps {
        let mut grid: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for (i, p) in props.iter().enumerate() {
            grid.entry(bucket(p.pos[0], p.pos[2])).or_default().push(i as u32);
        }
        CellProps { props, grid }
    }

    pub fn load(game: &std::path::Path) -> Result<CellProps, String> {
        let mounts = sab_tools::cells::france_mounts(game)?;
        let (all, _) = sab_tools::cells::scan_cells(&mounts)?;
        Ok(CellProps::from_props(
            all.into_iter().map(|p| Prop { pos: p.pos, axes: p.axes, class_name: p.class_name, type_name: p.type_name, cell: p.cell, list: p.list, tag: p.tag, layer: p.list_f14 }).collect(),
        ))
    }

    /// Indices of the props within `r` metres of `c`, nearest first, at most `max`.
    pub fn near(&self, c: [f32; 3], r: f32, max: usize) -> Vec<u32> {
        let (b0, b1) = (bucket(c[0] - r, c[2] - r), bucket(c[0] + r, c[2] + r));
        let r2 = r * r;
        let d2 = |p: [f32; 3]| (p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2) + (p[2] - c[2]).powi(2);
        let mut out: Vec<(f32, u32)> = Vec::new();
        for bx in b0.0..=b1.0 {
            for bz in b0.1..=b1.1 {
                for &i in self.grid.get(&(bx, bz)).map(Vec::as_slice).unwrap_or(&[]) {
                    let d = d2(self.props[i as usize].pos);
                    if d <= r2 {
                        out.push((d, i));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out.truncate(max);
        out.into_iter().map(|(_, i)| i).collect()
    }
}

/// The load state, polled each frame.
pub enum Loading {
    Idle,
    Pending(Receiver<Result<CellProps, String>>),
    Ready(CellProps),
    Failed(String),
}

impl Loading {
    pub fn start(game: PathBuf) -> Loading {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(CellProps::load(&game));
        });
        Loading::Pending(rx)
    }

    pub fn poll(&mut self) {
        if let Loading::Pending(rx) = self {
            match rx.try_recv() {
                Ok(Ok(p)) => *self = Loading::Ready(p),
                Ok(Err(e)) => *self = Loading::Failed(e),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => *self = Loading::Failed("loader stopped".into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(x: f32, z: f32) -> Prop {
        Prop { pos: [x, 0.0, z], axes: None, class_name: String::new(), type_name: String::new(), cell: 0, list: 0, tag: 0, layer: 0 }
    }

    #[test]
    fn target_preserves_exact_fields_and_owns_class_name() {
        let mut original = Prop {
            pos: [12.5, -3.25, 64.0], axes: None, class_name: "StreetLamp".into(), type_name: "Static".into(),
            cell: 0x12345678, list: 7, tag: 0x87654321, layer: 0xabcdef01,
        };
        let target = original.target();
        assert_eq!(target, CellTarget {
            cell: 0x12345678, list: 7, tag: 0x87654321, class_name: "StreetLamp".into(), pos: [12.5, -3.25, 64.0], layer: 0xabcdef01,
        });
        assert_eq!(target.clone(), target);
        assert!(format!("{target:?}").contains("CellTarget"));
        original.class_name.clear();
        original.pos = [0.0; 3];
        assert_eq!(target.class_name, "StreetLamp");
        assert_eq!(target.pos, [12.5, -3.25, 64.0]);
    }

    #[test]
    fn same_tag_in_different_lists_has_distinct_targets() {
        let first = Prop { cell: 42, list: 3, tag: 99, ..prop(1.0, 2.0) };
        let second = Prop { cell: 42, list: 4, tag: 99, ..prop(1.0, 2.0) };
        assert_ne!(first.target(), second.target());
    }

    #[test]
    fn same_tag_in_different_cells_has_distinct_targets() {
        let first = Prop { cell: 42, list: 3, tag: 99, ..prop(1.0, 2.0) };
        let second = Prop { cell: 43, list: 3, tag: 99, ..prop(1.0, 2.0) };
        assert_ne!(first.target(), second.target());
    }

    #[test]
    fn near_finds_props_across_bucket_edges_nearest_first() {
        let c = CellProps::from_props(vec![prop(0.0, 0.0), prop(31.9, 0.0), prop(-0.5, 0.0), prop(33.0, 0.0), prop(-40.0, -40.0)]);
        assert_eq!(c.near([0.0, 0.0, 0.0], 32.5, 10), vec![0, 2, 1]);
        assert_eq!(c.near([32.0, 0.0, 0.0], 1.5, 10), vec![1, 3]);
        assert_eq!(c.near([0.0, 0.0, 0.0], 100.0, 2), vec![0, 2]);
    }

    /// Measurement, not a pass/fail gate: how many terrain-cell props name an asset the mesh library can
    /// assemble, by Type, and the 120 m window with the most. Run with --ignored --nocapture.
    #[cfg(feature = "retail")]
    #[test]
    #[ignore = "needs the retail install named by .sab-local.toml; loads every terrain cell"]
    fn retail_how_many_cell_props_resolve_to_meshes() {
        let game = sab_formats::game_paths::local_config_retail_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_or_else(|e| panic!("{e}"));
        let cp = CellProps::load(&game).unwrap();
        let src = crate::testenv::retail();
        let names: std::collections::HashSet<String> =
            crate::assets::build(src).into_iter().filter(|a| a.category != crate::assets::WORLD).map(|a| a.name).collect();
        let mut by_type: HashMap<String, (usize, usize)> = HashMap::new();
        let mut cells: HashMap<(i32, i32), usize> = HashMap::new();
        for p in &cp.props {
            let hit = names.contains(&p.class_name);
            let e = by_type.entry(p.type_name.clone()).or_default();
            e.0 += 1;
            if hit {
                e.1 += 1;
                *cells.entry(((p.pos[0] / 120.0).floor() as i32, (p.pos[2] / 120.0).floor() as i32)).or_default() += 1;
            }
        }
        let mut v: Vec<_> = by_type.into_iter().collect();
        v.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        println!("props {} total", cp.props.len());
        for (t, (n, h)) in v.iter().take(25) {
            println!("  {t:<28} {n:>8} props, {h:>8} resolve");
        }
        let mut cls: HashMap<&str, usize> = HashMap::new();
        for p in cp.props.iter().filter(|p| p.type_name.is_empty()) {
            *cls.entry(p.class_name.as_str()).or_default() += 1;
        }
        let mut cv: Vec<_> = cls.into_iter().collect();
        cv.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let in_tpl = |c: &str| { use crate::source::GameSource as _; crate::testenv::retail().templates().find(c).is_some() };
        println!("empty-type props: {} distinct classes", cv.len());
        for (c, n) in cv.iter().take(12) {
            println!("  class {c:?} x{n} in_GameTemplates={} asset={}", in_tpl(c), names.contains(*c));
        }
        let in_t = cv.iter().filter(|(c, _)| in_tpl(c)).map(|(_, n)| n).sum::<usize>();
        println!("empty-type props whose class is a GameTemplates entry: {in_t}");
        let mut c: Vec<_> = cells.into_iter().collect();
        c.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for ((x, z), n) in c.iter().take(5) {
            println!("best window centre ({}, {}): {n} resolvable props", x * 120 + 60, z * 120 + 60);
        }
    }
}
