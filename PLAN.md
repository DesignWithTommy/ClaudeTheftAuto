# ClaudeTheftAuto: plan for a new runtime

Target rig: RTX 4060 8 GB (PCIe 4.0 x8), i5-14400F (6P + 4E, 16 threads), 16 GB RAM, 1920x1080 output. GPL-3.0, all Rust. Cloned code keeps its credit in CREDITS.md.

Sources read: Vice-Tay `3e3fd57` (crates/victory-gpu, victory-hle xmem), vilocity `b959d90` (native + compat), five-star-recomp `6a03b03`, Kelvaroq `92606ea` (patches 0055/0062/0063), AnyPS5 `docs/dev/ARCHITECTURE.md`, CTA run logs on the rig, and Vnity's research docs (FOCUS, findings, translators, research-translation, research-memory, r-translate, other-engines).

## 1. What the failures teach

The game already boots, loads and draws the 3D world under several runtimes. What fails is cost. There are two lineages, and they fail in opposite ways.

**Copy lineage** (VIctory, Vice-Tay, vilocity, CTA, Kelvaroq's public build):
- Every range the GPU touches gets a host copy. Big buffers have four: title bytes, a system-RAM staging side, a VRAM card side and a "before" twin used to find GPU writes. Textures have three (tiled title bytes, staging, VRAM).
- The result is 22-27 GB of RAM at 1080p (vilocity), and 17.5 GB private with 634 MB free on our rig before it hangs.
- All bookkeeping runs at submit, on one thread. On the 4090 with the Series S profile, a 47.3 ms frame is 33.7 ms "preparing" plus 12.6 ms copying back.
- On our rig the bad frames show what it costs: 6.5 GB a frame checked by the watch, 99% of it unchanged. Also 18k refreshes, 218k page faults, a 2.4 s heap pass, and 5,300 draws dropped for want of a pipeline.

**In-place lineage** (five-star-recomp by default, Kelvaroq 0063, vilocity compat):
- Title memory is `SEC_COMMIT` sections, opened as D3D12 heaps, and shaders read it by console address through a page map. There is one copy and no write tracking for buffers.
- But that memory sits in system RAM, so every GPU access crosses PCIe, plus two dependent map loads. The result is 200-400 ms world frames, and five-star's own roadmap says "game memory in video memory".
- AnyPS5 measured the same thing on NVIDIA: draws took 164 ms reading in place against 45 ms with copies.

**The bus is not the limit.** On the 4090 run, 87 MB goes up and 30 MB comes back a frame, about 3.5 GB/s at 30 fps, against ~13 GB/s usable on x8. The limits are three things:
1. Duplicated memory, which takes RAM.
2. Bookkeeping that scales with memory size, not with what changed.
3. Serial submit-time work.

Other facts that bound the design (measured by their authors):
- `OpenExistingHeapFromFileMapping` needs `SEC_COMMIT`, locks nothing and takes 0.2 ms per 256 MiB.
- `FromAddress` pins every page and blocks `VirtualProtect`.
- Tile mappings into an opened heap remove the device.
- Windows must stay under 4095 MiB.
- Write watch does not work on section views. Guards cost about 5 µs per change.
- 2 MiB sections cut map calls from 848 to 117 ms/s, a 2x fps gain (vilocity).
- The title creates 84k pipelines but draws with about 2-4k (a 4,259-name used list exists).
- The title calls the command-list vtable once per command, 45-60k times a frame. There is no PM4 to parse.
- The Series S profile drops the ray-traced and hair passes. CTA's Series S is black because `amd_fidelityfx_upscaler_xbox.dll` is refused.

## 2. The architecture

**One home, many caches, decided per block.** Title memory has exactly one home: pagefile-backed sections in RAM. VRAM is a cache with its own valid bits, never a second home. Each 64 KiB block carries counters and a tier, and moves between tiers as its use changes. Work scales with what changed this frame, never with how much memory exists.

```
 title threads ──calls──► word stream (cloned thin recorder)
                              │ 1024-word chunks
                              ▼
                 chunk workers (E-cores): hash ─► reuse? ─► replay cached host list
                              │ no                      
                              ▼                          
                 translate chunk ─► host list + read set + write set (blocks)
                              │
                              ▼
 residency (submit lane):  read sets ∩ invalid-in-VRAM ─► one copy-queue batch, section ─► VRAM
                           write sets ─► mark GPU-owned; copy back on demand
                              │
                              ▼
                 ExecuteCommandLists ─► present (one frame ahead)
```

### 2.1 Memory core (`cta-mem`)
- **Home:** physical pages are 2 MiB `SEC_COMMIT` sections (vilocity's size, five-star's commit mode), committed when first touched (BootTest idea). They are mapped at the title's addresses with `MapViewOfFile3`. A second "beside" view lets our own writes skip the guards (vilocity `land.rs`).
- **GPU access to the home:** sections are opened with `OpenExistingHeapFromFileMapping` in windows under 4095 MiB, each with one placed buffer (ztrx/vilocity `inplace.rs`, Kelvaroq 0063). No `FromAddress`, no tiles.
- **CPU change tracking, cheapest source first:**
  - Private allocations use `MEM_WRITE_WATCH`, with one `GetWriteWatch(RESET)` a frame (cloned).
  - Section views are guarded only while their block is VRAM-cached. Blocks that stay in place are never guarded, because the GPU reads the home directly.
  - Every block gets an epoch stamp, and a resource proved current stays proved until a block under it moves (AnyPS5 proof stamps, port).
- **Tiers per 64 KiB block** (new). Counters are updated from the chunk read and write sets, not by scanning.

| Tier | Rule | Where the GPU reads | Upkeep |
|---|---|---|---|
| In place | read by few draws, under ~512 reads (AnyPS5's NVIDIA limit), or written by the CPU every frame and read once | the home, over PCIe | none: no copy, no guard |
| VRAM cache | read often | a VRAM copy | guard plus a valid bit; re-upload only ranges a chunk reads that went invalid |
| GPU-owned | written by the GPU (render targets, UAV outputs) | VRAM only | the home is protected; on a CPU touch, only changed dwords are copied back |
| Cold | no use for N frames | nothing | VRAM copy evicted (LRU); the home pages may page out |

- **Copy-back without RAM twins** (new variant of AnyPS5's changed-bytes rule): the "before" image of a GPU-owned block lives in VRAM, not in RAM. A compute pass compares now with before, and writes only changed dwords straight into the opened home heap. A CPU store in between survives, and there are no land threads and no readback buffers. It runs lazily: the home pages are no-access, and the first CPU touch triggers it. Blocks the title reads every frame are copied back eagerly at the end of the frame.
- **Budgets enforced by us, not the driver:**
  - VRAM about 6.5 GB, from `QueryVideoMemoryInfo`, with eviction from 75% down to 70% (vilocity).
  - A commit ceiling and a free-RAM floor (five-star flags and CTA's 0014).
  - Promotion is refused under pressure, so a block stays in place instead of the driver silently spilling (the REBAR=1 lesson).

### 2.2 Command path (`cta-stream`)
- **Recording:** cloned from Vice-Tay's thin word stream. The title's lists keep zero put limits, so every command is a vtable call appended as `op|len` plus its arguments.
- **Chunk reuse** (new):
  - Each 1024-word chunk is hashed with xxh3 (~2.4 MB of words a frame, ~0.1 ms).
  - The key is the words plus the bound state at the chunk's start plus the epochs of the blocks in the chunk's last read set.
  - A hit replays last frame's closed host list. Constants and matrices live in title memory, not in the stream, so a camera move changes data, not words.
  - Per-draw ring slots, such as user SGPRs, are double-buffered per chunk so a reused list points at stable addresses.
  - A check mode retranslates a sample of hits and compares. That is the guard against Dolphin's failure (it reused by content alone, then removed the feature).
- **Translation of misses:** chunk workers run on the E-cores and the title stays on the P-cores (vilocity: the title on its own die cut 37.1 to 34.8 ms).
  - Each worker emits the host list and the chunk's read and write sets at block granularity. Per-draw shadow lookups go away.
  - Dirty-state binding and indirect merge are cloned (United `LIST_REUSE`, victoryperf `batch.rs`), as is the batched indirect clamp (vilocity).
- **Submit lane:** it does only three things: union the read sets, issue one upload batch on the copy queue, then execute. The work that made "preparing" 33.7 ms (refresh steps, snapshots, card compare, heap-record waits) does not exist here.
- **Descriptors:** Vice-Tay's 12-byte records in RDNA-sized slots and its inline CBV addresses are cloned. Slot changes come from the `Create*View`/`CopyDescriptors` calls and from guarded heap pages. There is no 470k-slot rescan, and only the used part of a page is published (Kelvaroq 0055 fingerprint).

### 2.3 Textures (`cta-tex`)
- **GPU detiling** (new for these runtimes; Xenia does it): the tiled bytes are copied raw, from the opened home heap straight to VRAM. A compute shader then detiles into the texture. This removes CPU detiling, the 64 MiB staging pools and one RAM copy per texture. The detiler is ported from the addrlib-derived CPU code in `victory-formats`.
- **Change detection:** block epochs instead of per-texture hashes every 8th frame. The 4/12/40/120/400-frame recheck becomes unnecessary.
- **Kept:** pooled placement heaps (vilocity `pool.rs`, 310 to 6 µs per allocation), mips on demand through reserved textures (not through title memory), and 3 GB kept for render targets made budget-aware.

### 2.4 Shaders and pipelines (`cta-shader`, `cta-pso`)
- **Translator:** cloned from `victory-shader` (RDNA2 to SSA, EXEC removal, resource tracing, direct DXIL, no DXC). The 26 wave64 programs with no 32-lane form are a known gap.
- **Cache key = content.** Translated DXIL is stored by its own hash, and the PSO key hashes the DXIL plus the host description, not the translator version or settings (five-star's content store). That ends the 815/855 misses per run.
- **Journal:** the PSOs actually drawn are journaled (`pipelines-used`, seeded from vilocity's 4,259 names). They are rebuilt at boot on half the cores while the title loads (AnyPS5), into a memory-mapped `ID3D12PipelineLibrary1`.
- **On a miss:** a stand-in draws nothing while the real PSO builds (vilocity compat `pso_async.rs`, five-star). Waits are by class: small passes 250 ms, UI and blits 10 s, the lighting chain 60 s. The most-skipped pipelines build first, under a compile-concurrency cap and the RAM budget.

### 2.5 Kept as cloned
`victory-pe`, `victory-loader`, `victory-hle` (xmem rewritten onto `cta-mem`, the rest as-is: files, audio, input, DirectStorage/BCPack, saves), `victory-formats`, 1080p output patching (`VICTORY_OUTPUT`), DRED and the crash reporter. They work and are not where the cost is.

## 3. Telemetry (`cta-feed`), present from the first boot
- **Every frame:** frame, submit and GPU ms; commit and free RAM; VRAM by tier; MB up and back; blocks per tier and promotions; chunks hit, missed and checked; PSO misses and stand-in draws; guard faults.
- **`cta probe`**, a standalone exe with no game:
  - copy-queue speed both ways
  - `EXISTING_HEAPS` support and `FromFileMapping` open cost on the 4060 driver
  - whether `VirtualProtect` still works on a section view while its heap is open
  - GPU read speed from the opened heap against VRAM, per access pattern
  - the 2 MiB map cost
- **Proof:** every change is a 1 s replay A/B, then one 90 s run at 1080p.

## 4. Build order (strangler: the game keeps booting at every step)
0. **Probe.** Its numbers set the in-place read limit, the window size and the budgets. If `FromFileMapping` or protection on opened views fails on this driver, the in-place tier falls back to a copy from a staging pool, and everything else still holds.
1. **Clone and boot.** Vice-Tay copied whole into crates/apps with credit and renamed `cta-*`. Boot to the world at 1080p, Series X profile. This is the baseline. Add `cta-feed`.
2. **Census before rebuilding.** Instrument the cloned stream with two counters:
   - (a) the chunk repeat rate frame to frame, which decides whether chunk reuse is worth building;
   - (b) per-block GPU reads against CPU writes, which sizes the tiers.
   This costs a day and can prove a pillar wrong before it is built.
3. **`cta-mem` home and tiers**, behind `CTA_MEM=1`, with the cloned shadow path as the fallback. Goals: commit under 14 GB, no free-RAM collapse in 90 s, and no 6.5 GB watch scans.
4. **Content-keyed shader cache, journal prebuild, stand-ins.** Goals: no 137 s build, and no draws dropped in the world after the first minute.
5. **Chunk workers with read and write sets, and the slim submit lane** (`CTA_STREAM=1`). Goal: the submit lane under 5 ms.
6. **Chunk reuse with check mode** (`CTA_REUSE=1`), if step 2 found enough repeats.
7. **GPU detile and lazy copy-back.** Goal: copy-back under 3 ms, and no texture staging in RAM.
8. **The Series S profile** (stub the FidelityFX upscaler DLL), for fewer passes on the 4060.

**Targets:** 30 fps in the world at 1080p, with the submit lane under 5 ms, worker translation under 15 ms in parallel, uploads under 4 ms, commit under 14 GB and VRAM under 7 GB.

## 5. What is new here, honestly
- **Placement per block, from use, with migration**, for a console's shared memory: not found in any emulator. The closest prior art is NVIDIA's UVM access counters, which work on GPU reads only and inside the driver.
- **Replaying translated command chunks, keyed on their inputs:** not found. Dolphin's display-list cache keyed on content alone and was removed.
- **A VRAM "before" image plus a compute copy-back into the opened home:** a variant of AnyPS5's changed-bytes rule, without RAM twins.
- **GPU detiling:** done by Xenia, not by any GTA VI runtime.
- **Not new:** the home sections, opened heaps, write watch, stand-ins, the journal and direct DXIL. Those are cloned, with credit.

## 6. Risks
- Opened heaps are "diagnostics only" per Microsoft, and there is no NVIDIA performance data. Step 0 decides, and copy fallbacks keep every tier working.
- A chunk reused with a missed input draws a stale frame. The check mode is always on in debug runs.
- Lazy copy-back puts a GPU round trip on a CPU fault. Blocks read every frame go eager, so steady-state faults stay rare.
- 16 GB RAM: the title alone may need most of it. Cold home pages paging out is the release valve, so watch hard faults in the feed.
