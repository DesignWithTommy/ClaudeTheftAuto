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

**On our rig** (VI-BR-PORT `2954325`, the author's 8 GB preset, 150 s, `6ixlaunch/runs/vibrport-base-165723.log`):
- Menus and loading screens ran at 30 fps, with 9.7 GB private before the world.
- The world collapsed to 0.0-0.3 fps, up to 28 s a frame. Private was 15.4 GB, free RAM 0.3-1 GB, and the overlay showed 29.5 GB committed.
- Each frame did 9,042 refreshes, 6.8 GB checked. 5,293 draws were dropped for want of a pipeline, with 2,580 still being made.

So the title itself fits in about 10 GB. The other ~5.7 GB, plus the pipelines it never had time to build, is the runtime's own overhead.

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
- **GPU access to the home, two transports:**
  - **Opened windows** (default when the probe passes): sections are opened with `OpenExistingHeapFromFileMapping` in windows under 4095 MiB, each with one placed buffer (ztrx/vilocity `inplace.rs`, Kelvaroq 0063). No `FromAddress`, no tiles.
  - **Upload ring** (fallback, always built): a fixed 256 MB upload heap. The CPU copies the blocks a batch reads from the home into it, then the copy queue moves them to VRAM. It is bounded and reused every frame, so it is never a second home.
  - The RAM saving comes from the single home, not from in place, so it holds with either transport.
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

## Step 0 results (`cta-probe 256` on the rig, driver as installed)

| Test | Result |
|---|---|
| `EXISTING_HEAPS`, GPU upload heaps (ReBAR) | both supported |
| `OpenExistingHeapFromFileMapping`, `SEC_COMMIT` | ok: 17 ms per 256 MB. A `SEC_RESERVE` section is refused |
| CPU and GPU see each other's writes in the opened home | yes, both ways, through views and after a view is remapped |
| View made read-only while its section heap is open | **works**, so guards stay possible. `FromAddress` blocks it, so it's not used |
| Opening a 256 MB section | +256 MB working set and +256 MB of the card's **non-local budget (7,437 MB)** |
| Copy speed: upload heap / opened home -> card / card -> home | 12.5 / 12.2 / 13.0 GB/s. The home is as fast a copy source as a staging heap, so no staging is needed |
| Shader reads, 256 MB read once: VRAM / home / today's shared copy | 247 / 11.9 / 11.6 GB/s |
| Shader reads, 16 MB read 16 times: VRAM / home / shared | 1,405 / 134 / 176 GB/s |
| Mapping 512 MB in 64 KiB / 2 MiB views | 5.4 + 8.8 ms / 0.2 + 0.3 ms (same ~0.75 µs per view; 32x fewer views) |

**What this changes:**
- **The in-place tier is real.** Data read once costs the same in place as a copy (~12 GB/s either way), with no second copy in RAM.
- **Hot data must be in VRAM.** Repeated reads are 10x faster there (1,405 vs 134 GB/s). The tiers in 2.1 stand.
- **The whole home cannot stay open.** Each opened byte counts against the 7.4 GB non-local budget and the working set. The home is opened in 64-256 MB windows on demand, LRU, under that budget (AnyPS5 import windows, five-star `--section-heap-budget`). Blocks outside an open window are copied to VRAM instead of read in place.
- **2 MiB sections are confirmed.**

## 4. Why it should work

Each reason rests on a measurement, not on hope.
1. **The collapse is the runtime's overhead, not the game.** Under VI-BR-PORT, menus and loading ran at 30 fps on 9.7 GB. The world died when three things piled on top of the title:
   - about 5.7 GB of the runtime's own copies: write-combined 5,607 MB, other private 2,183 MB, heap segments 1,829 MB and opened windows 1,783 MB, reserved;
   - 6.8 GB of checks a frame;
   - 2,580 pipelines still being made.
   One home removes the copies. Read sets replace the scans. A journal builds the pipelines before the world.
2. **The title touches far less than it reserves.** In the world, its page pool held 3,977 MB with 0 MB resident, its heaps 3,069 MB (628 resident) and its views 6,160 MB (578 resident). A section committed at first touch pays only for touched 2 MiB pieces.
3. **The bus has room.** A frame needs about 3.5 GB/s at 30 fps. The probe measured 12-13 GB/s both ways.
4. **The primitives work on this driver** (step 0). Sections open as heaps. Guards still work on opened views. The home is as fast a copy source as an upload heap, so no staging copy is needed. Data read once costs the same in place as copied.
5. **The hard, working parts are cloned, not rewritten:** the PE loader, HLE, the shader translator, the thin recorder and descriptors. New code covers only memory placement, the submit lane and reuse, which are the parts that fail.
6. **Nothing can fall below the baseline.** Each pillar ships behind a switch, with the cloned path as the fallback. A pillar that loses its A/B is switched off, not debated.

## 5. Why it might not, and what decides

Ranked by likelihood times damage. Each risk gets a gate measured in the baseline census (step 2), a pass line and a fallback.

| # | Risk | Gate (measured in the census) | Pass | If it fails |
|---|---|---|---|---|
| R1 | **16 GB cannot hold the title plus Windows.** Idle, this PC has only 5.8 GB free of 16.2 GB with the usual apps open. In-world resident figures were shrunk by trimming, so the title's true need is unknown. | G1: title-owned commit and resident (VirtualQuery map at 30 s and 90 s in the world, background apps closed), apart from runtime-owned memory | title resident ≤ 7 GB, and projected free RAM ≥ 1.5 GB once runtime copies are gone | Series S profile, a smaller texture pool with top mips dropped, and cold home pages left to page out (watch hard faults). If the title alone needs more than ~9 GB, 16 GB cannot hold it, and we say so plainly (32 GB is the fix). |
| R2 | **The 14400F is too slow even with zero overhead.** Vice-Tay used 624 CPU-seconds a minute on a 9950X3D (about 10 cores busy). The title is native x64, and its own threads may need the whole CPU. | G3: CPU ms a frame per thread class (title vs runtime), and the title's longest thread | title critical path ≤ 25 ms | 25-40 ms: target 20-25 fps with steady pacing. Over 40 ms: the cap is the game's own CPU. The design cannot fix that, and we stop chasing fps. |
| R3 | **Chunks rarely repeat.** Ring-allocated constant addresses, per-frame descriptor indices or counters in the words would change every hash. | G2: share of chunk bytes repeated frame to frame, raw and with ring addresses made relative | ≥ 50% (normalized counts) | Skip reuse (step 7). Parallel translation on the E-cores still takes the work off the submit lane. |
| R4 | **8 GB of VRAM is too small.** Render targets, hot textures and hot buffers may exceed ~6 GB, and driver spill is a cliff (vilocity: card share 6 ms became 64-122 ms). | G4: VRAM per tier, Series S, 1080p | GPU working set ≤ 6 GB | Mips on demand, top-mip drop, and our own eviction holding 70-75% of budget. Promotion is refused before the driver spills. |
| R5 | **In place is slower in the game than in the probe.** Scattered reads, atomics or GPU writes to system RAM are slow (five-star's own finding). Opening a window costs 17 ms per 256 MB. | per-block read counters from the census, plus an A/B of in-place against copy | in-place blocks under ~512 reads; window churn off the submit lane | Move everything to the VRAM-cache tier: vilocity's design without RAM shadows. In place is a saving, not a load-bearing pillar. |
| R6 | **Opened heaps are "diagnostics only".** They may be a slow path, may break with a driver update, or may fail under load. | the step 0 probe, re-run after each driver update; device-removed count in the feed | no removals; probe speeds held | The upload-ring transport (2.1). It still has one home, so the RAM saving holds. |
| R7 | **Copy-back is too frequent.** Lazy copy-back takes a GPU round trip on a CPU fault. | G5: blocks written by the GPU then read by the CPU, per frame, and their bytes | under ~200 faults a frame | Copy back eagerly at the end of the frame for those blocks. Hash-compare whole pieces (cloned) for buffers the CPU never writes, so the VRAM before-image is kept only where it is needed. |
| R8 | **The new parts draw wrong.** Stale reuse, or a GPU detile that mismatches. | check mode: retranslate a sample of reused chunks; compare GPU detile with the cloned CPU detile, per format | zero mismatches over a 90 s run | Turn the part off by its switch. A detile mismatch falls back to CPU detile into a pooled ring, for that format only. |
| R9 | **The first prewarm is long.** Five-star took about 35 min for about 3,850 pipelines. | prewarm time on the 6P+4E, and stand-in draws in the world | stand-ins near zero after the first minute | A one-time cost. Stand-ins keep frames moving while it finishes, and later boots load the pipeline library. |
| R10 | **Effort stalls.** VI-BR-PORT is a large tree, and the four new pieces are real work. | — | every step lands with a run and a number | The strangler order below. The smallest step with the biggest RAM win goes first, and a minimum win is defined before any novel part. |

**Minimum win.** Suppose reuse, in place and GPU detiling all fail. The single home with the upload ring, read sets in place of scans, and prebuilt pipelines still remove what killed the world. The bar for that: stable 20 fps at 1080p Series S, no free-RAM collapse in 90 s, and commit under 14 GB. 30 fps is the stretch goal, and only if G3 passes.

**Stop lines** (said plainly, not worked around):
- G1 shows the title alone needs more than ~9 GB resident.
- G3 shows the title's own threads need more than 40 ms a frame.
Either one means this PC caps the game, not the translator.

## 6. Build order (strangler: the game keeps booting at every step)
0. **Probe.** Done (results above). It sets the in-place read limit, the window size and the budgets.
1. **Clone and boot.** M1LK (VIctory + DexNom lineage, GPL-3.0) copied whole into crates/apps with credit. Boot to the world at 1080p. Add `cta-feed` with the census counters G1-G5 from the first boot.
   - **Why M1LK.** Every runtime in the catalog (about 50) was weighed for this rig, which has 16 GB of RAM, not for its best fps on a big card.
     - M1LK's reference PC is nearly ours: an RTX 4060 Laptop 8 GB, an i5-12500H and 16 GB of RAM.
     - It holds there: a 4.5 fps walking median, no crash across tests 14-35, 20-minute sessions, and a 15.8-17.8 GB peak.
     - It has 35 logged A/B tests on that machine, so the census starts with real numbers.
   - **Its frame is our target.** A 215 ms frame is about 130-140 ms of CPU preparing (72-86 ms of it copying title memory), about 30 ms of copy-back and about 20 ms of GPU. That is the overhead this design removes.
   - **Why not the others:**

     | Runtime | Why not |
     |---|---|
     | VI-BR-PORT (24 fps, 4060 Ti) | Collapsed here: 0.0-0.3 fps, 15.4 GB private |
     | AnyXboxX (24 fps, 4070 SUPER) | Collapsed here: 0.1-0.4 fps, 21.5 GB private, 5.1 GB of VRAM spilled |
     | Vice-Tay `-Machine laptop` (16 GB profile) | 548-1,150 ms world frames |
     | Neptune | Its own README says 18-21 GB private |
     | ruff-lab | Tuned for AMD (RX 6600 XT) |
     | Leonida | Black shadows; measured on 32 GB |
     | five-star | Black world, about 1 s a frame |
     | vilocity | Needs 22 GB |

   - **Lessons it already paid for:**
     - In place made walking slower in M1LK (test 16: 2.6 against 4.5 fps), and so did GPU detile (test 25: 1.3 fps). R5 and R8 are real, which is why those parts stay behind switches after checkpoint M.
     - In-place *constants* helped (test 22: 4.9 fps), as did batching the indirect clamps (test 27).
   - **Series S first.** Every gate is measured under it (M1LK tests 8-10 fixed its device removal and black picture).
2. **Census.** One instrumented 90 s world run, background apps closed. G1-G5 and the R5 counters go into a table here. Each gate's verdict decides whether its step below is built, changed or dropped. This costs a day and can prove a pillar wrong before it is written.
3. **`cta-mem`, ring transport** (`CTA_MEM=1`). One home in 2 MiB sections, with the VRAM-cache and GPU-owned tiers. The cloned translator emits block read and write sets instead of per-range refreshes. No in place yet.
4. **Content-keyed shader cache, journal prebuild, stand-ins.**
   - **Checkpoint M.** Steps 3 and 4 make the minimum win. Run, then compare with the bar. If it misses, the census says why (R1, R2 or R4) before anything novel is built.
5. **In-place tier and opened windows** (`CTA_INPLACE=1`), A/B against the ring (R5, R6).
6. **Chunk workers on the E-cores and the slim submit lane** (`CTA_STREAM=1`). Goal: submit under 5 ms.
7. **Chunk reuse with check mode** (`CTA_REUSE=1`), only if G2 passed.
8. **GPU detile and lazy copy-back** (`CTA_GPUDETILE=1`, `CTA_LAZYBACK=1`), shaped by G5. Goal: copy-back under 3 ms, and no texture staging in RAM.

**Targets.**
- Bar (checkpoint M): 20 fps stable, commit under 14 GB, VRAM under 7 GB.
- Stretch (after step 8): 30 fps, submit under 5 ms, worker translation under 15 ms in parallel, uploads under 4 ms.

## 7. What is new here, honestly
- **Placement per block, from use, with migration**, for a console's shared memory: not found in any emulator. The closest prior art is NVIDIA's UVM access counters, which work on GPU reads only and inside the driver.
- **Replaying translated command chunks, keyed on their inputs:** not found. Dolphin's display-list cache keyed on content alone and was removed.
- **A VRAM "before" image plus a compute copy-back into the opened home:** a variant of AnyPS5's changed-bytes rule, without RAM twins.
- **GPU detiling:** done by Xenia, not by any GTA VI runtime.
- **Not new:** the home sections, opened heaps, write watch, stand-ins, the journal and direct DXIL. Those are cloned, with credit.
- **The minimum win needs none of the new parts.** That is deliberate: the new parts are upside, not the foundation.
