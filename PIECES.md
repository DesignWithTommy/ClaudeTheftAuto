# CTA pieces

CTA is built like a puzzle: no base runtime. Each piece is one crate (or one
file of the front end) with one job, taken from whichever runtime does that
job best, or written by us. A piece can be swapped for another runtime's
version of the same job without touching the others. The full list of what
each runtime offers is in `vnity/docs/Features.md` and `vnity/features/*.md`.

Rule for a swap: the new piece must build against the same neighbours. A
test run through i95 at 1920x1080 must beat the old piece on the rig (RTX
4060 8 GB, i5-14400F, 16 GB) before the swap stays.

## The board

| Piece | Job | From | Why this one | Swap candidates |
|---|---|---|---|---|
| `apps/cta/src/main.rs` | front end: `run`, `shaders`, `warm` only | **ours** | | |
| `crates/cta-mem` | one home for title memory, 64 KiB block tiers, upload ring (PLAN.md 2.1) | **ours** | | |
| `crates/cta-probe` | step 0 driver probe | **ours** (inplace.rs from VI-BR-PORT) | | |
| `apps/cta/src/run.rs` | start the title: map, bind, run | Vice-Tay 3e3fd57 | Vice-Tay with preset_4060 was the best on the rig (~16 fps first room) | |
| `apps/cta/src/warm.rs` | pipeline prebuild workers | Vice-Tay 3e3fd57 | same | VIctory-NVIDIA warm, five-star |
| `apps/cta/src/translate*`, `shader*` | shader cache fill (`shaders`) | Vice-Tay 3e3fd57 | same | |
| `crates/victory-pe` | PE32+ reading | Vice-Tay 3e3fd57 | same | |
| `crates/victory-loader` | maps the title's x64 image natively | Vice-Tay 3e3fd57 | same | |
| `crates/victory-hle` | the Xbox system API the title imports | Vice-Tay 3e3fd57 | same | |
| `crates/victory-shader` | Xbox shaders to DXIL | Vice-Tay 3e3fd57 | same | ztrx shader crate |
| `crates/victory-gpu` | D3D12.X command streams onto host D3D12 | Vice-Tay 3e3fd57 | same; **the piece CTA replaces part by part** (memory and submit go to cta-mem, PLAN step 3) | |
| `crates/victory-formats` | texture layouts, audio codecs | Vice-Tay 3e3fd57 | same | |
| `crates/victory-game` | title containers, build ids, patches | Vice-Tay 3e3fd57 | same | |
| `crates/victory-pipeline` | offline asset conversion cache | Vice-Tay 3e3fd57 | needed by victory-gpu | |
| `crates/victory-core` | config, paths, logging | Vice-Tay 3e3fd57 | same | |

## Plug points still to wire

cta-mem's census hooks went into M1LK's GPU piece. They go back in on the
Vice-Tay piece next:

- title memory home: `victory-hle/src/modules/xmem.rs` (section flags, `title_memory_section`)
- read sets: `victory-gpu/src/host.rs` refresh batch flush
- GPU writes: `Step::GpuWrite`
- CPU writes: `victory-gpu/src/watch.rs`
- frame end: the stats `frame()` in `host.rs`
