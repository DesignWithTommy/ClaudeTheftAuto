//! CTA's own measurements for PLAN.md step 0, on top of the copied
//! in-place probe: copy speed each way, how fast shaders read each kind of
//! memory (the card's own, a section opened in place, today's shared host
//! copy) once and again and again, and what mapping views of a section
//! costs at 64 KiB and at 2 MiB.

use std::time::Instant;

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::s;
use windows_sys::Win32::System::Memory as mem;

use crate::inplace::{From, Rig, open, place};

type R<T> = windows::core::Result<T>;

fn desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

fn buffer(rig: &Rig, props: D3D12_HEAP_PROPERTIES, size: u64, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> R<ID3D12Resource> {
    let mut b: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe { rig.device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc(size, flags), state, None, &mut b)? };
    b.ok_or_else(windows::core::Error::empty)
}

fn kind(t: D3D12_HEAP_TYPE) -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES { Type: t, ..Default::default() }
}

/// Today's host copy: write-back pages in the PC's memory the card reads.
fn shared_kind() -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_CUSTOM,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
        MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
        ..Default::default()
    }
}

fn gbps(bytes: u64, ms: f64) -> String {
    format!("{:.1} GB/s ({ms:.1} ms)", bytes as f64 / 1e6 / ms)
}

/// Best of three wall-clock runs of one recorded list.
fn best(rig: &mut Rig, record: &dyn Fn(&ID3D12GraphicsCommandList)) -> R<f64> {
    let mut best = f64::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        rig.run(|l| record(l))?;
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(best)
}

pub fn copies(rig: &mut Rig, mb: u64) -> Vec<String> {
    let bytes = mb << 20;
    let mut out = Vec::new();
    let run = |rig: &mut Rig| -> R<Vec<String>> {
        let card = buffer(rig, kind(D3D12_HEAP_TYPE_DEFAULT), bytes, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON)?;
        let up = buffer(rig, kind(D3D12_HEAP_TYPE_UPLOAD), bytes, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ)?;
        let back = buffer(rig, kind(D3D12_HEAP_TYPE_READBACK), bytes, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST)?;
        let mut v = Vec::new();
        // SAFETY: live buffers of `bytes` bytes.
        let ms = best(rig, &|l| unsafe { l.CopyBufferRegion(&card, 0, &up, 0, bytes) })?;
        v.push(format!("copy up, upload heap -> card, {mb} MB: {}", gbps(bytes, ms)));
        let ms = best(rig, &|l| unsafe { l.CopyBufferRegion(&back, 0, &card, 0, bytes) })?;
        v.push(format!("copy back, card -> readback heap, {mb} MB: {}", gbps(bytes, ms)));
        // A committed section opened in place, as the home of title memory.
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        // SAFETY: a section of our own, closed below.
        let handle = unsafe {
            mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, (bytes >> 32) as u32, bytes as u32, std::ptr::null())
        };
        if handle.is_null() {
            v.push("copy with the home: no section".into());
            return Ok(v);
        }
        let heap = open(&rig.device, From::Section(handle as usize));
        let home = match &heap {
            Ok(h) => place(&rig.device, h, 0, bytes),
            Err(e) => Err(e.clone()),
        };
        match home {
            Ok(home) => {
                let ms = best(rig, &|l| unsafe { l.CopyBufferRegion(&card, 0, &home, 0, bytes) })?;
                v.push(format!("copy up, opened section (home) -> card, {mb} MB: {}", gbps(bytes, ms)));
                let ms = best(rig, &|l| unsafe { l.CopyBufferRegion(&home, 0, &card, 0, bytes) })?;
                v.push(format!("copy back, card -> opened section (home), {mb} MB: {}", gbps(bytes, ms)));
            }
            Err(e) => v.push(format!("copy with the home: refused {:#010x} {}", e.code().0, e.message().trim())),
        }
        drop(heap);
        // SAFETY: our handle.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        Ok(v)
    };
    match run(rig) {
        Ok(v) => out.extend(v),
        Err(e) => out.push(format!("copies failed: {:#010x} {}", e.code().0, e.message().trim())),
    }
    out
}

/// Compiles the read shader: every thread sums uint4 loads, `passes` times
/// over a window of `words16` uint4s; the sum is stored only if it hits a
/// magic value, so nothing is optimised away and nothing is written.
fn reader(rig: &Rig) -> R<(ID3D12RootSignature, ID3D12PipelineState)> {
    const SOURCE: &str = "#define RS \"SRV(t0), UAV(u0), RootConstants(num32BitConstants=3, b0)\"\n\
        ByteAddressBuffer src : register(t0);\nRWByteAddressBuffer dst : register(u0);\n\
        cbuffer C : register(b0) { uint mask; uint total; uint threads; }\n\
        [RootSignature(RS)]\n[numthreads(256, 1, 1)]\n\
        void main(uint3 id : SV_DispatchThreadID) {\n\
          uint acc = 0;\n\
          for (uint i = id.x; i < total; i += threads) { uint4 v = src.Load4((i & mask) * 16); acc += v.x ^ v.y ^ v.z ^ v.w; }\n\
          if (acc == 0x7f3a91c5u) dst.Store(0, acc);\n\
        }\n";
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: as in inplace.rs `shade`.
    unsafe {
        D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("reader"), None, None, s!("main"), s!("cs_5_1"), 0, 0, &mut code, None)?;
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = rig.device.CreateRootSignature(0, bytes)?;
        let pso = rig.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
            ..Default::default()
        })?;
        Ok((root, pso))
    }
}

/// How fast shaders read `mb` MB of each kind of memory: once through
/// (data read once, the in-place case) and 16 times over a 16 MB window
/// (hot data, which the plan copies to the card).
pub fn reads(rig: &mut Rig, mb: u64) -> Vec<String> {
    let bytes = mb << 20;
    let mut out = Vec::new();
    let (root, pso) = match reader(rig) {
        Ok(x) => x,
        Err(e) => return vec![format!("read shader failed: {:#010x} {}", e.code().0, e.message().trim())],
    };
    let dst = match buffer(rig, kind(D3D12_HEAP_TYPE_DEFAULT), 65536, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON) {
        Ok(b) => b,
        Err(e) => return vec![format!("read target failed: {e}")],
    };
    let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    // SAFETY: a section of our own, closed at the end.
    let handle = unsafe {
        mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, (bytes >> 32) as u32, bytes as u32, std::ptr::null())
    };
    let heap = (!handle.is_null()).then(|| open(&rig.device, From::Section(handle as usize)).ok()).flatten();
    let mut kinds: Vec<(&str, Option<ID3D12Resource>)> = vec![
        ("the card's own memory", buffer(rig, kind(D3D12_HEAP_TYPE_DEFAULT), bytes, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON).ok()),
        ("a section opened in place (the home)", heap.as_ref().and_then(|h| place(&rig.device, h, 0, bytes).ok())),
        ("today's shared host copy (custom, write-back, L0)", buffer(rig, shared_kind(), bytes, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON).ok()),
    ];
    let threads: u32 = 1024 * 256;
    let groups = threads / 256;
    let all16 = (bytes / 16) as u32;
    let window16 = ((16u64 << 20) / 16) as u32;
    for (name, res) in kinds.drain(..) {
        let Some(res) = res else {
            out.push(format!("read {name}: could not be made"));
            continue;
        };
        // SAFETY: live buffers; the shader reads inside `res` only.
        let gpu = unsafe { res.GetGPUVirtualAddress() };
        let dgpu = unsafe { dst.GetGPUVirtualAddress() };
        let (root, pso) = (&root, &pso);
        let pass = |mask: u32, total: u32| {
            move |l: &ID3D12GraphicsCommandList| unsafe {
                l.SetComputeRootSignature(root);
                l.SetPipelineState(pso);
                l.SetComputeRootShaderResourceView(0, gpu);
                l.SetComputeRootUnorderedAccessView(1, dgpu);
                l.SetComputeRoot32BitConstants(2, 3, [mask, total, threads].as_ptr().cast(), 0);
                l.Dispatch(groups, 1, 1);
            }
        };
        let once = best(rig, &pass(all16 - 1, all16));
        let hot = best(rig, &pass(window16 - 1, window16 * 16));
        let show = |r: R<f64>, b: u64| r.map_or_else(|e| format!("failed {:#010x}", e.code().0), |ms| gbps(b, ms));
        out.push(format!(
            "read {name}: {mb} MB once {}; a 16 MB window 16 times {}",
            show(once, bytes),
            show(hot, 16 * (16u64 << 20))
        ));
    }
    drop(heap);
    if !handle.is_null() {
        // SAFETY: our handle.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    }
    out
}

/// Mapping and unmapping `mb` MB of a committed section in views of
/// 64 KiB and of 2 MiB (vilocity measured 2 MiB sections halving frame time).
pub fn maps(mb: u64) -> Vec<String> {
    let bytes = (mb << 20) as usize;
    let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    let mut out = Vec::new();
    // SAFETY: a section of our own; every view mapped is unmapped.
    unsafe {
        let handle =
            mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, (bytes as u64 >> 32) as u32, bytes as u32, std::ptr::null());
        if handle.is_null() {
            return vec!["maps: no section".into()];
        }
        for size in [64usize << 10, 2 << 20] {
            let n = bytes / size;
            let t = Instant::now();
            let mut views = Vec::with_capacity(n);
            for i in 0..n {
                let off = (i * size) as u64;
                let v = mem::MapViewOfFile(handle, mem::FILE_MAP_WRITE, (off >> 32) as u32, off as u32, size);
                if v.Value.is_null() {
                    break;
                }
                views.push(v);
            }
            let mapped = t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            let count = views.len();
            for v in views {
                mem::UnmapViewOfFile(v);
            }
            let unmapped = t.elapsed().as_secs_f64() * 1000.0;
            out.push(format!(
                "maps of {mb} MB in {} KiB views: {count} views, mapped in {mapped:.1} ms ({:.1} us each), unmapped in {unmapped:.1} ms",
                size >> 10,
                mapped * 1000.0 / count.max(1) as f64
            ));
        }
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }
    out
}
