---
author: codex
status: current
---
# The Saboteur (2009) Level Editor

Source release of the experimental **SAB Workshop** level editor and asset workbench.
The native Rust application uses wgpu and egui. It loads game data from an installation
selected by the user; game data is not included in this repository.

The editor provides a placement viewport, terrain preview and picking, placement
editing sessions, project serialization, and staged exports. It also includes model,
animation, texture, text and template inspection tools. This is a development snapshot;
do not assume that arbitrary edits or a complete village workflow are validated in game.

## Build and run

Use stable Rust with the native C/C++ toolchain for your platform. This snapshot is
validated on Windows x86_64 with Rust 1.95 and the Visual Studio C++ build tools.
Select the game directory in Settings, or pass it explicitly:

```powershell
cargo build --release --locked -p sab_workshop --bin sab_workshop
cargo run --release --locked -p sab_workshop --bin sab_workshop -- --game "D:/Games/The Saboteur"
```

Use a release build for interactive work: asset decoding is slow in debug builds.
The application saves personal settings outside the repository in the user's config
directory. The game is required to inspect or edit its content, but not to compile.

Run the synthetic/offline checks without a game installation:

```powershell
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
```

Some Windows staging-script tests need PowerShell 7 (`pwsh`). Tests behind the `retail`
feature additionally require local game files and, for some cases, earlier experiment
outputs. They are excluded from the default checks. `.sab-local.example.toml` shows
the local path format; do not commit your personal configuration.

Release validation: `cargo check --workspace --all-targets --locked --offline` passed
on Windows. The default offline test suite passed 391 tests with 8 ignored and no
failures. No live-game compatibility testing was performed for this packaging task.

## Source layout

| Path | Purpose |
| --- | --- |
| `tools/sab_workshop` | Native editor, GPU renderer, UI and shaders |
| `tools/sab_tools` | Editing sessions, projects, staging and command-line tools |
| `tools/sab_formats` | Shared parsers, codecs and serializers |
| `tools/sab_terrain` | Terrain access and geometry |
| `tools/sab_terrain_query` | Read-only terrain queries and picking support |
| `data/hash_names.tsv` | Small generated schema-key lookup table |

This release includes code, tests, Cargo manifests/lockfile, required shaders, licensed
UI fonts and minimal build documentation. It omits the research archive, reverse-
engineering reports, AI/session transcripts, generated evidence, retail assets,
executables, private configuration and the original development Git history.

The extracted 80,000+ entry retail name catalog is deliberately absent. The included
table resolves only a small set of format keys already defined in source. Other names
can appear as hexadecimal hashes. A user may replace the table locally before building;
the original catalog-generation research tool is not part of this release.

Live-game readback also needs the separate `sab_asi` development bridge, which is not
included in this editor source snapshot. Missing container-name mappings additionally
limit live readback. Offline editing and staging do not require that bridge. This
release does not claim that all live-game controls are self-contained.

## License and origin

The original project code is provided under the existing [MIT license](LICENSE).
The XMA2 decoder port retains its **LGPL-2.1-or-later** license and FFmpeg attribution;
Oswald fonts retain their **SIL Open Font License**. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

This snapshot was extracted from the maintained editor in
[notes-on-reversing-the-saboteur](https://github.com/Mercenaries-Fan-Build/notes-on-reversing-the-saboteur),
including the editor-related working-tree changes available at release preparation.
It is an unofficial community project and is not the game's original source code.
