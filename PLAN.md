# ClaudeTheftAuto: plan for a new runtime

Target rig: RTX 4060 8 GB, i5-14400F, 16 GB RAM, 1920x1080 output. GPL-3.0; parts cloned from other runtimes keep their credit (CREDITS.md).

## What has been tried (and what it cost)
| Approach | Who | Cost |
|---|---|---|
| Host shadow copies + page watching | VIctory family, Vice-Tay | Memory is held twice; 22 GB private, so RAM collapses at 60-70 s |
| Shaders read title memory in place | vilocity compat | Fastest (33 fps on a 5080) but wants a big card; the 8 GB card spills |
| Pagefile sections + a page table shaders read | GTA6-BootTest | Zero-copy, but every memory access in every shader goes through the table |
| ReBAR single-copy heap | Vice-Tay switch | 0.3 fps on 8 GB (VRAM over budget) |
| Translate shaders on first use, compile live | almost all | Compile stalls; vilocity hides them with stand-ins that draw nothing |
| Re-translate every command list every frame | all | CPU submit 91 ms a frame on our rig |

Every one of these treats memory as one flat space with one policy. The rig has 8 GB of VRAM against a console's 16 GB of unified memory, so that one policy is always wrong for part of the data.

## New architecture: tiered residency plus incremental frame translation
Neither half has been tried by the other runtimes.

### 1. Residency by observed use (the memory core)
- Each title allocation becomes a tracked region with a tier: **GPU-only** (render targets, written only by the GPU, live in VRAM, no host copy), **CPU-written / GPU-read** (upload ring in system memory, streamed in), **shared** (both read and write, the only tier that gets a copy and page watching), **cold** (not touched for N frames, evicted to disk-backed sections, so 16 GB RAM stays under budget).
- A region's tier is learned from the first frames (write-watch plus command-list references) and changes when its use changes. One copy per region, never two, except in the shared tier.
- A fixed VRAM budget (e.g. 6.5 GB on the 4060) is enforced by the runtime, not the driver, so there is no silent spill.
- Clone: BootTest's commit-on-touch exception handler, vilocity's in-place reads for the GPU-only tier, Vice-Tay's card-memory meters.

### 2. Incremental frame translation (the CPU core)
- The title's command lists are hashed in chunks. An unchanged chunk reuses its translated D3D12 command bundle from last frame; only changed chunks are translated again. Most of the 91 ms submit is repeated work.
- Translation runs one frame behind on worker threads (a pipeline: decode -> translate -> submit), so the game thread never waits on it.
- Clone: Vice-Tay's next-frame prep and its PE loader/HLE, PercyScarlett's import routing.

### 3. Ahead-of-time shaders
- All of the title's GPU programs are translated offline into a persistent, content-keyed cache (five-star-recomp compiled all 25,908). The run only loads them. A miss gets a stand-in that draws nothing (vilocity), and the real program is built in the background.
- Clone: five-star-recomp's structured control-flow recovery and fast DXIL backend with a dxc fallback; WARP instruction checks run offline, not live.

### 4. Measured from day one
- Every subsystem reports frame time, memory per tier and VRAM budget into one shared-memory feed; one 90 s run at 1080p is the test.

## Order
1. Skeleton: loader + HLE cloned from Vice-Tay, booting to the first Present (no translation changes yet).
2. Telemetry feed (step 4) so every later step is measured.
3. Residency tiers with a learned classifier, behind a switch, against the flat copy as the fallback.
4. Offline shader cache plus stand-ins.
5. Incremental command-list translation.
6. Goal: in the world at 1080p without a RAM collapse, then 30 fps.
