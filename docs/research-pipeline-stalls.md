# Pipeline stalls and block uploads: research

The numbers point to synchronous compiles: 270–680 new PSOs in a 12–24 s frame is about 35 ms per PSO (DXC plus the driver), with 0.2 s of CPU prep and ≤14 ms of GPU time. The order below is by expected impact.

## 1. Compile PSOs off the render thread and skip or substitute the draw (largest gain)
- **How it works:** a draw that misses the cache queues {DXC → CreatePipelineState} on a worker pool and returns at once. yuzu skips the draw until the pipeline exists. Dolphin's "skip drawing" mode does the same. KytyPS5 added a PsoCacheManager.
- **Gain:** KytyPS5 PR 2119 cut main-thread lock time from 20.2 s to 1.8 s (−91%, the author's own measurement). This is the same shape as our stall.
- **Sources:**
  - https://yuzu-mirror.github.io/entry/yuzu-progress-report-jul-2021
  - https://en.gamegpu.com/news/igry/emulyator-ps5-kytyps5-sokratil-zaderzhki-kompilyatsii-shejderov-na-91
- **For us:** this alone should bring a world frame to about the 0.2 s CPU floor (~5 fps). Use 12–14 workers on the 14400F (DXC instances are thread-safe, one compiler per thread). Skip draws that write to the depth or shadow passes for only a frame or two. Order the queue by first use.

## 2. Persistent caches: DXIL blobs plus ID3D12PipelineLibrary
- **How it works:** cache the DXIL from each translated shader on disk, keyed on a hash of the guest microcode. Store the finished PSOs with `StorePipeline` and `Serialize`, and on a hit call `LoadPipeline` with the same stream desc.
  - Corrupt blobs or blobs from an older driver return defined errors, so rebuild on those.
  - The driver also keeps its own cache (D3D12 shader cache spec).
- **Gain:** a warm run does no compiles at all. Ryujinx and Xenia ship disk shader caches for this reason.
- **Sources:**
  - https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nn-d3d12-id3d12pipelinelibrary1
  - https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device1-createpipelinelibrary
  - https://microsoft.github.io/DirectX-Specs/d3d/ShaderCache.html
- **For us:** split the two layers. DXIL depends only on the shader, while PSO keys are shader plus state. One DXIL serves many PSOs, so most of the 270–680 misses should then cost only the driver step.

## 3. Prebuild in Fossilize style
- **How it works:** record every PSO desc a run creates, then replay them all on a background pool at the next launch or on a `--prewarm` pass, before the world loads.
- **Gain:** Steam compiles Vulkan pipelines during the download using this method. Replay can take a long time and use a lot of memory, so limit the threads.
- **Sources:**
  - https://github.com/ValveSoftware/Fossilize
  - https://devblogs.microsoft.com/directx/introducing-advanced-shader-delivery/ (Microsoft's own D3D12 prebuild)
- **For us:** one walk through the world gives a replay list, and later runs start warm.

## 4. Ubershader or interpreter stand-in (Dolphin hybrid)
- **How it works:** a few generic shaders, compiled ahead of time, interpret the state that the specialized shader would bake in. They render the draw while the specialized PSO compiles in the background, then hand over to it.
- **Gain:** removes stutter with no missing draws, at a high GPU cost (exclusive mode is "extremely" heavy).
- **Sources:**
  - https://dolphin-emu.org/blog/2017/07/30/ubershaders/
  - https://bugs.dolphin-emu.org/issues/12646
- **For us:** this is a lot of work for Xbox Series shaders, which are arbitrary programs, not fixed-function TEV state. An ubershader is realistic only for the fixed state: blend, depth, render-target formats, and vertex fetch. Build that as a second step after #1.

## 5. Compile fast first, optimize later
- **How it works:** build the first version with DXC `-O0` (or `-O1`) and swap in an `-O3` build later at low priority.
- **Caveat:** DXC runs some passes even at O0, such as resource-handle propagation and unrolling, and O0 can fail validation, so time and validate both levels on our shaders. A Clang-built DXC compiles about 10% faster, and up to 20–30% faster on some shaders.
- **Sources:**
  - https://llvm.github.io/wg-hlsl/meetings/2025-07-31
  - https://devblogs.microsoft.com/directx/dxc-1-8-2405-available
  - https://therealmjp.github.io/posts/dxil-linking (DXIL linking, which lets shared library code compile once)

## 6. Graphics pipeline libraries (VK_EXT_GPL) and their D3D12 limits
- **How it works:** pre-rasterization, fragment, and output-interface pieces are compiled separately and then fast-linked. DXVK 2.0 compiles at shader load instead of at draw time, so its state caches dropped from thousands of entries to dozens.
- **Sources:**
  - https://registry.khronos.org/vulkan/specs/1.3-extensions/man/html/VK_EXT_graphics_pipeline_library.html
  - https://www.dsogaming.com/news/dxvk-2-0-released-aiming-to-minimize-shader-compilation-stutters-in-dx11-dx12-games/
  - https://www.phoronix.com/news/VKD3D-Proton-2.9 (vkd3d-proton uses it only in "extreme edge cases")
- **For us:** D3D12 has no graphics fast-link, so the nearest equivalent is the DXIL-per-stage cache in #2. Even vkd3d-proton gets only a small gain from GPL, so a rewrite toward it isn't worth the effort.

## Uploads (the CPU floor after the compile fix)
- **Batch dirty blocks:**
  - Merge adjacent dirty 64 KiB blocks into runs, so each run gets one CopyBufferRegion.
  - Keep dirty state in a bitmap per page set and scan it with word-wide ops, not one map entry per block.
  - Flush once per submit, not once per draw.
- **Write watch vs guard pages:**
  - `GetWriteWatch` gives the written pages in one call with no per-write fault, but only on memory allocated `MEM_WRITE_WATCH`, and query plus reset races with writers (use `WRITE_WATCH_FLAG_RESET`).
  - Guard pages (`VirtualProtect`) fault on every first write, and Wine's segfault fallback showed how expensive that is.
  - `D3D12_HEAP_FLAG_ALLOW_WRITE_WATCH` forces GPU_UPLOAD heaps into system RAM.
  - Sources: https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-getwritewatch, https://list.winehq.org/hyperkitty/list/wine-gitlab@list.winehq.org/message/GF7R6WWUFVZKC4RO6ROGVCMRIB4ILVCF/, https://devblogs.microsoft.com/pix/pix-and-id3d12manualwritetrackingresource/
- **Persistent upload ring:**
  - Keep one mapped UPLOAD buffer and recycle it by fence. Size it for the worst frame.
  - Copy queue → fence → direct queue.
  - Sources: https://learn.microsoft.com/en-us/windows/win32/direct3d12/fence-based-resource-management, https://learn.microsoft.com/en-us/windows/win32/direct3d12/large-buffers
- **Copy queue and GPU_UPLOAD (ReBAR):**
  - The copy queue gets the best PCIe throughput, but use the graphics or compute queue for VRAM-to-VRAM copies.
  - GPU_UPLOAD lets the CPU write straight to VRAM with no copy. Never read it from the CPU, since it is write-combined. Strided writes are about 2x slower.
  - Sources: https://gpuopen.com/learn/using-d3d12-heap-type-gpu-upload/, https://developer.nvidia.com/blog/optimizing-dx12-resource-uploads-to-the-gpu-using-cpu-visible-vram
- **For us:** batched runs plus GetWriteWatch remove the per-block bookkeeping, and a ring on the copy queue moves the copies off the critical path.

## Expected path
1. #1 (async plus skip) takes 0.1 fps to about 5 fps.
2. #2 and #3 (caches plus prewarm) remove the rest of the misses.
3. Batched uploads and the ring lower the 0.2 s CPU floor toward playable.

No benchmarks exist for O0 vs O3 or for write watch vs guard pages, so measure both locally.
