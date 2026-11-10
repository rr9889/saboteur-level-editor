//! WSAO material→texture resolver — the engine's real binding, read from `France.materials`
//! (loose file at the game root, magic `OASW`, ~4.3 MB — PRESENT on the retail PC build; the earlier
//! "absent on PC" note was wrong). A drawcall material hash → a WSMA record → a
//! `[textureBegin..+numTextures]` slice of the WSTX texture-name-hash array (order [d,s,n,wm], so
//! slot 0 is the colour map). This is what the click-path resolver uses instead of the name-suffix
//! heuristic — the mapping the game itself uses, correct for every character/vehicle/weapon/prop, not
//! just the ~27 assets whose textures happen to carry a `_D` suffix.

#![allow(dead_code)]

use std::collections::HashMap;

fn u32at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub struct Wsao {
    wstx: Vec<u32>,
    by_mat: HashMap<u32, (u32, u32)>,
    cutout: HashMap<u32, bool>,
}

impl Wsao {
    pub fn open(path: &str) -> Result<Wsao, String> {
        let b = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
        Self::parse(&b)
    }

    fn parse(b: &[u8]) -> Result<Wsao, String> {
        if b.len() < 80 || &b[0..4] != b"OASW" {
            return Err("not a WSAO (France.materials) file".into());
        }
        let num_wstx = u32at(&b, 4 * 17) as usize;
        let num_wsma = u32at(&b, 8) as usize;
        let find = |m: &[u8; 4]| b.windows(4).position(|w| w == m).ok_or("block not found");
        let wstx_off = find(b"XTSW")? + 4;
        let wsma_off = find(b"AMSW")? + 4;
        let wspa_off = find(b"APSW")? + 4;
        let num_wspa = u32at(b, 76) as usize;
        if num_wstx > (b.len() - wstx_off) / 4 || num_wspa > (b.len() - wspa_off) / 36 {
            return Err("WSTX or WSPA truncated".into());
        }
        let mut state_sets = Vec::new();
        let mut state_off = find(b"TSSW")? + 4;
        for _ in 0..u32at(b, 12) {
            if state_off + 4 > b.len() {
                return Err("WSST truncated".into());
            }
            let count = u32at(b, state_off) as usize;
            state_off += 4;
            if count > (b.len() - state_off) / 8 {
                return Err("WSST pairs truncated".into());
            }
            let alpha_test = (0..count).rev().find_map(|pair| {
                let offset = state_off + pair * 8;
                (u32at(b, offset) == 15).then(|| u32at(b, offset + 4) != 0)
            }).unwrap_or(false);
            state_sets.push(alpha_test);
            state_off += count * 8;
        }
        let wstx: Vec<u32> = (0..num_wstx).map(|i| u32at(&b, wstx_off + i * 4)).collect();
        let mut by_mat = HashMap::new();
        let mut cutout = HashMap::new();
        let mut o = wsma_off;
        for _ in 0..num_wsma {
            if o + 8 > b.len() {
                return Err("WSMA truncated".into());
            }
            // Field 0 is a record UID, NOT a drawcall material hash: measured to match a drawcall's
            // `materials[0]` 0/14331 times. The hashes a drawcall actually keys on are the `ids[]`
            // (alias) array that follows. Indexing by `uid` first (as this once did) only mislead the
            // lookup — on a collision it would hand back the UID owner's textures — so skip it.
            let _uid = u32at(&b, o);
            let idc = u32at(&b, o + 4) as usize;
            o += 8;
            if idc > 64 || o + idc * 4 + 16 > b.len() {
                return Err("WSMA aliases truncated or invalid".into());
            }
            let ids: Vec<u32> = (0..idc).map(|i| u32at(&b, o + i * 4)).collect();
            o += idc * 4;
            let num_tex = u32at(&b, o + 4);
            let tex_begin = u32at(&b, o + 8);
            let pass_index = u32at(b, o + 12) as usize;
            if pass_index >= num_wspa || tex_begin as u64 + num_tex as u64 > wstx.len() as u64 {
                return Err("WSMA texture or pass index out of range".into());
            }
            let pass_off = wspa_off + pass_index * 36;
            let flags = u32at(b, pass_off + 4);
            let state_index = u32at(b, pass_off + 8);
            let alpha_test = if state_index == u32::MAX {
                false
            } else {
                *state_sets.get(state_index as usize).ok_or("WSPA state index out of range")?
            };
            o += 16;
            for id in ids {
                by_mat.entry(id).or_insert((tex_begin, num_tex));
                cutout.entry(id).or_insert(flags & 0x20 != 0 || alpha_test);
            }
        }
        Ok(Wsao { wstx, by_mat, cutout })
    }

    /// Texture name-hashes for a material hash, in [diffuse, spec, normal, wm] order.
    pub fn textures(&self, mat: u32) -> Option<Vec<u32>> {
        let &(begin, n) = self.by_mat.get(&mat)?;
        let (b, e) = (begin as usize, begin.checked_add(n)? as usize);
        (e <= self.wstx.len()).then(|| self.wstx[b..e].to_vec())
    }

    /// Shader cutout-family bit (WSPA f1 & 0x20) or D3D9 ALPHATESTENABLE (WSST id 15).
    /// WSPA f2 selects a state set, not an alpha threshold; id 22 is culling, not alpha test.
    pub fn cutout(&self, mat: u32) -> Option<bool> {
        self.cutout.get(&mat).copied()
    }
}

/// Load a loose `<dir>/<hash:08X>.dtex` (as written by `sab_poc mattias`) and decode its finest mip.
pub fn load_loose_dtex(dir: &str, hash: u32) -> Result<crate::dtex::CpuTexture, String> {
    let path = format!("{dir}/{hash:08X}.dtex");
    let bytes = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
    // files are written with a leading 'DTEX' tag; the decoder wants the bare record
    let rec = if bytes.len() > 4 && &bytes[0..4] == b"DTEX" { &bytes[4..] } else { &bytes[..] };
    crate::dtex::decode(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_words(bytes: &mut Vec<u8>, words: &[u32]) {
        for word in words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }

    fn fixture() -> Vec<u8> {
        let mut bytes = vec![0; 80];
        bytes[..4].copy_from_slice(b"OASW");
        bytes[8..12].copy_from_slice(&5u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&3u32.to_le_bytes());
        bytes[68..72].copy_from_slice(&1u32.to_le_bytes());
        bytes[76..80].copy_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(b"TSSW");
        push_words(&mut bytes, &[1, 15, 1, 1, 22, 1, 2, 15, 1, 15, 0]);
        bytes.extend_from_slice(b"XTSW");
        push_words(&mut bytes, &[0x1234]);
        bytes.extend_from_slice(b"APSW");
        for (flags, state_index) in [(0, 0), (0, 1), (0x20, 1), (0, u32::MAX), (0, 2)] {
            push_words(&mut bytes, &[0, flags, state_index, 0, 0, 0, 0, 0, 0]);
        }
        bytes.extend_from_slice(b"AMSW");
        for pass_index in 0..5 {
            push_words(&mut bytes, &[100 + pass_index, 1, 10 + pass_index, 0, 1, 0, pass_index]);
        }
        bytes
    }

    #[test]
    fn cutout_uses_family_bit_or_alpha_test_not_cull_or_state_index() {
        let wsao = Wsao::parse(&fixture()).unwrap();
        assert_eq!(wsao.cutout(10), Some(true), "id 15 enables alpha test");
        assert_eq!(wsao.cutout(11), Some(false), "id 22 only enables CULL_NONE");
        assert_eq!(wsao.cutout(12), Some(true), "the shader family bit enables cutout");
        assert_eq!(wsao.cutout(13), Some(false), "sentinel means no state set");
        assert_eq!(wsao.cutout(14), Some(false), "last alpha-test state wins");
        assert_eq!(wsao.cutout(100), None, "UIDs are not material aliases");
        assert_eq!(wsao.textures(12), Some(vec![0x1234]));
    }

    #[test]
    fn duplicate_alias_keeps_texture_and_cutout_from_same_first_record() {
        let mut bytes = fixture();
        let second_alias = bytes.len() - 4 * 28 + 8;
        bytes[second_alias..second_alias + 4].copy_from_slice(&10u32.to_le_bytes());
        let wsao = Wsao::parse(&bytes).unwrap();
        assert_eq!(wsao.cutout(10), Some(true));
        assert_eq!(wsao.cutout(11), None);
        assert_eq!(wsao.textures(10), Some(vec![0x1234]));
    }

    #[test]
    fn malformed_counts_references_and_truncation_return_errors() {
        for header_offset in [8, 12, 68, 76] {
            let mut bytes = fixture();
            bytes[header_offset..header_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(Wsao::parse(&bytes).is_err(), "header {header_offset}");
        }
        let fixture = fixture();
        for length in 0..fixture.len() {
            assert!(Wsao::parse(&fixture[..length]).is_err(), "truncated at {length}");
        }
        let mut bad_pass = fixture.clone();
        let pass_offset = bad_pass.len() - 4;
        bad_pass[pass_offset..].copy_from_slice(&5u32.to_le_bytes());
        assert!(Wsao::parse(&bad_pass).is_err());
        let mut bad_texture = fixture.clone();
        let texture_offset = bad_texture.len() - 8;
        bad_texture[texture_offset..texture_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Wsao::parse(&bad_texture).is_err());
        let mut bad_state = fixture;
        let pass_start = bad_state.windows(4).position(|tag| tag == b"APSW").unwrap() + 4;
        bad_state[pass_start + 8..pass_start + 12].copy_from_slice(&3u32.to_le_bytes());
        assert!(Wsao::parse(&bad_state).is_err());
    }

    #[cfg(feature = "retail")]
    #[test]
    fn retail_material_database_resolves_texture_and_cutout_for_every_alias() {
        let game = sab_formats::game_paths::local_config_retail_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let wsao = Wsao::open(&game.join("France.materials").to_string_lossy()).unwrap();
        assert!(!wsao.by_mat.is_empty());
        assert!(wsao.cutout.values().any(|enabled| *enabled));
        assert!(wsao.cutout.values().any(|enabled| !enabled));
        for material in wsao.by_mat.keys() {
            assert!(wsao.textures(*material).is_some());
            assert!(wsao.cutout(*material).is_some());
        }
        eprintln!("{} material aliases, {} with WSPA/WSST cutout", wsao.by_mat.len(), wsao.cutout.values().filter(|enabled| **enabled).count());
    }
}
