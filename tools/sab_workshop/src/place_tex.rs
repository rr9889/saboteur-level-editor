//! Diffuse textures for the room's materials.
//!
//! The chain is the engine's own material table: a prim's material hash → a `France.materials` (WSAO)
//! record → its texture-name hashes, slot 0 being the colour map (`wsao.rs`) → the DTEX record whose
//! name hashes to that value, found in the room's own sub-pack first, then `Global/Dynamic0` and
//! `Global/Palettes0` → decoded RGBA (`dtex.rs`). Only the diffuse slot is used; spec, normal and
//! mask maps are not.
//!
//! Alpha is selected by the material's WSPA/WSST cutout state, not the texture's pixel distribution.
//! Preview copies are keyed by material so one diffuse can be opaque and cutout in different draws.
//! The GPU's fixed 0.5 alpha threshold remains an editor approximation, not a recovered runtime
//! fAlphaTest constant. Blending and the other pass states are not reproduced here.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::dtex::CpuTexture;

/// The room's textures: which texture each material uses, and the decoded pixels.
pub struct TextureSet {
    /// Material hash → preview texture key (the material hash, not the source DTEX hash).
    pub mat_tex: HashMap<u32, u32>,
    /// Preview texture key → decoded pixels (material cutout alpha or opaque 255).
    pub tex: HashMap<u32, Arc<CpuTexture>>,
    /// What was resolved and what was not.
    pub report: String,
}

impl TextureSet {
    pub fn empty(report: impl Into<String>) -> TextureSet {
        TextureSet { mat_tex: HashMap::new(), tex: HashMap::new(), report: report.into() }
    }

    /// Whether any material has a decoded diffuse texture.
    pub fn is_useful(&self) -> bool {
        self.mat_tex.values().any(|h| self.tex.contains_key(h))
    }

    fn bind_texture(&mut self, material: u32, source: &CpuTexture, cutout: bool) -> bool {
        if material == 0 || material == u32::MAX {
            return false;
        }
        let mut texture = source.clone();
        apply_cutout_rule(&mut texture, cutout);
        self.mat_tex.insert(material, material);
        self.tex.insert(material, Arc::new(texture));
        true
    }

    /// The union of several sets from the same material database. Preview keys are material aliases,
    /// so shared diffuse textures with different material states remain distinct. Reports are joined.
    pub fn merge(sets: &[&TextureSet]) -> TextureSet {
        let mut out = TextureSet::empty(String::new());
        let mut reports = Vec::new();
        for s in sets {
            out.mat_tex.extend(s.mat_tex.iter().map(|(k, v)| (*k, *v)));
            out.tex.extend(s.tex.iter().map(|(k, v)| (*k, v.clone())));
            reports.push(s.report.clone());
        }
        out.report = reports.join(" | ");
        out
    }
}

/// Keep source alpha for cutout materials; alpha in other materials must not cause GPU discard.
fn apply_cutout_rule(t: &mut CpuTexture, cutout: bool) {
    if !cutout {
        for px in t.rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
}

/// Outdoor meshes store diffuse textures in their effective mounted terrain cells.
pub fn resolve_cells(game: &Path, cells: &BTreeSet<u32>, materials: &BTreeSet<u32>) -> TextureSet {
    if cells.is_empty() {
        return resolve(game, &[], materials);
    }
    let mounts = match sab_tools::cells::france_mounts(game) {
        Ok(mounts) => mounts,
        Err(error) => return TextureSet::empty(format!("cell textures: {error}")),
    };
    let mut own = Vec::new();
    for cell in cells {
        if let Some((mount, entry)) = sab_tools::cells::resolve_entry(&mounts, sab_tools::cells::cell_crc(*cell)) {
            if let Ok(bytes) = sab_formats::megapack::entry_slice(&mount.map, entry) {
                own.push(bytes);
            }
        }
    }
    resolve(game, &own, materials)
}

/// Resolve the diffuse textures of `materials`. `own` are the room's own sub-pack slices, searched
/// first. Never fails: what can't be resolved is counted in the report.
pub fn resolve(game: &Path, own: &[&[u8]], materials: &BTreeSet<u32>) -> TextureSet {
    let wsao_path = game.join("France.materials");
    let wsao = match crate::wsao::Wsao::open(&wsao_path.to_string_lossy()) {
        Ok(w) => w,
        Err(e) => return TextureSet::empty(format!("textures: not available ({e})")),
    };
    let mut mat_tex = HashMap::new();
    let (mut no_record, mut no_diffuse) = (0usize, 0usize);
    for &m in materials {
        match wsao.textures(m) {
            None => no_record += 1,
            Some(t) => match t.first().copied().filter(|&h| h != 0) {
                Some(h) => {
                    mat_tex.insert(m, h);
                }
                None => no_diffuse += 1,
            },
        }
    }
    let diffuse: HashSet<u32> = mat_tex.values().copied().collect();
    let mut needed = diffuse.clone();
    let mut found: HashMap<u32, crate::resolve::TexAsset> = HashMap::new();
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    if let Err(e) = crate::resolve::take_hashes_from_slices(own, &mut needed, &mut found) {
        errors.push(e);
    }
    sources.push(format!("{} in the room's own sub-pack", found.len()));
    for (label, rel) in [("Dynamic0", "Global/Dynamic0.megapack"), ("Palettes0", "Global/Palettes0.megapack")] {
        if needed.is_empty() {
            break;
        }
        let before = needed.len();
        match crate::pack::Megapack::open(&game.join(rel).to_string_lossy()) {
            Ok(mp) => {
                if let Err(e) = crate::resolve::take_hashes_from_packs(&[&mp], &mut needed, &mut found) {
                    errors.push(format!("{label}: {e}"));
                }
                sources.push(format!("{} in {label}", before - needed.len()));
            }
            Err(e) => errors.push(format!("{label}: {e}")),
        }
    }
    let mut decoded = HashMap::new();
    let mut undecoded = 0usize;
    for (h, asset) in found {
        match asset.decode() {
            Ok(t) => {
                decoded.insert(h, t);
            }
            Err(e) => {
                undecoded += 1;
                errors.push(format!("texture {h:08x}: {e}"));
            }
        }
    }
    let mut output = TextureSet::empty(String::new());
    let mut cutout = 0usize;
    for (material, hash) in &mat_tex {
        if let Some(texture) = decoded.get(hash) {
            let enabled = wsao.cutout(*material).unwrap_or(false);
            if output.bind_texture(*material, texture, enabled) {
                cutout += usize::from(enabled);
            } else {
                errors.push(format!("material {material:08x}: reserved preview texture key"));
            }
        }
    }
    let mut report = format!(
        "textures: {} materials → {} diffuse textures ({}); {} materials without a WSAO record, {} without a diffuse slot, {} textures not found, {} not decoded; WSPA/WSST cutout alpha on {} materials (preview threshold 0.5 approximate; blending not implemented)",
        materials.len(),
        diffuse.len(),
        sources.join(", "),
        no_record,
        no_diffuse,
        needed.len(),
        undecoded,
        cutout
    );
    if !errors.is_empty() {
        report.push_str(&format!("; errors: {}", errors.join(" | ")));
    }
    output.report = report;
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tex(alphas: &[u8]) -> CpuTexture {
        CpuTexture { name: "t".into(), width: alphas.len() as u32, height: 1, format: 0, rgba: alphas.iter().flat_map(|&a| [10, 20, 30, a]).collect() }
    }

    #[test]
    fn material_state_not_pixel_distribution_selects_cutout() {
        let mut smooth = tex(&[200, 90, 40, 255, 130, 60, 220, 10, 100, 170]);
        let original = smooth.rgba.clone();
        apply_cutout_rule(&mut smooth, true);
        assert_eq!(smooth.rgba, original, "smooth alpha remains available to a cutout shader");

        let mut binary = tex(&[0, 255, 0, 255]);
        apply_cutout_rule(&mut binary, false);
        assert!(binary.rgba.chunks_exact(4).all(|pixel| pixel == [10, 20, 30, 255]));
    }

    #[test]
    fn shared_diffuse_has_independent_material_alpha_and_merge_keys() {
        let source = tex(&[0, 90, 255]);
        let mut opaque = TextureSet::empty("");
        let mut cutout = TextureSet::empty("");
        assert!(opaque.bind_texture(10, &source, false));
        assert!(cutout.bind_texture(20, &source, true));
        let merged = TextureSet::merge(&[&opaque, &cutout]);
        let opaque_pixels = &merged.tex[&merged.mat_tex[&10]].rgba;
        let cutout_pixels = &merged.tex[&merged.mat_tex[&20]].rgba;
        assert!(opaque_pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert_eq!(*cutout_pixels, source.rgba);
        assert_eq!(source.rgba[3], 0, "source decode remains unmodified");
        assert!(merged.is_useful());
    }

    #[test]
    fn gpu_sentinel_material_keys_are_not_bound() {
        let mut textures = TextureSet::empty("");
        assert!(!textures.bind_texture(0, &tex(&[0]), true));
        assert!(!textures.bind_texture(u32::MAX, &tex(&[0]), true));
        assert!(!textures.is_useful());
    }
}
