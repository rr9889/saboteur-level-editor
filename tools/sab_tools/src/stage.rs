//! Staging: artifacts are written to an output folder with a manifest and PowerShell install /
//! restore scripts the user runs. Nothing here writes into the game directory.

use std::fmt::Write as _;
use std::path::Path;

use memmap2::Mmap;
use sab_formats::{editnodes, loosefiles};
use serde_json::json;
use sha2::{Digest, Sha256};

/// The base EditNodes pack's name inside `France\loosefiles_BinPC.pack`.
pub const EDITNODES_LOOSE: &str = r"France\EditNodes\EditNodes.pack";
/// The loose-file container holding the base EditNodes pack.
pub const LOOSEFILES: &str = r"France\loosefiles_BinPC.pack";
/// The DLC slot's EditNodes pack, searched before the base pack per entry hash
/// (`docs/formats/editnodes.md`, DLC override).
pub const DLC_EDITNODES: &str = r"DLC\01\France\EditNodes\EditNodes.pack";

/// Upper-case hex SHA-256, the form `Get-FileHash` prints.
pub fn sha256(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02X}")).collect()
}

pub fn map(p: &Path) -> Result<Mmap, String> {
    let file = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    unsafe { Mmap::map(&file) }.map_err(|e| format!("{}: {e}", p.display()))
}

/// The one check every writer calls before writing at `path` (a central write guard, after
/// W3ModWorkbench's `FileSafety`): refuses the game folder (and with it the DLC folders), the user's
/// Downloads folder and Rust build output (`target`, `target_*`). Paths are compared lexically and
/// case-insensitively, so a path that does not exist yet is judged too.
pub fn guard_write(path: &Path, game: &Path) -> Result<(), String> {
    let norm = |p: &Path| -> String {
        let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf()) };
        let mut s = abs.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
        while s.ends_with('/') {
            s.pop();
        }
        s
    };
    let inside = |p: &str, root: &str| !root.is_empty() && (p == root || p.starts_with(&format!("{root}/")));
    let p = norm(path);
    if !game.as_os_str().is_empty() && inside(&p, &norm(game)) {
        return Err(format!("refusing to write inside the game folder: {}", path.display()));
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        if inside(&p, &norm(&Path::new(&home).join("Downloads"))) {
            return Err(format!("refusing to write inside Downloads: {}", path.display()));
        }
    }
    if p.split('/').any(|c| c == "target" || c.starts_with("target_")) {
        return Err(format!("refusing to write inside build output: {}", path.display()));
    }
    Ok(())
}

pub fn write(out: &Path, name: &str, b: &[u8]) -> Result<(), String> {
    std::fs::write(out.join(name), b).map_err(|e| format!("{}: {e}", out.join(name).display()))
}

/// The two EditNodes packs the engine searches (DLC slot first, then base), as bytes and parsed.
pub struct EditNodesSources {
    pub base_bytes: Vec<u8>,
    pub dlc_bytes: Vec<u8>,
    pub base: editnodes::Pack,
    pub dlc: editnodes::Pack,
    /// SHA-256 of the whole `loosefiles_BinPC.pack` the base pack came from.
    pub loosefiles_sha: String,
}

impl EditNodesSources {
    pub fn open(game: &Path) -> Result<EditNodesSources, String> {
        let lf = map(&game.join(LOOSEFILES))?;
        let entries = loosefiles::entries(&lf)?;
        let e = loosefiles::find(&entries, EDITNODES_LOOSE).ok_or("EditNodes.pack is not in loosefiles_BinPC.pack")?;
        let base_bytes = lf[e.data.clone()].to_vec();
        let dlc_bytes = std::fs::read(game.join(DLC_EDITNODES)).map_err(|e| format!("{DLC_EDITNODES}: {e}"))?;
        EditNodesSources::from_bytes(base_bytes, dlc_bytes, sha256(&lf))
    }

    pub fn from_bytes(base_bytes: Vec<u8>, dlc_bytes: Vec<u8>, loosefiles_sha: String) -> Result<EditNodesSources, String> {
        let base = editnodes::Pack::parse(&base_bytes)?;
        let dlc = editnodes::Pack::parse(&dlc_bytes)?;
        Ok(EditNodesSources { base_bytes, dlc_bytes, base, dlc, loosefiles_sha })
    }

    /// The winning copy of a container: the DLC slot's if it has one, else the base pack's.
    pub fn winner(&self, container: u32) -> Option<(&'static str, &editnodes::Entry)> {
        match self.dlc.entry(container) {
            Some(e) => Some(("DLC/01", e)),
            None => self.base.entry(container).map(|e| ("base (loosefiles)", e)),
        }
    }

    pub fn dlc_sha(&self) -> String {
        sha256(&self.dlc_bytes)
    }
}

/// One file a staged package installs.
pub struct Dest {
    pub rel: String,
    pub existed: bool,
    pub original_sha: Option<String>,
    pub staged_name: String,
    pub installed_sha: String,
}

/// The manifest and the two PowerShell scripts. Install refuses unless every precondition file still
/// has its recorded hash and each destination is still in its recorded original state (or already
/// installed); restore puts back the recorded original only from the recorded installed state, and
/// does nothing when the original is already in place.
pub fn write_scripts(out: &Path, game: &Path, exp: &str, dests: &[Dest], pre: &[(String, String)], provenance: serde_json::Value) -> Result<(), String> {
    let files: Vec<_> = dests
        .iter()
        .map(|d| {
            json!({ "dest": d.rel, "existed_before": d.existed, "original_sha256": d.original_sha,
                    "staged_file": d.staged_name, "installed_sha256": d.installed_sha,
                    "backup": format!("{}.{exp}.bak", d.rel) })
        })
        .collect();
    let pre_json: Vec<_> = pre.iter().map(|(rel, h)| json!({ "file": rel, "sha256": h })).collect();
    let manifest = json!({ "experiment": exp, "game_dir": game.display().to_string(), "preconditions": pre_json, "files": files, "provenance": provenance });
    write(out, "manifest.json", serde_json::to_string_pretty(&manifest).unwrap().as_bytes())?;

    let mut inst = String::new();
    let mut rest = String::new();
    for s in [&mut inst, &mut rest] {
        let _ = writeln!(s, "# Generated by sab_tools for {exp}. Run from this folder.\n$ErrorActionPreference = 'Stop'");
        let _ = writeln!(s, "$Game = '{}'\nfunction Get-Sha256($p) {{ if (Test-Path -LiteralPath $p) {{ (Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash }} else {{ $null }} }}", game.display());
    }
    for (rel, h) in pre {
        let _ = writeln!(inst, "if ((Get-Sha256 (Join-Path $Game '{rel}')) -ne '{h}') {{ throw \"precondition: {rel} is missing or changed since staging; refusing\" }}");
    }
    for destination in dests {
        let _ = writeln!(inst, "if ((Get-Sha256 (Join-Path $PSScriptRoot '{staged}')) -ne '{new}') {{ throw \"staged file {staged} is missing or changed since staging; refusing\" }}",
            staged = destination.staged_name, new = destination.installed_sha);
    }
    for d in dests {
        let orig = d.original_sha.clone().map_or("$null".into(), |h| format!("'{h}'"));
        let _ = writeln!(
            inst,
            "$dest = Join-Path $Game '{rel}'; $bak = \"$dest.{exp}.bak\"; $orig = {orig}; $new = '{new}'
$cur = Get-Sha256 $dest
if ($cur -eq $new) {{ Write-Host \"already installed: {rel}\"; return }}
if ($cur -ne $orig) {{ throw \"{rel} changed since staging (sha256 $cur); refusing\" }}
if ($orig) {{ if (Test-Path -LiteralPath $bak) {{ if ((Get-Sha256 $bak) -ne $orig) {{ throw \"$bak exists with different contents; refusing\" }} }} else {{ Copy-Item -LiteralPath $dest -Destination $bak }} }}
New-Item -ItemType Directory -Force -Path (Split-Path $dest) | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot '{staged}') -Destination $dest -Force
if ((Get-Sha256 $dest) -ne $new) {{ throw \"installed {rel} hash mismatch\" }}
Write-Host \"installed: {rel}\"",
            rel = d.rel,
            new = d.installed_sha,
            staged = d.staged_name
        );
        let _ = writeln!(
            rest,
            "$dest = Join-Path $Game '{rel}'; $bak = \"$dest.{exp}.bak\"; $orig = {orig}; $new = '{new}'
$cur = Get-Sha256 $dest
if ($cur -eq $orig) {{ Write-Host \"already in its original state: {rel}\"; return }}
if ($cur -ne $new) {{ throw \"{rel} changed since install (sha256 $cur); refusing\" }}
if ($orig) {{
  if ((Get-Sha256 $bak) -ne $orig) {{ throw \"backup $bak missing or changed; refusing\" }}
  Copy-Item -LiteralPath $bak -Destination $dest -Force
  if ((Get-Sha256 $dest) -ne $orig) {{ throw \"restored {rel} hash mismatch\" }}
  Remove-Item -LiteralPath $bak
}} else {{ Remove-Item -LiteralPath $dest }}
Write-Host \"restored: {rel}\"",
            rel = d.rel,
            new = d.installed_sha
        );
    }
    write(out, "install.ps1", inst.as_bytes())?;
    write(out, "restore.ps1", rest.as_bytes())
}

pub fn diff_positions(a: &[u8], b: &[u8]) -> Vec<usize> {
    (0..a.len().min(b.len())).filter(|&i| a[i] != b[i]).collect()
}

pub fn ranges_cover(diffs: &[usize], ranges: &[(usize, usize)]) -> bool {
    diffs.iter().all(|&i| ranges.iter().any(|&(s, n)| i >= s && i < s + n))
}
