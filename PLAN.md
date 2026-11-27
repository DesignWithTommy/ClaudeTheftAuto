# ClaudeTheftAuto: plan for a new runtime

Target rig: RTX 4060 8 GB (PCIe 4.0 x8), i5-14400F, 16 GB RAM, 1920x1080 output. GPL-3.0. Cloned parts keep their credit in CREDITS.md. Sources for this plan are Vnity's `docs/FOCUS.md`, `findings.md`, `translators.md`, `research-translation.md`, `research-memory.md`, `r-translate.md` and `features/other-engines.md`.

## The three costs (measured on the rig with Vice-Tay/CTA)
1. **Memory held twice:** commit 22-36 GB, free RAM down to 3 MB, hangs from 60-80 s. CTA's commit ceiling got 90 s with no hang, but only 5.1 fps.
2. **CPU per draw:** ~24k draws, submit 91 ms a frame, 2-11 fps. Re-translating unchanged state every frame is most of it.
3. **Compiles:** 137 s PSO build, and 815 of 855 shader-cache lookups miss per run, so it stutters.

Hard limits: the x8 bus moves ~250 MB a frame at 30 fps in practice, and 16 GB RAM cannot hold the pool plus a shadow plus staging. Cutting duplicated commit comes before cutting copied bytes.

## What others tried and where each stops
| Approach | Who | Where it stops |
|---|---|---|
| Host shadow + VirtualProtect watch + card compare | VIctory family, Vice-Tay | Memory held twice; fault per first write; RAM collapse |
| Shaders read title memory in place | vilocity compat | 33 fps on a 5080; NVIDIA reads host memory "far slower" (AnyPS5: 164 vs 45 ms) |
| Pagefile sections + a page table read by shaders | BootTest (no licence: idea only) | Every shader access pays the table |
| ReBAR single copy | victoryperf, CTA switch | 0.3 fps on 8 GB: silently demoted to system memory over budget |
| OpenExistingHeap windows | xcl, ztrx, TuxBox | 4095 MiB per window; FromAddress locks pages and blocks VirtualProtect; tile mappings into it remove the device |
| Import where it can, copy where it must, write watch | AnyPS5 (PS5) | Closest design; per-resource policy, Vulkan only |
| Translate on first use, live compile | almost all | Stalls; vilocity hides them with stand-ins that draw nothing |
| Re-translate every command list every frame | all | 91 ms submit |

None of them place memory per block by measured use, and none reuse last frame's translated command chunks.

## Architecture

### 1. One backing, placed per 64 KiB block by measured use (memory)
- **Backing:** the title's pool is `SEC_COMMIT` file-mapped sections at its fixed address, committed on first touch (BootTest idea), reserved with `MEM_WRITE_WATCH`. There is no CPU shadow copy.
- **CPU dirty:** one `GetWriteWatch(RESET)` a frame, stamped into 64 KiB blocks with epochs (AnyPS5 `GuestMemory.cpp`, port). There is no VirtualProtect fault path. This is also a prerequisite, because opened heaps block VirtualProtect.
- **GPU view:** sections are opened with `OpenExistingHeapFromFileMapping` (no page lock, 0.2 ms per 256 MiB; ztrx `inplace.rs`, copy) in windows under 4095 MiB (xcl `uma.rs`, copy). The import-settle step drops the driver's false dirty marks (AnyPS5).
- **Placement:** every block counts its GPU reads and CPU writes per frame and gets a tier, with a policy that changes as the counts do:
  - **In place:** read a few times (under ~512 reads; AnyPS5's NVIDIA limit). The GPU reads system memory directly, with no copy.
  - **VRAM cache:** hot blocks, copied once and kept valid by per-block valid bits. Only the ranges a draw reads and that are invalid get uploaded, in one copy-queue batch a frame.
  - **GPU-owned:** written only by the GPU (render targets). Lives in VRAM, and changed bytes are copied back only when the CPU touches it (Ryujinx flush-on-access, AnyPS5 changed-bytes copy-back).
  - **Cold:** untouched for N frames. Evicted from VRAM, LRU.
- **Budgets the runtime enforces, not the driver:** a VRAM cap from `QueryVideoMemoryInfo` (about 6.5 GB on the 4060), an import cap, a commit ceiling and a free-RAM floor (five-star-recomp limits, port), plus a compile-concurrency cap. No sparse tiles at pool scale (xcl saw multi-minute stalls).
- **Fallback:** the plain copy path behind a switch, if `D3D12_FEATURE_EXISTING_HEAPS` is missing. Microsoft calls opened heaps diagnostics-only.

### 2. Incremental frame translation (CPU)
- The title's command lists are hashed in chunks keyed by state plus the epochs of the blocks they touch. An unchanged chunk replays last frame's translated D3D12 commands; only changed chunks are translated again.
- Inside a changed chunk, only dirty state is re-bound (DXVK/Xenia dirty bits, United `LIST_REUSE`), one-record ExecuteIndirect calls are merged (victoryperf `batch.rs`, copy), and the heap pass re-checks only marked slots (United `heap_pass.rs`, copy).
- Translation runs one frame behind on worker threads (decode, then translate, then record one list per worker, then submit) with kept threads. Uploads and readbacks stay off the recording thread (copy-queue lanes).

### 3. Shaders ahead of time
- All ~25.9k title programs are translated offline into a content-keyed cache that is stable across translator builds. The run only loads them, which ends the 815/855 misses.
- Clone: five-star-recomp's structured control-flow recovery and its direct DXIL backend (5-20x faster than dxc, with a dxc fallback), its resumable `setup` step runner and its auto-warm when the translator changes; victoryperf `precompile.rs`; WARP instruction checks run offline.
- At run time a miss gets a stand-in that draws nothing (vilocity `pso_async.rs`) while the real pipeline builds in the background, under the compile cap. PSO keys are journaled and the missing list is written out (BootTest idea) for the next prebuild.

### 4. Measured from day one
- Every frame, one shared-memory feed reports frame time, submit time, commit, free RAM, VRAM per tier, MB copied, chunks reused vs retranslated, and PSO misses.
- `cta probe` runs first: copy-queue H2D GB/s on the rig, EXISTING_HEAPS, OPTIONS16, and local and non-local budgets.
- Each feature is proven with a 1 s replay A/B, then one 90 s run at 1080p.

## What gets cloned unchanged
Vice-Tay's PE loader, its HLE of the console libraries (xmem, d3d12_xs, files, audio, input, DirectStorage/BCPack, saves), `VICTORY_OUTPUT` 1080p patching and the RDNA2 decoder. These already work and are not where the cost is.

## Order
0. `cta probe`. Its numbers decide the in-place limits and the budgets.
1. Skeleton: the cloned loader and HLE booting to the first Present, plus the telemetry feed.
2. Write watch with block epochs, and no CPU shadow. Goal: commit under 16 GB.
3. Opened-heap windows and placement tiers, with the copy path as fallback. Goal: under 250 MB copied a frame.
4. Offline shader cache, stand-ins, compile cap. Goal: no stutter, no 137 s build.
5. Incremental chunk translation and threaded recording. Goal: submit under 20 ms.
6. In the world at 1080p, no RAM collapse, then 30 fps.

## Risks
- Opened heaps have no public performance data, and Microsoft discourages them. Step 0 or 3 may force the copy fallback for all tiers.
- Chunk reuse is only valid if every input a chunk reads is in its key. A missed input draws stale frames, so a check mode retranslates and compares.
