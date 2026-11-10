//! Terrain-cell placement lists: the EditNode bodies a world streamblock's ALBS sub-pack carries in
//! its type-8 record (`docs/formats/sbla_subpack.md`; data survey in
//! `saboteur-camera-mod/hitman/TOWERS_AGENT_NOTES.md`).
//!
//! A type-8 blob is stored raw (`comp == uncomp`) and is one little-endian EditNode entry body:
//! `u32 0, u32 object_count, object_count × { u32 object_tag, u32 param_count, params }`, each param
//! `{ u32 tag, u32 byte_len, bytes }` ([`crate::editnodes::parse_entry`]). The type-4 record of the
//! same cell is a separate blob; in retail data it is commonly a byte-identical prefix of the type-8
//! blob, so an in-place edit inside that prefix must change both copies to keep them identical.
//!
//! Only same-size edits are offered: whether the engine accepts a resized type-8 blob is untested.

use crate::sbla::{self, TYPE_STREAMED};

/// The ALBS type that repeats a prefix of the placement list.
pub const TYPE_PREFIX: usize = 4;

/// One param's location: `data_off` is the first byte of its payload, relative to the list blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamLoc {
    pub tag: u32,
    pub data_off: usize,
    pub len: usize,
}

/// One object's location in the list blob, with its params in stored order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectLoc {
    pub tag: u32,
    pub off: usize,
    pub params: Vec<ParamLoc>,
}

/// Byte offsets of every object and param of one placement list. Framing is positional, as in the
/// engine's reader `FUN_00a005a0`: root count = objects, object count = params, param count = bytes.
pub fn index(blob: &[u8]) -> Result<Vec<ObjectLoc>, String> {
    let u32_at = |o: usize| -> Result<u32, String> {
        blob.get(o..o + 4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .ok_or_else(|| format!("placement list: EOF reading u32 at {o} of {}", blob.len()))
    };
    if u32_at(0)? != 0 {
        return Err("placement list: root word is not 0 (not an EditNode body)".into());
    }
    let count = u32_at(4)? as usize;
    let mut p = 8;
    let mut objects = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let off = p;
        let tag = u32_at(p)?;
        let n = u32_at(p + 4)? as usize;
        p += 8;
        let mut params = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            let ptag = u32_at(p)?;
            let len = u32_at(p + 4)? as usize;
            let data_off = p + 8;
            if data_off.checked_add(len).map_or(true, |e| e > blob.len()) {
                return Err(format!("placement list: param at {p} runs past the end"));
            }
            params.push(ParamLoc { tag: ptag, data_off, len });
            p = data_off + len;
        }
        objects.push(ObjectLoc { tag, off, params });
    }
    if p != blob.len() {
        return Err(format!("placement list: consumed {p} of {} bytes", blob.len()));
    }
    Ok(objects)
}

/// A blob's position inside the sub-pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobLoc {
    /// Index into the sub-pack's record table.
    pub record: usize,
    /// Absolute offset of the blob's first byte in the sub-pack.
    pub off: usize,
    pub comp: usize,
    pub uncomp: usize,
}

/// One type-8 placement list: its blob, the record's two unexplained words, and its objects.
#[derive(Clone, Debug)]
pub struct List {
    pub loc: BlobLoc,
    pub f10: u32,
    pub f14: u32,
    pub objects: Vec<ObjectLoc>,
}

/// A type-4 blob and the list it is a raw byte-identical prefix of, if any.
#[derive(Clone, Copy, Debug)]
pub struct Prefix {
    pub loc: BlobLoc,
    pub mirrors: Option<usize>,
}

/// Every placement list of one terrain-cell sub-pack (a cell can carry several type-8 records), and
/// every type-4 blob.
#[derive(Clone, Debug)]
pub struct Cell {
    pub lists: Vec<List>,
    pub prefixes: Vec<Prefix>,
}

/// Locate and index a sub-pack's placement lists. `Ok(None)` when it has no type-8 record; an error
/// when a list is not stored raw or does not parse exactly.
pub fn read(subpack: &[u8]) -> Result<Option<Cell>, String> {
    let sp = sbla::parse(subpack, None)?;
    let base = subpack.as_ptr() as usize;
    let loc = |i: usize| BlobLoc {
        record: i,
        off: sp.blobs[i].as_ptr() as usize - base,
        comp: sp.records[i].comp as usize,
        uncomp: sp.records[i].uncomp as usize,
    };
    let mut lists = Vec::new();
    for i in (0..sp.records.len()).filter(|&i| sp.records[i].kind == TYPE_STREAMED) {
        let l = loc(i);
        if l.comp != l.uncomp {
            return Err(format!("type-8 record {i}: comp {} != uncomp {} (not stored raw)", l.comp, l.uncomp));
        }
        let objects = index(&subpack[l.off..l.off + l.comp]).map_err(|e| format!("type-8 record {i}: {e}"))?;
        lists.push(List { loc: l, f10: sp.records[i].f10, f14: sp.records[i].f14, objects });
    }
    if lists.is_empty() {
        return Ok(None);
    }
    let prefixes = (0..sp.records.len())
        .filter(|&i| sp.records[i].kind == TYPE_PREFIX)
        .map(|i| {
            let b = loc(i);
            let raw = b.comp == b.uncomp;
            let bytes = &subpack[b.off..b.off + b.comp];
            let mirrors = lists
                .iter()
                .position(|l| raw && b.comp <= l.loc.comp && bytes == &subpack[l.loc.off..l.loc.off + b.comp]);
            Prefix { loc: b, mirrors }
        })
        .collect();
    Ok(Some(Cell { lists, prefixes }))
}

/// What [`patch_in_place`] changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patched {
    pub bytes: Vec<u8>,
    /// Absolute sub-pack offsets written (the list, then each mirrored type-4 blob).
    pub written_at: Vec<usize>,
}

/// Overwrite `new.len()` bytes of placement list `list` at list-relative `data_off`, only if they
/// equal `expected_old` and the lengths match. Every type-4 blob that mirrors that list and covers the
/// range gets the same bytes. A mirroring type-4 blob that covers only part of the range is an error:
/// the copies would diverge.
pub fn patch_in_place(subpack: &[u8], cell: &Cell, list: usize, data_off: usize, expected_old: &[u8], new: &[u8]) -> Result<Patched, String> {
    if expected_old.len() != new.len() {
        return Err(format!("size change {} -> {}: only same-size edits are supported", expected_old.len(), new.len()));
    }
    let l = cell.lists.get(list).ok_or_else(|| format!("no placement list {list}"))?;
    let end = data_off + new.len();
    if end > l.loc.comp {
        return Err("edit runs past the placement list".into());
    }
    let at = l.loc.off + data_off;
    if &subpack[at..at + new.len()] != expected_old {
        return Err("placement list: current bytes differ from the expected old bytes".into());
    }
    let mut out = subpack.to_vec();
    out[at..at + new.len()].copy_from_slice(new);
    let mut written_at = vec![at];
    for pf in cell.prefixes.iter().filter(|p| p.mirrors == Some(list)) {
        if data_off >= pf.loc.comp {
            continue;
        }
        if end > pf.loc.comp {
            return Err(format!("type-4 record {} covers only part of the edited range", pf.loc.record));
        }
        let p = pf.loc.off + data_off;
        out[p..p + new.len()].copy_from_slice(new);
        written_at.push(p);
    }
    Ok(Patched { bytes: out, written_at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sbla::{Blob, Descriptor};

    fn list_blob() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&0xAABB_CCDDu32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&crate::editnodes::tags::POSITION.to_le_bytes());
        b.extend_from_slice(&12u32.to_le_bytes());
        for v in [1.0f32, 2.0, 3.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }

    fn cell_with(list: &[u8], prefix: &[u8]) -> Vec<u8> {
        let mut counts = [0u32; 9];
        counts[TYPE_STREAMED] = 1;
        counts[TYPE_PREFIX] = 1;
        let tail = crate::map6::Tail { list1: vec![], list2: vec![], x: 0, counts, a: vec![], nb: 0, b: vec![], c: vec![] };
        let d = Descriptor { heightfield: None, tail };
        let blobs = [
            Blob { kind: TYPE_STREAMED, hash: 0, uncomp: list.len() as u32, f10: 0, f14: 0, data: list, empty_offset: 0 },
            Blob { kind: TYPE_PREFIX, hash: 1, uncomp: prefix.len() as u32, f10: 0, f14: 0, data: prefix, empty_offset: 0 },
        ];
        sbla::write(Some(&d), &blobs).unwrap()
    }

    #[test]
    fn index_finds_the_position_payload() {
        let objs = index(&list_blob()).unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].tag, 0xAABB_CCDD);
        assert_eq!(objs[0].params[0], ParamLoc { tag: crate::editnodes::tags::POSITION, data_off: 24, len: 12 });
        let mut short = list_blob();
        short.pop();
        assert!(index(&short).is_err());
    }

    #[test]
    fn patch_changes_only_the_range_in_the_list_and_its_identical_prefix() {
        let list = list_blob();
        let sub = cell_with(&list, &list);
        let cell = read(&sub).unwrap().unwrap();
        assert_eq!(cell.prefixes[0].mirrors, Some(0));
        let old: Vec<u8> = [1.0f32, 2.0, 3.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let new: Vec<u8> = [1.5f32, 2.0, 3.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let p = patch_in_place(&sub, &cell, 0, 24, &old, &new).unwrap();
        assert_eq!(p.written_at.len(), 2);
        let diff: Vec<usize> = (0..sub.len()).filter(|&i| sub[i] != p.bytes[i]).collect();
        assert!(diff.iter().all(|&i| p.written_at.iter().any(|&w| (w..w + 12).contains(&i))));
        let again = read(&p.bytes).unwrap().unwrap();
        assert_eq!(again.prefixes[0].mirrors, Some(0), "the copies stay identical");
        assert!(patch_in_place(&sub, &cell, 0, 24, &new, &old).is_err(), "old bytes checked");
        assert!(patch_in_place(&sub, &cell, 0, 24, &old, &new[..8]).is_err(), "size change refused");
        assert!(patch_in_place(&sub, &cell, 1, 24, &old, &new).is_err(), "no second list");
    }

    #[test]
    fn a_mirroring_prefix_covering_part_of_the_range_is_refused() {
        let list = list_blob();
        let sub = cell_with(&list, &list[..30]);
        let cell = read(&sub).unwrap().unwrap();
        assert_eq!(cell.prefixes[0].mirrors, Some(0));
        let old: Vec<u8> = [1.0f32, 2.0, 3.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert!(patch_in_place(&sub, &cell, 0, 24, &old, &old).is_err());
    }
}
