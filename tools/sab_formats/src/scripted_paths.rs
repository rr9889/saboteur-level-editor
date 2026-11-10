//! `France.paths` — the named scripted-path (patrol route) resource, magic `6HTP`.
//!
//! Layout (little-endian), verified on the retail file by an independent parser and by this module's
//! byte-exact round-trip test: `"6HTP"`, `u32 record_count`, then per record
//! `u32 id, u32 node_count, node[node_count], u32 link_count, u32 links[link_count], u16 flags`
//! where a node is `3 x f32` world position followed by four `u32` fields of unknown meaning
//! (28 bytes). Retail has 2,992 records / 13,200 nodes, every link is the sentinel `0xFFFFFFFF`,
//! flags are 0 or 1, and the parse ends exactly at EOF. A route's connectivity is its ordered node
//! sequence; there are no cross-record links.
//!
//! A record's `id` is [`crate::pandemic_hash`] of the full Lua path string, e.g.
//! `Missions\act_1\connecttobar\main\PATH_StumbleAway` -> `0xb840a0e9` (62 retail Lua names match).
//!
//! Status: format and id rule CONFIRMED offline. That the running game picks up a modified/loose
//! `France.paths`, and that an NPC then follows an appended route, is NOT yet tested in game.
//! Research source: `research/collision-nav-20261106` (Track 3); this is an independent port.

use crate::pandemic_hash;

pub const MAGIC: &[u8; 4] = b"6HTP";
const NODE_BYTES: usize = 28;
/// Refuse absurd counts before allocating (retail max is far below this).
const MAX_NODES_PER_RECORD: usize = 100_000;
const MAX_LINKS_PER_RECORD: usize = 4096;

/// One path node: world position plus four fields whose meaning is unresolved (kept verbatim).
#[derive(Clone, Debug, PartialEq)]
pub struct PathNode {
    pub pos: [f32; 3],
    pub unknown: [u32; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub struct PathRecord {
    pub id: u32,
    pub nodes: Vec<PathNode>,
    pub links: Vec<u32>,
    pub flags: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScriptedPaths {
    pub records: Vec<PathRecord>,
}

fn u32_at(b: &[u8], o: usize, what: &str) -> Result<u32, String> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| format!("{what}: truncated at 0x{o:x}"))
}

impl ScriptedPaths {
    /// Strict parse: bad magic, overruns, implausible counts and trailing bytes are all errors.
    pub fn parse(b: &[u8]) -> Result<Self, String> {
        if b.get(..4) != Some(&MAGIC[..]) {
            return Err("not a 6HTP paths file (bad magic)".into());
        }
        let count = u32_at(b, 4, "record count")? as usize;
        // Every record is at least 8 + 4 + 2 bytes, so a larger count cannot be real.
        if count > b.len() / 14 {
            return Err(format!("implausible record count {count} for {} bytes", b.len()));
        }
        let mut o = 8usize;
        let mut records = Vec::with_capacity(count);
        for i in 0..count {
            let id = u32_at(b, o, "record id")?;
            let nc = u32_at(b, o + 4, "node count")? as usize;
            o += 8;
            if nc > MAX_NODES_PER_RECORD || o + nc * NODE_BYTES > b.len() {
                return Err(format!("record {i} (id {id:08x}): node count {nc} overruns the file"));
            }
            let mut nodes = Vec::with_capacity(nc);
            for _ in 0..nc {
                let f = |k: usize| f32::from_le_bytes(b[o + k..o + k + 4].try_into().unwrap());
                let u = |k: usize| u32::from_le_bytes(b[o + k..o + k + 4].try_into().unwrap());
                nodes.push(PathNode { pos: [f(0), f(4), f(8)], unknown: [u(12), u(16), u(20), u(24)] });
                o += NODE_BYTES;
            }
            let lc = u32_at(b, o, "link count")? as usize;
            o += 4;
            if lc > MAX_LINKS_PER_RECORD || o + lc * 4 + 2 > b.len() {
                return Err(format!("record {i} (id {id:08x}): link count {lc} overruns the file"));
            }
            let links = (0..lc).map(|k| u32::from_le_bytes(b[o + 4 * k..o + 4 * k + 4].try_into().unwrap())).collect();
            o += lc * 4;
            let flags = u16::from_le_bytes([b[o], b[o + 1]]);
            o += 2;
            records.push(PathRecord { id, nodes, links, flags });
        }
        if o != b.len() {
            return Err(format!("{} trailing byte(s) after the last record (0x{o:x} != 0x{:x})", b.len() - o, b.len()));
        }
        Ok(Self { records })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.records.len() as u32).to_le_bytes());
        for r in &self.records {
            out.extend_from_slice(&r.id.to_le_bytes());
            out.extend_from_slice(&(r.nodes.len() as u32).to_le_bytes());
            for n in &r.nodes {
                for v in n.pos {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                for v in n.unknown {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            out.extend_from_slice(&(r.links.len() as u32).to_le_bytes());
            for l in &r.links {
                out.extend_from_slice(&l.to_le_bytes());
            }
            out.extend_from_slice(&r.flags.to_le_bytes());
        }
        out
    }

    pub fn find(&self, id: u32) -> Option<&PathRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// The id the engine derives from a full Lua path string.
    pub fn id_for_name(full_lua_path: &str) -> u32 {
        pandemic_hash(full_lua_path)
    }

    /// Append a new named route cloned from `template_id` (its unknown node fields, link sentinel and flags)
    /// with the given world positions. Existing records are untouched. Refuses a duplicate id, an unknown
    /// template or fewer than two points.
    pub fn append_route(&mut self, full_lua_path: &str, template_id: u32, points: &[[f32; 3]]) -> Result<u32, String> {
        let id = Self::id_for_name(full_lua_path);
        if self.find(id).is_some() {
            return Err(format!("route id {id:08x} (\"{full_lua_path}\") already exists; refusing to overwrite"));
        }
        if points.len() < 2 {
            return Err("a route needs at least two points".into());
        }
        let template = self.find(template_id).ok_or_else(|| format!("template route {template_id:08x} not found"))?;
        let unknown = template.nodes.first().map(|n| n.unknown).ok_or("template route has no nodes")?;
        let rec = PathRecord {
            id,
            nodes: points.iter().map(|&pos| PathNode { pos, unknown }).collect(),
            links: template.links.clone(),
            flags: template.flags,
        };
        self.records.push(rec);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ScriptedPaths {
        let node = |x: f32| PathNode { pos: [x, 1.0, 2.0], unknown: [1, 2, 3, 4] };
        ScriptedPaths {
            records: vec![
                PathRecord { id: 7, nodes: vec![node(0.0), node(1.0)], links: vec![0xFFFF_FFFF], flags: 1 },
                PathRecord { id: 9, nodes: vec![node(5.0), node(6.0), node(7.0)], links: vec![0xFFFF_FFFF], flags: 0 },
            ],
        }
    }

    #[test]
    fn synthetic_round_trip_is_byte_exact() {
        let bytes = sample().encode();
        let back = ScriptedPaths::parse(&bytes).unwrap();
        assert_eq!(back, sample());
        assert_eq!(back.encode(), bytes);
    }

    #[test]
    fn rejects_bad_magic_truncation_and_trailing_bytes() {
        let good = sample().encode();
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(ScriptedPaths::parse(&bad).unwrap_err().contains("magic"));
        assert!(ScriptedPaths::parse(&good[..good.len() - 1]).is_err());
        let mut extra = good.clone();
        extra.push(0);
        assert!(ScriptedPaths::parse(&extra).unwrap_err().contains("trailing"));
        // A huge declared node count must not allocate or panic.
        let mut huge = good.clone();
        huge[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ScriptedPaths::parse(&huge).is_err());
    }

    #[test]
    fn append_leaves_existing_records_and_refuses_duplicates() {
        let mut p = sample();
        let before: Vec<_> = p.records.clone();
        let id = p.append_route(r"Missions\test\PATH_Quiet", 7, &[[0.0, 0.0, 0.0], [3.0, 0.0, 4.0]]).unwrap();
        assert_eq!(id, pandemic_hash(r"Missions\test\PATH_Quiet"));
        assert_eq!(&p.records[..2], &before[..]);
        assert_eq!(p.records[2].nodes.len(), 2);
        let again = p.append_route(r"Missions\test\PATH_Quiet", 7, &[[0.0; 3], [1.0; 3]]);
        assert!(again.unwrap_err().contains("already exists"));
        assert!(p.append_route("x", 999, &[[0.0; 3], [1.0; 3]]).is_err());
        assert!(p.append_route("y", 7, &[[0.0; 3]]).is_err());
        assert_eq!(ScriptedPaths::parse(&p.encode()).unwrap(), p);
    }

    /// Retail check, skipped when the game is not installed here.
    #[test]
    fn retail_france_paths_round_trips_byte_exact() {
        let path = r"C:\Saboteur\The Saboteur\France.paths";
        let Ok(bytes) = std::fs::read(path) else { return };
        let p = ScriptedPaths::parse(&bytes).expect("retail France.paths parses to exact EOF");
        assert_eq!(p.records.len(), 2992);
        assert_eq!(p.records.iter().map(|r| r.nodes.len()).sum::<usize>(), 13_200);
        assert!(p.records.iter().all(|r| r.links == [0xFFFF_FFFF]));
        assert_eq!(p.encode(), bytes, "re-encoding reproduces the retail file byte-for-byte");
        let stumble = p.find(ScriptedPaths::id_for_name(r"Missions\act_1\connecttobar\main\PATH_StumbleAway")).expect("named route");
        assert_eq!(stumble.id, 0xb840a0e9);
        assert_eq!(stumble.nodes.len(), 7);
        assert!((stumble.nodes[0].pos[0] - 2683.2788).abs() < 1e-3);
    }
}
