//! GameTemplates.wsd — The Saboteur's object-definition layer: a flat table of named templates
//! (disguise use-points, cars, weapons, props, lights, particles, humans…), each a bag of
//! `{property-hash → data}` pairs. Ground-truthed against `DLC/01/GameTemplates.wsd` and the full main
//! DB (embedded in `France/loosefiles_BinPC.pack`), and the loader `FUN_0162bfa0 @0x0162bfa0`. See
//! `docs/formats/gametemplates.md`.
//!
//! ```text
//! Header: magic, u32 entry_count
//! entry_count × Entry, each a Marker (12 bytes: size 8 + 8 zero) or a Template:
//!   u32 total_size (bytes after this field), u32 unk1=0, u32 unk2=1,
//!   u32 name_len, name+NUL, u32 type_len, type+NUL, u32 pair_count,
//!   pair_count × { u32 hash=pandemic_hash(prop), u32 data_size, data[data_size] }
//! ```
//!
//! Two revisions share the layout and differ in word order ([`ByteOrder`]): retail PC `AULB` is
//! little-endian; the 2008 Xbox 360 build's `GameTemplates.wsd` is `BLU2`, big-endian throughout, data
//! included. Its `LoadTemplates` (WildStar_p @0x823695e8, WildStar_d @0x82653858) reads `BLU2`, then
//! per entry the size, a word, and `unk2` blueprints of `{name, type, properties}`, so a Marker is an
//! entry of zero blueprints.
//!
//! A 4-byte `data` may itself be a `pandemic_hash` — a sibling-template ref, `"none"`, or a **texture**
//! (`data == pandemic_hash(textureName)`, the ALBS/WSTX key; see the format doc §Texture references).

/// The word order of a templates file, named by its magic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrder {
    /// `AULB`: retail PC.
    Little,
    /// `BLU2`: the 2008 Xbox 360 build.
    Big,
}

impl ByteOrder {
    pub const fn magic(self) -> &'static [u8; 4] {
        match self {
            ByteOrder::Little => b"AULB",
            ByteOrder::Big => b"BLU2",
        }
    }

    pub fn of_magic(m: &[u8]) -> Option<ByteOrder> {
        [ByteOrder::Little, ByteOrder::Big].into_iter().find(|o| o.magic().as_slice() == m)
    }

    pub fn u32(self, b: [u8; 4]) -> u32 {
        match self {
            ByteOrder::Little => u32::from_le_bytes(b),
            ByteOrder::Big => u32::from_be_bytes(b),
        }
    }

    pub fn bytes(self, v: u32) -> [u8; 4] {
        match self {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        }
    }

    /// A Marker entry: a size of 8 followed by eight zero bytes.
    fn marker(self) -> [u8; 12] {
        let mut m = [0u8; 12];
        m[..4].copy_from_slice(&self.bytes(8));
        m
    }
}

#[derive(Clone, Debug)]
pub struct Pair {
    pub hash: u32,
    pub data: Vec<u8>,
    /// The word order of the file the pair belongs to.
    pub order: ByteOrder,
}

impl Pair {
    /// If `data` is exactly 4 bytes, its value as a u32 in the file's word order (an int, f32 bits, or
    /// a pandemic_hash ref).
    pub fn as_u32(&self) -> Option<u32> {
        (self.data.len() == 4).then(|| self.order.u32(self.data.clone().try_into().unwrap()))
    }
}

#[derive(Clone, Debug)]
pub struct Template {
    pub unk1: u32,
    pub unk2: u32,
    pub name: String,
    pub ttype: String,
    pub pairs: Vec<Pair>,
    /// Bytes after the last pair that the stored `total_size` still covers. Retail has none (all
    /// 10,761 records measure exactly). A modded file can: on a camera-mod install one record stores
    /// 276 for 215 bytes of pairs, and the following records only parse when framed by the stored size
    /// (framed by the pairs instead, the next record reads as garbage). Kept verbatim and written back.
    pub tail: Vec<u8>,
    /// The word order of the file the template belongs to.
    pub order: ByteOrder,
}

impl Template {
    pub fn pair(&self, hash: u32) -> Option<&Pair> {
        self.pairs.iter().find(|p| p.hash == hash)
    }
    pub fn pair_mut(&mut self, hash: u32) -> Option<&mut Pair> {
        self.pairs.iter_mut().find(|p| p.hash == hash)
    }
    /// Set (or insert) a pair's data by property-name hash.
    pub fn set_pair(&mut self, hash: u32, data: Vec<u8>) {
        match self.pair_mut(hash) {
            Some(p) => p.data = data,
            None => self.pairs.push(Pair { hash, data, order: self.order }),
        }
    }
    fn body_size(&self) -> usize {
        let mut n = 4 + 4; // unk1 + unk2
        n += 4 + self.name.len() + 1;
        n += 4 + self.ttype.len() + 1;
        n += 4; // pair_count
        for p in &self.pairs {
            n += 4 + 4 + p.data.len();
        }
        n + self.tail.len()
    }
}

#[derive(Clone, Debug)]
pub enum Entry {
    Marker,
    Template(Template),
}

#[derive(Clone)]
pub struct GameTemplates {
    pub entries: Vec<Entry>,
    pub order: ByteOrder,
    /// The header's `entry_count` when it promises more entries than the file holds; written back
    /// unchanged. Retail files match exactly (`None`). A modded file can end cleanly on a record
    /// boundary short of its count (seen on a camera-mod install).
    pub declared_count: Option<u32>,
}

impl GameTemplates {
    /// Iterate templates (skipping markers) with their entry index.
    pub fn templates(&self) -> impl Iterator<Item = (usize, &Template)> {
        self.entries.iter().enumerate().filter_map(|(i, e)| match e {
            Entry::Template(t) => Some((i, t)),
            Entry::Marker => None,
        })
    }
    pub fn find(&self, name: &str) -> Option<(usize, &Template)> {
        self.templates().find(|(_, t)| t.name == name)
    }
    pub fn template_mut(&mut self, entry_index: usize) -> Option<&mut Template> {
        match self.entries.get_mut(entry_index) {
            Some(Entry::Template(t)) => Some(t),
            _ => None,
        }
    }

    pub fn parse(b: &[u8]) -> Result<(GameTemplates, usize), String> {
        let order = b
            .get(0..4)
            .and_then(ByteOrder::of_magic)
            .ok_or_else(|| format!("bad magic {:?}, expected AULB or BLU2", b.get(0..4)))?;
        let rd_u32 = |b: &[u8], o: usize| -> Result<u32, String> {
            b.get(o..o + 4).map(|s| order.u32([s[0], s[1], s[2], s[3]])).ok_or_else(|| format!("EOF u32 @{o}"))
        };
        let marker = order.marker();
        let count = rd_u32(b, 4)?;
        let mut o = 8;
        let mut entries = Vec::with_capacity(count as usize);
        let mut declared_count = None;
        for ti in 0..count {
            // A file that ends exactly on a record boundary before its count is read as it stands.
            if o == b.len() {
                declared_count = Some(count);
                break;
            }
            if b.get(o..o + 12) == Some(marker.as_slice()) {
                o += 12;
                entries.push(Entry::Marker);
                continue;
            }
            let total_size = rd_u32(b, o)?;
            let body_start = o + 4;
            let unk1 = rd_u32(b, body_start)?;
            let unk2 = rd_u32(b, body_start + 4)?;
            let mut p = body_start + 8;
            // A stored string is its length (counting the terminator), its bytes and a NUL; the writer
            // always emits the NUL, so a missing one would not survive a rewrite and is refused.
            let text = |p: usize, what: &str| -> Result<String, String> {
                let len = rd_u32(b, p)? as usize;
                let s = p + 4;
                let bytes = s.checked_add(len).and_then(|e| b.get(s..e)).ok_or_else(|| format!("template {ti} @{o}: {what} length {len} runs past the end"))?;
                match bytes.split_last() {
                    Some((0, body)) => String::from_utf8(body.to_vec()).map_err(|_| format!("template {ti} @{o}: {what} is not valid UTF-8 (it would not survive a rewrite)")),
                    Some(_) => Err(format!("template {ti} @{o}: {what} of length {len} is not NUL-terminated")),
                    None => Err(format!("template {ti} @{o}: {what} length 0 (no terminator)")),
                }
            };
            let nlen = rd_u32(b, p)? as usize;
            let name = text(p, "name")?;
            p += 4 + nlen;
            let tlen = rd_u32(b, p)? as usize;
            let ttype = text(p, "type").map_err(|e| format!("{e} ({name:?})"))?;
            p += 4 + tlen;
            let pcount = rd_u32(b, p)?;
            p += 4;
            let mut pairs = Vec::with_capacity(pcount as usize);
            for _ in 0..pcount {
                let hash = rd_u32(b, p)?;
                let dsize = rd_u32(b, p + 4)? as usize;
                let data = b.get(p + 8..p + 8 + dsize).ok_or_else(|| format!("template {ti} ({name:?}) @{o}: pair size {dsize} runs past the end"))?.to_vec();
                p += 8 + dsize;
                pairs.push(Pair { hash, data, order });
            }
            // Records are framed by the stored size (see `Template::tail`); pairs that run past it
            // are a structural error.
            let measured = (p - body_start) as u32;
            if measured > total_size {
                return Err(format!("template {ti} ({name:?}): total_size={total_size} but the pairs run to {measured}"));
            }
            let end = body_start + total_size as usize;
            let tail = b.get(p..end).ok_or_else(|| format!("template {ti} ({name:?}): total_size runs past the end"))?.to_vec();
            o = end;
            entries.push(Entry::Template(Template { unk1, unk2, name, ttype, pairs, tail, order }));
        }
        Ok((GameTemplates { entries, order, declared_count }, o))
    }

    /// The file bytes, in [`GameTemplates::order`].
    pub fn write(&self) -> Vec<u8> {
        let w = |v: u32| self.order.bytes(v);
        let mut out = Vec::new();
        out.extend_from_slice(self.order.magic());
        out.extend_from_slice(&w(self.declared_count.unwrap_or(self.entries.len() as u32)));
        for e in &self.entries {
            match e {
                Entry::Marker => out.extend_from_slice(&self.order.marker()),
                Entry::Template(t) => {
                    out.extend_from_slice(&w(t.body_size() as u32));
                    out.extend_from_slice(&w(t.unk1));
                    out.extend_from_slice(&w(t.unk2));
                    out.extend_from_slice(&w((t.name.len() + 1) as u32));
                    out.extend_from_slice(t.name.as_bytes());
                    out.push(0);
                    out.extend_from_slice(&w((t.ttype.len() + 1) as u32));
                    out.extend_from_slice(t.ttype.as_bytes());
                    out.push(0);
                    out.extend_from_slice(&w(t.pairs.len() as u32));
                    for p in &t.pairs {
                        out.extend_from_slice(&w(p.hash));
                        out.extend_from_slice(&w(p.data.len() as u32));
                        out.extend_from_slice(&p.data);
                    }
                    out.extend_from_slice(&t.tail);
                }
            }
        }
        out
    }
}

#[path = "gametemplates_props.rs"]
mod props;

/// What a property's bytes mean. Derived, not declared: the engine's per-class `SetProperty`
/// dispatchers reveal which reader helper consumes each key, and the type is only recorded when
/// that reader agrees with the size histogram observed across all 10,761 retail templates.
///
/// This is for DISPLAY and authoring assistance only, never for framing. `size` in the pair header
/// is always authoritative — see `docs/formats/editnodes.md`'s account of a value-based classifier
/// that became a live parse bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PropType {
    /// one byte, usually a flag (often packed into a bitfield by the engine)
    Bool,
    /// four bytes read plainly — an int or a float, and the corpus cannot always say which
    Scalar4,
    /// four bytes read byte-reversing: a `pandemic_hash` naming another blueprint, a texture or a
    /// string. `"none"` (`0x4FF9F863`) is the null sentinel.
    Ref,
    /// twelve bytes, three f32
    Vec3,
    /// NUL-terminated text
    Str,
    /// anything else, including keys whose width varies between templates
    Bytes,
}

/// The property name for a key, if one is known ([`crate::names`]).
pub fn known_property_name(hash: u32) -> Option<&'static str> {
    crate::names::name(hash)
}

/// The derived value type for a key. `None` for a key that does not occur in the retail table.
pub fn property_type(hash: u32) -> Option<PropType> {
    props::TYPES
        .binary_search_by_key(&hash, |&(k, _)| k)
        .ok()
        .map(|i| props::TYPES[i].1)
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    fn db(order: ByteOrder) -> GameTemplates {
        let t = Template {
            unk1: 0,
            unk2: 1,
            name: "FBS_Test".into(),
            ttype: "FxHumanBodySetup".into(),
            pairs: vec![Pair { hash: 0x7969_1A2F, data: order.bytes(0xD15A_163C).to_vec(), order }],
            tail: Vec::new(),
            order,
        };
        GameTemplates { entries: vec![Entry::Marker, Entry::Template(t)], order, declared_count: None }
    }

    /// Both revisions write and re-read the same entries, and a 4-byte value reads back in the
    /// file's own word order.
    #[test]
    fn both_word_orders_round_trip() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let bytes = db(order).write();
            assert_eq!(&bytes[..4], order.magic());
            let (back, used) = GameTemplates::parse(&bytes).unwrap();
            assert_eq!((used, back.order), (bytes.len(), order));
            assert!(matches!(back.entries[0], Entry::Marker));
            let (_, t) = back.find("FBS_Test").unwrap();
            assert_eq!(t.pair(0x7969_1A2F).and_then(Pair::as_u32), Some(0xD15A_163C));
            assert_eq!(back.write(), bytes);
        }
        let be = db(ByteOrder::Big).write();
        assert_eq!(&be[..12], b"BLU2\x00\x00\x00\x02\x00\x00\x00\x08");
    }

    /// The two irregularities seen in a modded file: a record whose stored size covers bytes past its
    /// pairs (framing follows the size; the bytes are kept), and a header count larger than the
    /// records present with the file ending on a record boundary. Both round-trip byte for byte;
    /// pairs overrunning the size stay an error.
    #[test]
    fn a_tail_past_the_pairs_and_a_short_file_round_trip() {
        let order = ByteOrder::Little;
        let mut bytes = db(order).write();
        let size_at = 8 + 12;
        let size = u32::from_le_bytes(bytes[size_at..size_at + 4].try_into().unwrap());
        bytes[size_at..size_at + 4].copy_from_slice(&(size + 5).to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4, 5]);
        bytes.extend_from_slice(&order.marker());
        bytes[4..8].copy_from_slice(&5u32.to_le_bytes());
        let (gt, used) = GameTemplates::parse(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert!(matches!(gt.entries[2], Entry::Marker), "the next record starts where total_size says");
        assert_eq!(gt.find("FBS_Test").unwrap().1.tail, vec![1, 2, 3, 4, 5]);
        assert_eq!((gt.entries.len(), gt.declared_count), (3, Some(5)), "three records present, five declared");
        assert_eq!(gt.write(), bytes, "byte-identical round trip");

        let mut short = db(order).write();
        short[size_at..size_at + 4].copy_from_slice(&(size - 1).to_le_bytes());
        assert!(GameTemplates::parse(&short).is_err(), "pairs overrunning total_size are refused");
    }

    /// The relaxed framing (`tail`, `declared_count`) accepts only a file that ends on a record
    /// boundary; each of these damaged files is refused with an error naming the problem.
    #[test]
    fn damaged_records_are_refused() {
        let order = ByteOrder::Little;
        let good = db(order).write();
        // Layout of `db`: header 8, marker 12, then the record: total_size @20, unk1 @24, unk2 @28,
        // name length @32, "FBS_Test\0" @36..45, type length @45, "FxHumanBodySetup\0" @49..66,
        // pair count @66, pair hash @70, pair size @74, pair data @78..82.
        let refused = |bytes: &[u8], want: &str| {
            let err = GameTemplates::parse(bytes).err().unwrap_or_else(|| panic!("accepted a file that should be refused ({want})"));
            assert!(err.contains(want), "error {err:?} should mention {want:?}");
        };
        let set = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());

        // A record cut mid-pair: inside the pair data, and inside the pair header.
        refused(&good[..80], "pair size 4 runs past the end");
        refused(&good[..76], "EOF u32");

        // A name or type length past the end of the file, a zero length, a missing terminator.
        let mut b = good.clone();
        set(&mut b, 32, 1_000_000);
        refused(&b, "name length 1000000 runs past the end");
        let mut b = good.clone();
        set(&mut b, 45, 1_000_000);
        refused(&b, "type length 1000000 runs past the end");
        let mut b = good.clone();
        set(&mut b, 32, 0);
        refused(&b, "name length 0");
        let mut b = good.clone();
        b[44] = b'x';
        refused(&b, "name of length 9 is not NUL-terminated");
        let mut b = good.clone();
        set(&mut b, 32, 20);
        refused(&b, "is not NUL-terminated");

        // Pairs that overrun total_size: by a declared pair size, and by an extra pair count.
        let mut b = good.clone();
        set(&mut b, 74, 8);
        b.extend_from_slice(&[0; 4]);
        refused(&b, "but the pairs run to");
        let mut b = good.clone();
        set(&mut b, 66, 2);
        b.extend_from_slice(&[0; 8]);
        refused(&b, "but the pairs run to");

        // A file ending mid-record, not on a record boundary: inside the stored size's tail, and
        // inside the next record's header.
        let mut b = good.clone();
        let size = u32::from_le_bytes(b[20..24].try_into().unwrap());
        set(&mut b, 20, size + 6);
        b.extend_from_slice(&[1, 2, 3]);
        refused(&b, "total_size runs past the end");
        let mut b = good.clone();
        set(&mut b, 4, 3);
        b.extend_from_slice(&[0x10, 0x00]);
        refused(&b, "EOF u32");

        // Garbage after the last record while the count promises more: short, zero-filled, and
        // filled with a repeated byte.
        for (garbage, want) in [(vec![0xAB; 3], "EOF u32"), (vec![0; 64], "name length 0"), (vec![0xAB; 64], "runs past the end")] {
            let mut b = good.clone();
            set(&mut b, 4, 3);
            b.extend_from_slice(&garbage);
            refused(&b, want);
        }
    }
}

#[cfg(test)]
mod prop_table_tests {
    use super::*;
    use crate::pandemic_hash;

    /// The type table is binary-searched, so sortedness is load-bearing, and a duplicate key would
    /// make the lookup pick an arbitrary one of them.
    #[test]
    fn the_type_table_is_sorted_and_unique() {
        for w in props::TYPES.windows(2) {
            assert!(w[0].0 < w[1].0, "TYPES not sorted/unique at 0x{:08X}", w[0].0);
        }
    }

    /// Spot-checks that survive regeneration, one per evidence class:
    /// - `Looping` / `Sound Events` — the audio attachment block, where the recovered NAMES and the
    ///   decompiled record layout independently agree
    /// - `Game Event Name` — a name containing SPACES, which identifier-shaped harvesting cannot
    ///   produce and which the community dictionary filed under a byteswapped key
    /// - `Model` / `Texture` — carried over from the 15 hand-listed names, so nothing regressed
    #[test]
    fn known_names_and_types_survive_regeneration() {
        for (key, name) in [
            (0x1E59E605u32, "Looping"),
            (0x42C6DA9D, "Sound Events"),
            (0x246DB06C, "Game Event Name"),
            (0xD76D7747, "Sound Event Name"),
            (0x5B724250, "Model"),
            (0xF011157A, "Texture"),
        ] {
            assert_eq!(pandemic_hash(name), key, "{name:?} is not a preimage of 0x{key:08X}");
            assert_eq!(
                known_property_name(key).map(str::to_ascii_lowercase),
                Some(name.to_ascii_lowercase()),
                "0x{key:08X} should be named {name:?} (pandemic_hash case-folds, so compare folded)"
            );
        }
        // Width-backed types are trustworthy: `Looping` is one byte in all 6,525 uses.
        assert_eq!(property_type(0x1E59E605), Some(PropType::Bool));
        // `AudibleEvent` is a Ref on the engine's own reader (FUN_004511d0) agreeing with width 4.
        assert_eq!(property_type(0x02D3EB9F), Some(PropType::Ref));
        // KNOWN LIMIT, not an oversight. `Game Event Name` really is a Ref — the decomp reads it
        // with the byte-reversing helper — but its only appearance is behind an inverted
        // `if (param_2 != 0xKEY)` guard, so equality-only attribution cannot claim it and the
        // size histogram cannot tell Scalar4 from Ref (both are 4 bytes). It therefore falls back
        // to Scalar4. Separating the two needs brace-aware extraction; until then this asserts the
        // conservative answer rather than a flattering one.
        assert_eq!(property_type(0x246DB06C), Some(PropType::Scalar4));
        // A key that is not in the retail table has no type.
        assert_eq!(property_type(0xDEADBEEF), None);
    }
}
