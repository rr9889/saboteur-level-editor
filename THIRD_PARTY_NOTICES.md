---
author: codex
status: current
---
# Third-party notices

- The upstream project's MIT notice is preserved verbatim in `LICENSE`, including
  Copyright (c) 2026 Mercenaries-Fan-Build.
- `tools/sab_formats/src/xma2/` contains an FFmpeg XMA2 decoder port. It is licensed
  LGPL-2.1-or-later, with its original attribution comments and the complete license
  in `tools/sab_formats/src/xma2/COPYING.LGPLv2.1`. The crate accordingly declares
  `MIT AND LGPL-2.1-or-later`.
- `tools/sab_workshop/assets/fonts/Oswald-Medium.ttf` and `Oswald-SemiBold.ttf` are
  redistributed with their SIL Open Font License in that directory's `OFL.txt`.
- `mercs2_formats` and the remaining Cargo dependencies are fetched separately;
  their licenses and source attributions remain with those projects. This release
  does not vendor their source or override their licensing.
- Source comments describing observed format behavior and generated numeric
  compatibility tables are retained. No game archives, executable, symbols,
  decompiled game source, meshes, textures, audio or extracted scripts are included.

This is a source-only distribution. It contains no compiled application release.
