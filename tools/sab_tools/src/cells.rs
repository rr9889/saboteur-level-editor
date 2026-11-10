//! France terrain cells as the engine mounts them, and the placements their type-8 records hold.
//!
//! France pack mounting, read from `FUN_009f4320` (clean GOG exe, disassembly `0x9f49d0..0x9f4bba`):
//! `Mega.megapack`, else `Mega0.megapack`, priority 50; then `Mega1..3` at `(5 - i) * 10`, stopping at
//! the first missing file. Patch slot 0 is `patchmega.megapack`, else `patchmega0.megapack`, priority
//! 100050 (`0x186D2`); then `patchmega1..3` at `(10005 - i) * 10`, stopping at the first missing file;
//! no patch pack mounts when slot 0 is empty. A crc found in several mounted packs is taken from the
//! highest priority one: the same mounter (`FUN_00e34f70`) and resolver as the Global packs
//! (`docs/formats/megapack_write.md`), not re-traced for France. `DLC/01/dlc01mega0.megapack` is mounted
//! by `FUN_009906c0` at priority 100100 (`0x18704`), above every France patch slot
//! (`docs/level_editor/point7_resolution.md`). Loose cell files do not participate after the world
//! mount. Equal-priority ties below retain input order; the engine's qsort tie order is not proven.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use memmap2::Mmap;
use sab_formats::editnodes::tags;
use sab_formats::{cellplace, map6, megapack, pandemic_hash, sbla};

use crate::stage::map;

pub const DLC_MEGAPACK: &str = r"DLC\01\dlc01mega0.megapack";
pub const DLC_PRIORITY: u32 = 100_100;

pub fn vec3(d: &[u8]) -> Option<[f32; 3]> {
    (d.len() == 12).then(|| {
        let g = |o: usize| f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
        [g(0), g(4), g(8)]
    })
}

pub fn cstr(d: &[u8]) -> String {
    let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
    String::from_utf8_lossy(&d[..end]).into_owned()
}

pub struct Mount {
    pub rel: String,
    pub priority: Option<u32>,
    pub map: Mmap,
    pub pack: megapack::Megapack,
}

impl Mount {
    fn open(game: &Path, rel: &str, priority: Option<u32>) -> Result<Option<Mount>, String> {
        let p = game.join(rel);
        if !p.is_file() {
            return Ok(None);
        }
        let map = map(&p)?;
        let pack = megapack::parse(&map).map_err(|e| format!("{rel}: {e}"))?;
        Ok(Some(Mount { rel: rel.to_string(), priority, map, pack }))
    }

    pub fn find(&self, crc: u32) -> Option<&megapack::Entry> {
        self.pack.entries.iter().find(|e| e.crc == crc)
    }
}

/// Every France megapack the engine would mount, including DLC, highest priority first.
pub fn france_mounts(game: &Path) -> Result<Vec<Mount>, String> {
    let mut v = Vec::new();
    let slot0 = ["patchmega.megapack", "patchmega0.megapack"].iter().find(|n| game.join("France").join(n).is_file());
    if let Some(n) = slot0 {
        v.extend(Mount::open(game, &format!(r"France\{n}"), Some(100_050))?);
        for i in 1..=3u32 {
            match Mount::open(game, &format!(r"France\patchmega{i}.megapack"), Some((10_005 - i) * 10))? {
                Some(m) => v.push(m),
                None => break,
            }
        }
    }
    let base0 = ["Mega.megapack", "Mega0.megapack"].iter().find(|n| game.join("France").join(n).is_file());
    if let Some(n) = base0 {
        v.extend(Mount::open(game, &format!(r"France\{n}"), Some(50))?);
        for i in 1..=3u32 {
            match Mount::open(game, &format!(r"France\Mega{i}.megapack"), Some((5 - i) * 10))? {
                Some(m) => v.push(m),
                None => break,
            }
        }
    }
    v.extend(Mount::open(game, DLC_MEGAPACK, Some(DLC_PRIORITY))?);
    v.sort_by(|left, right| right.priority.cmp(&left.priority));
    Ok(v)
}

/// Resolve a CRC to the highest-priority mounted entry, regardless of mount order or entry index.
/// Equal priorities keep the first mount (and its first matching entry). `None` sorts below every
/// known priority; `france_mounts` supplies only known priorities.
pub fn resolve_entry(mounts: &[Mount], crc: u32) -> Option<(&Mount, &megapack::Entry)> {
    let mut winner: Option<(&Mount, &megapack::Entry)> = None;
    for mount in mounts {
        if let Some(entry) = mount.find(crc) {
            if winner.map_or(true, |(current, _)| mount.priority > current.priority) {
                winner = Some((mount, entry));
            }
        }
    }
    winner
}

/// The file name a new patch pack must take to mount: the first slot the engine would try and find
/// missing, given the files present.
/// This is slot availability, NOT permission to override a CRC. Callers must reject any holder
/// whose priority is unknown or at least this slot's priority, in particular DLC at 100100.
pub fn free_patch_slot(game: &Path) -> Result<(String, u32), String> {
    let fr = game.join("France");
    if !fr.join("patchmega.megapack").is_file() && !fr.join("patchmega0.megapack").is_file() {
        return Ok(("patchmega.megapack".into(), 100_050));
    }
    for i in 1..=3u32 {
        let n = format!("patchmega{i}.megapack");
        if !fr.join(&n).is_file() {
            return Ok((n, (10_005 - i) * 10));
        }
    }
    Err("all four France patch slots are taken".into())
}

pub fn cell_crc(id: u32) -> u32 {
    pandemic_hash(&format!("{}.pack", map6::streamblock_path("France", id)))
}

pub struct Placement {
    pub cell: u32,
    pub pack: String,
    pub tag: u32,
    pub list: usize,
    pub lists_in_cell: usize,
    pub list_f10: u32,
    /// `pandemic_hash` of the list's static layer tag; 0 = the base layer (`docs/formats/cell_placements.md`).
    pub list_f14: u32,
    pub nth_in_cell: usize,
    pub pos: [f32; 3],
    /// X/Y/Z axis leaves when all three are present (rows = axes), else None.
    pub axes: Option<[[f32; 3]; 3]>,
    pub type_name: String,
    pub class_name: String,
    pub params: usize,
    pub lua_params: usize,
    pub in_prefix: bool,
}

/// Every placement with a Position in the effective terrain cells, and per-tag occurrence counts.
/// Base France Mega entries with `index & 0xff == 1` (MAP6 terrain, not collection/zone tiles)
/// define candidate CRCs and cell identities; each CRC is scanned once
/// using its highest-priority bytes (including DLC/patch overrides, even with entry index zero).
/// Unrelated DLC/patch entries do not introduce cells. Counts include objects without a Position
/// and all layer tags, not just layers active at runtime (`docs/level_editor/point2_resolution.md`).
pub fn scan_cells(mounts: &[Mount]) -> Result<(Vec<Placement>, HashMap<u32, usize>), String> {
    let mut out = Vec::new();
    let mut counts: HashMap<u32, usize> = HashMap::new();
    let mut effective: HashMap<u32, (&Mount, &megapack::Entry)> = HashMap::new();
    for mount in mounts {
        for entry in &mount.pack.entries {
            let winner = effective.entry(entry.crc).or_insert((mount, entry));
            if mount.priority > winner.0.priority {
                *winner = (mount, entry);
            }
        }
    }
    let mut scanned = HashSet::new();
    for m in mounts.iter().filter(|m| matches!(m.rel.as_str(),
        r"France\Mega.megapack" | r"France\Mega0.megapack" | r"France\Mega1.megapack" |
        r"France\Mega2.megapack" | r"France\Mega3.megapack")) {
        for e in &m.pack.entries {
            if (e.index & 0xff) != 1 || e.crc != cell_crc(e.index) || !scanned.insert(e.crc) {
                continue;
            }
            let cell_id = e.index;
            let (m, e) = effective[&e.crc];
            let context = |err| format!("{} cell {cell_id} (crc {:08x}): {err}", m.rel, e.crc);
            let sub = megapack::entry_slice(&m.map, e).map_err(context)?;
            if sbla::descriptor_len(sub).map_err(context)? == 0 {
                continue;
            }
            let Some(cell) = cellplace::read(sub).map_err(context)? else {
                continue;
            };
            let mut seen: HashMap<u32, usize> = HashMap::new();
            for (li, l) in cell.lists.iter().enumerate() {
                let blob = &sub[l.loc.off..l.loc.off + l.loc.comp];
                let prefix_len = cell.prefixes.iter().filter(|p| p.mirrors == Some(li)).map(|p| p.loc.comp).max().unwrap_or(0);
                for o in &l.objects {
                    *counts.entry(o.tag).or_default() += 1;
                    let nth = seen.entry(o.tag).or_default();
                    let get = |t: u32| o.params.iter().find(|p| p.tag == t).map(|p| &blob[p.data_off..p.data_off + p.len]);
                    let Some(pos) = get(tags::POSITION).and_then(vec3) else { continue };
                    out.push(Placement {
                        cell: cell_id,
                        pack: m.rel.clone(),
                        tag: o.tag,
                        list: li,
                        lists_in_cell: cell.lists.len(),
                        list_f10: l.f10,
                        list_f14: l.f14,
                        nth_in_cell: *nth,
                        pos,
                        axes: match (get(tags::X_AXIS).and_then(vec3), get(tags::Y_AXIS).and_then(vec3), get(tags::Z_AXIS).and_then(vec3)) {
                            (Some(x), Some(y), Some(z)) => Some([x, y, z]),
                            _ => None,
                        },
                        type_name: get(tags::TYPE).map(cstr).unwrap_or_default(),
                        class_name: get(tags::CLASS_NAME).map(cstr).unwrap_or_default(),
                        params: o.params.len(),
                        lua_params: o.params.iter().filter(|p| p.tag == tags::LUA_PARAM).count(),
                        in_prefix: o.params.last().map_or(false, |p| p.data_off + p.len <= prefix_len),
                    });
                    *nth += 1;
                }
            }
        }
    }
    Ok((out, counts))
}

/// Edge length of a terrain-cell square on X/Z, metres (PROVEN on retail: all 7669 cells with placements, `tests/c2_cells_retail.rs`).
pub const CELL_SIZE: f32 = 60.0;

/// Index from world X/Z to the `France.map` tiles (terrain cells) covering that 60 m square.
///
/// Findings it encodes (asset bytes, `docs/formats/terrain_cell_mapping.md`): a cell's id is its tile's `hash`; the square is
/// `[min, min + 60)` on X and Z, min a multiple of 60; Y is not part of the box (0..0). Several tiles can share one square
/// (7,857 squares, up to 11 tiles; the extra tiles are other variants/layers of the same ground), so a position maps to a
/// SET of cells, never to one. Which of them an object belongs to is the object's own cell and is not derived from position.
#[derive(Debug, Default)]
pub struct CellGrid {
    squares: HashMap<(i32, i32), Vec<u32>>,
}

/// The square index of a world coordinate. Half-open: a point exactly on a shared edge belongs to the higher square.
pub fn square_of(x: f32, z: f32) -> (i32, i32) {
    ((x / CELL_SIZE).floor() as i32, (z / CELL_SIZE).floor() as i32)
}

/// What moving an object to a new position means for its owning cell.
#[derive(Debug, PartialEq, Eq)]
pub enum Relocation {
    /// The new position is inside the same square: the object stays in its cell (nothing to re-home).
    SameSquare,
    /// The new position is in another square holding these cells (sorted). The object's record must move to one of them;
    /// which one (which layer) is NOT determined by the data, so the caller must choose, never guess.
    NeedsRehome(Vec<u32>),
    /// The new position is in a square with no cell at all: no cell can own the object there.
    NoCellThere,
    /// The new position is not finite.
    InvalidPosition,
}

impl CellGrid {
    pub fn from_tiles(tiles: &[sab_formats::map6::Tile]) -> CellGrid {
        let mut squares: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for t in tiles {
            let (x, z) = (t.a[0].min(t.b[0]), t.a[2].min(t.b[2]));
            // the square index of the tile's minimum corner; nudged inward so float noise cannot select the neighbour
            squares.entry(square_of(x + 0.01, z + 0.01)).or_default().push(t.hash);
        }
        for v in squares.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        CellGrid { squares }
    }

    /// Every cell covering the world point (X/Z), sorted by id; empty when none does or the point is not finite.
    pub fn cells_at(&self, x: f32, z: f32) -> Vec<u32> {
        if !x.is_finite() || !z.is_finite() {
            return Vec::new();
        }
        self.squares.get(&square_of(x, z)).cloned().unwrap_or_default()
    }

    pub fn square_count(&self) -> usize {
        self.squares.len()
    }

    /// Classify moving an object (currently owned by `from_cell`, whose square is `from_square`) to `to`.
    pub fn relocation(&self, from_square: (i32, i32), to: [f32; 3]) -> Relocation {
        if !to[0].is_finite() || !to[2].is_finite() {
            return Relocation::InvalidPosition;
        }
        let sq = square_of(to[0], to[2]);
        if sq == from_square {
            return Relocation::SameSquare;
        }
        match self.squares.get(&sq) {
            Some(v) => Relocation::NeedsRehome(v.clone()),
            None => Relocation::NoCellThere,
        }
    }
}

#[cfg(test)]
mod grid_tests {
    use super::*;
    use sab_formats::map6::Tile;

    fn tile(hash: u32, x: f32, z: f32) -> Tile {
        Tile { hash, name: Vec::new(), a: [x, 0.0, z], b: [x + 60.0, 0.0, z + 60.0], u0: 0, sel: 1, tail: None }
    }

    fn grid() -> CellGrid {
        CellGrid::from_tiles(&[tile(10, 0.0, 0.0), tile(11, 0.0, 0.0), tile(20, 60.0, 0.0), tile(30, -60.0, -60.0)])
    }

    #[test]
    fn a_position_maps_to_every_cell_sharing_its_square() {
        let g = grid();
        assert_eq!(g.cells_at(30.0, 30.0), vec![10, 11], "two tiles share the square");
        assert_eq!(g.cells_at(61.0, 5.0), vec![20]);
        assert_eq!(g.cells_at(-1.0, -1.0), vec![30], "negative coordinates floor, not truncate");
        assert!(g.cells_at(500.0, 500.0).is_empty(), "outside every cell");
        assert!(g.cells_at(f32::NAN, 0.0).is_empty() && g.cells_at(0.0, f32::INFINITY).is_empty());
        assert_eq!(g.square_count(), 3);
    }

    #[test]
    fn boundaries_are_half_open_and_y_is_ignored() {
        let g = grid();
        assert_eq!(g.cells_at(0.0, 0.0), vec![10, 11], "the minimum corner is inside");
        assert_eq!(g.cells_at(59.999, 0.0), vec![10, 11]);
        assert_eq!(g.cells_at(60.0, 0.0), vec![20], "the shared edge belongs to the higher square");
        assert_eq!(square_of(-0.001, 0.0), (-1, 0));
    }

    #[test]
    fn relocation_distinguishes_same_square_rehome_nothing_and_invalid() {
        let g = grid();
        let from = square_of(30.0, 30.0);
        assert_eq!(g.relocation(from, [10.0, 999.0, 10.0]), Relocation::SameSquare, "height changes never re-home");
        assert_eq!(g.relocation(from, [70.0, 0.0, 10.0]), Relocation::NeedsRehome(vec![20]));
        assert_eq!(g.relocation(from, [900.0, 0.0, 10.0]), Relocation::NoCellThere);
        assert_eq!(g.relocation(from, [f32::NAN, 0.0, 0.0]), Relocation::InvalidPosition);
    }

    #[test]
    fn tile_ids_are_preserved_exactly_and_deduplicated() {
        let g = CellGrid::from_tiles(&[tile(7, 0.0, 0.0), tile(7, 0.0, 0.0), tile(u32::MAX, 0.0, 0.0)]);
        assert_eq!(g.cells_at(1.0, 1.0), vec![7, u32::MAX]);
    }
}
