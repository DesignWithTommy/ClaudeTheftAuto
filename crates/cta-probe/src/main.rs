//! `cta-probe`: PLAN.md step 0. What this PC's card and driver allow and
//! cost for the new memory design, with no game: opening title-like memory
//! in place (copied VI-BR-PORT probe), copy speed both ways, shader read
//! speed per kind of memory, and the cost of mapping views.
//!
//! Usage: `cta-probe [MB]` (default 256). Prints a report; nothing else is
//! touched.

mod inplace;
mod measure;

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

fn main() -> windows::core::Result<()> {
    let mb: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(256);
    // SAFETY: plain factory, adapter and device creation.
    let (adapter, device) = unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)?;
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
        (adapter.cast::<IDXGIAdapter3>()?, device.ok_or_else(windows::core::Error::empty)?)
    };
    // SAFETY: plain queries that fill our structures.
    let desc = unsafe { adapter.GetDesc1()? };
    let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_owned();
    println!("cta-probe: {name}, {} MB own memory, {} MB shared; test size {mb} MB", desc.DedicatedVideoMemory >> 20, desc.SharedSystemMemory >> 20);
    let memory = || -> Option<[u64; 4]> {
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: as above.
        unsafe {
            adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own).ok()?;
            adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared).ok()?;
        }
        Some([own.Budget, own.CurrentUsage, shared.Budget, shared.CurrentUsage])
    };
    if let Some(m) = memory() {
        println!("budget: card {} MB (in use {}), PC memory shared with it {} MB (in use {})", m[0] >> 20, m[1] >> 20, m[2] >> 20, m[3] >> 20);
    }
    let mut existing = D3D12_FEATURE_DATA_EXISTING_HEAPS::default();
    let mut o16 = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
    // SAFETY: plain queries that fill our structures.
    unsafe {
        let a = device.CheckFeatureSupport(D3D12_FEATURE_EXISTING_HEAPS, (&raw mut existing).cast(), size_of_val(&existing) as u32);
        let b = device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS16, (&raw mut o16).cast(), size_of_val(&o16) as u32);
        println!(
            "features: existing heaps {} ({}), GPU upload heaps (ReBAR) {} ({})",
            existing.Supported.as_bool(),
            if a.is_ok() { "asked" } else { "not answered" },
            o16.GPUUploadHeapSupported.as_bool(),
            if b.is_ok() { "asked" } else { "not answered" }
        );
    }
    println!("\n== in place (VI-BR-PORT's probe) ==");
    for line in inplace::probe(&device, &memory, mb as usize).lines {
        println!("{line}");
    }
    let mut rig = inplace::Rig::new(&device)?;
    println!("\n== copies ==");
    for line in measure::copies(&mut rig, mb) {
        println!("{line}");
    }
    println!("\n== shader reads ==");
    for line in measure::reads(&mut rig, mb) {
        println!("{line}");
    }
    println!("\n== views of a section ==");
    for line in measure::maps(mb.max(512)) {
        println!("{line}");
    }
    Ok(())
}
