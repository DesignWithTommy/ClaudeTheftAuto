//! the title's upscaler (ffx-api) on the host: a pluggable backend.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

// inputs are read by compute, the output written through unordered access
pub const INPUT_STATE: u32 = 0x40 | 0x80;
pub const OUTPUT_STATE: u32 = 0x8;

/// what the title asked for when it made its context
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpscaleSetup {
    pub max_render: [u32; 2],
    pub max_output: [u32; 2],
    // ffx create flags as given (hdr 0x1, depth inverted 0x8, depth infinite 0x10, ...)
    pub flags: u32,
}

/// one frame's upscale, title resources already moved to host copies
pub struct UpscaleFrame<'a> {
    pub colour: &'a ID3D12Resource,
    pub depth: Option<&'a ID3D12Resource>,
    pub motion: Option<&'a ID3D12Resource>,
    pub exposure: Option<&'a ID3D12Resource>,
    pub reactive: Option<&'a ID3D12Resource>,
    pub transparency: Option<&'a ID3D12Resource>,
    pub output: &'a ID3D12Resource,
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub render_size: [u32; 2],
    pub output_size: [u32; 2],
    pub sharpen: Option<f32>,
    pub frame_ms: f32,
    pub pre_exposure: f32,
    pub reset: bool,
    pub near: f32,
    pub far: f32,
    pub fov_y: f32,
    // ffx dispatch flags as given
    pub flags: u32,
}

/// a host upscaler; inputs arrive in INPUT_STATE, output in OUTPUT_STATE, leave them so
pub trait Upscaler: Send {
    fn name(&self) -> &'static str;
    // called on context create and when sizes change; false refuses the context
    fn setup(&mut self, device: &ID3D12Device, setup: &UpscaleSetup) -> bool;
    // records into the title's host list; false skips the frame (output untouched)
    fn dispatch(&mut self, device: &ID3D12Device, list: &ID3D12GraphicsCommandList, frame: &UpscaleFrame) -> bool;
    // the context is gone; drop size-bound state
    fn release(&mut self) {}
}

static BACKEND: Mutex<Option<Box<dyn Upscaler>>> = Mutex::new(None);
static FAILS: AtomicU32 = AtomicU32::new(0);

/// installs a backend (dlss); without one the spatial backend is used
pub fn set_backend(backend: Box<dyn Upscaler>) {
    tracing::info!(backend = backend.name(), "upscaler backend set");
    *BACKEND.lock().unwrap_or_else(|e| e.into_inner()) = Some(backend);
}

// VICTORY_UPSCALER=dlss (and VICTORY_DLSS naming a mode) else spatial. DLSS is bound at run time (see `dlss`).
fn chosen_backend() -> Box<dyn Upscaler> {
    if chosen() == "dlss" {
        match crate::dlss::DlssBackend::from_env() {
            Some(dlss) => return Box::new(dlss),
            None => tracing::warn!("M1LK_UPSCALER=dlss without M1LK_DLSS=quality|balanced|performance|ultra; spatial instead"),
        }
    }
    Box::new(Spatial::default())
}

fn with_backend<R>(f: impl FnOnce(&mut dyn Upscaler) -> R) -> R {
    let mut slot = BACKEND.lock().unwrap_or_else(|e| e.into_inner());
    f(slot.get_or_insert_with(chosen_backend).as_mut())
}

static RELEASED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// the title made a context; a released backend lets go only now, its lists long run
pub fn setup(setup: &UpscaleSetup) -> bool {
    let Some(host) = crate::host::host() else { return false };
    with_backend(|b| {
        if RELEASED.swap(false, Ordering::Relaxed) {
            b.release();
        }
        let ok = b.setup(&host.device, setup);
        tracing::info!(backend = b.name(), ?setup, ok, "upscaler context");
        ok
    }) || fall_back_to_spatial(setup)
}

/// DLSS could not start (no NVIDIA card, no DLL, old driver): the title still
/// gets an upscale, a bilinear one, rather than none.
fn fall_back_to_spatial(setup: &UpscaleSetup) -> bool {
    let Some(host) = crate::host::host() else { return false };
    let mut slot = BACKEND.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_none_or(|b| b.name() != "dlss") {
        return false;
    }
    tracing::warn!("DLSS is unavailable; the spatial upscaler serves instead");
    let mut spatial: Box<dyn Upscaler> = Box::new(Spatial::default());
    let ok = spatial.setup(&host.device, setup);
    *slot = Some(spatial);
    ok
}

/// the title destroyed its context; lists in flight may still use the backend's state
pub fn release() {
    RELEASED.store(true, Ordering::Relaxed);
}

/// which backend serves: VICTORY_UPSCALER=spatial|dlss|amd, else amd when its dll is there
pub fn chosen() -> String {
    std::env::var("VICTORY_UPSCALER").map(|v| v.trim().to_ascii_lowercase()).unwrap_or_default()
}

/// whether the title's upscaler goes to a backend here rather than amd's pc dll
pub fn ours() -> bool {
    matches!(chosen().as_str(), "spatial" | "dlss")
}

/// records one upscale into the host list (inside `external::record`)
pub fn dispatch(list: &ID3D12GraphicsCommandList, frame: &UpscaleFrame) -> bool {
    let Some(host) = crate::host::host() else { return false };
    let ok = with_backend(|b| b.dispatch(&host.device, list, frame));
    if !ok && FAILS.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::warn!(render = ?frame.render_size, output = ?frame.output_size, "upscale backend skipped a frame");
    }
    ok
}

// typeless host copies need a typed view
fn typed(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format {
        DXGI_FORMAT_R16G16B16A16_TYPELESS => DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_B8G8R8A8_TYPELESS => DXGI_FORMAT_B8G8R8A8_UNORM,
        DXGI_FORMAT_R10G10B10A2_TYPELESS => DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_R32G32B32A32_TYPELESS => DXGI_FORMAT_R32G32B32A32_FLOAT,
        DXGI_FORMAT_R32_TYPELESS => DXGI_FORMAT_R32_FLOAT,
        DXGI_FORMAT_R16G16_TYPELESS => DXGI_FORMAT_R16G16_FLOAT,
        DXGI_FORMAT_UNKNOWN => return None,
        f if f.0 >= DXGI_FORMAT_R24G8_TYPELESS.0 && f.0 <= DXGI_FORMAT_X24_TYPELESS_G8_UINT.0 => return None,
        f => f,
    })
}

const SPATIAL: &str = r#"#define RS "RootConstants(num32BitConstants=4, b0), DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)"
cbuffer c : register(b0) { uint2 render; uint2 output; };
Texture2D<float4> src : register(t0);
RWTexture2D<float4> dst : register(u0);
SamplerState lin : register(s0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (any(id.xy >= output)) return;
    float w, h;
    src.GetDimensions(w, h);
    float2 uv = (id.xy + 0.5) / float2(output) * float2(render) / float2(w, h);
    dst[id.xy] = src.SampleLevel(lin, uv, 0);
}
"#;

const SLOTS: u32 = 512;

/// the stand-in until dlss: a bilinear stretch of colour to the output
#[derive(Default)]
pub struct Spatial {
    made: Option<(ID3D12RootSignature, ID3D12PipelineState, ID3D12DescriptorHeap, u32)>,
    next: u32,
    broken: bool,
}

impl Spatial {
    fn make(&mut self, device: &ID3D12Device) -> windows::core::Result<()> {
        if self.made.is_some() {
            return Ok(());
        }
        let code = crate::host::compile(SPATIAL, s!("main"), s!("cs_5_1"))?;
        // SAFETY: plain creation from bytes that hold the root signature
        let made = unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &code)?;
            let pso: ID3D12PipelineState = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                ..Default::default()
            })?;
            let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: SLOTS,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
            let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            (root, pso, heap, step)
        };
        self.made = Some(made);
        Ok(())
    }
}

impl Upscaler for Spatial {
    fn name(&self) -> &'static str {
        "spatial"
    }

    fn setup(&mut self, device: &ID3D12Device, _: &UpscaleSetup) -> bool {
        if let Err(e) = self.make(device) {
            tracing::warn!(error = %e, "spatial upscaler not made");
            self.broken = true;
        }
        !self.broken
    }

    fn dispatch(&mut self, device: &ID3D12Device, list: &ID3D12GraphicsCommandList, f: &UpscaleFrame) -> bool {
        if self.broken || self.make(device).is_err() {
            return false;
        }
        let Some((root, pso, heap, step)) = &self.made else { return false };
        // SAFETY: live host copies
        let (cd, od) = unsafe { (f.colour.GetDesc(), f.output.GetDesc()) };
        if od.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return false;
        }
        let (Some(cf), Some(of)) = (typed(cd.Format), typed(od.Format)) else { return false };
        let slot = self.next % (SLOTS / 2) * 2;
        self.next = self.next.wrapping_add(1);
        // SAFETY: our heap and slots; an open list; live resources
        unsafe {
            let cpu = heap.GetCPUDescriptorHandleForHeapStart().ptr + (slot * step) as usize;
            let gpu = heap.GetGPUDescriptorHandleForHeapStart().ptr + u64::from(slot * step);
            let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: cf,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() },
                },
            };
            device.CreateShaderResourceView(f.colour, Some(&srv), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu });
            let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: of,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_UAV::default() },
            };
            device.CreateUnorderedAccessView(f.output, None, Some(&uav), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu + *step as usize });
            let out = [f.output_size[0].min(od.Width as u32), f.output_size[1].min(od.Height)];
            let consts = [f.render_size[0], f.render_size[1], out[0], out[1]];
            list.SetDescriptorHeaps(&[Some(heap.clone())]);
            list.SetComputeRootSignature(root);
            list.SetPipelineState(pso);
            list.SetComputeRoot32BitConstants(0, 4, consts.as_ptr().cast(), 0);
            list.SetComputeRootDescriptorTable(1, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: gpu });
            list.SetComputeRootDescriptorTable(2, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: gpu + u64::from(*step) });
            list.Dispatch(out[0].div_ceil(8), out[1].div_ceil(8), 1);
            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::transmute_copy(f.output) }),
                },
            }]);
        }
        true
    }
}

impl Upscaler for crate::dlss::DlssBackend {
    fn name(&self) -> &'static str {
        crate::dlss::DlssBackend::name(self)
    }

    fn setup(&mut self, device: &ID3D12Device, setup: &UpscaleSetup) -> bool {
        crate::dlss::DlssBackend::setup(self, device, setup.flags)
    }

    fn dispatch(&mut self, _: &ID3D12Device, list: &ID3D12GraphicsCommandList, f: &UpscaleFrame) -> bool {
        let (Some(depth), Some(motion)) = (f.depth, f.motion) else { return false };
        let inputs = crate::dlss::UpscaleInputs {
            color: f.colour,
            depth,
            motion_vectors: motion,
            exposure: f.exposure,
            output: f.output,
            jitter: f.jitter,
            mv_scale: f.motion_scale,
            render_size: f.render_size,
            reset: f.reset,
            pre_exposure: f.pre_exposure,
            frame_time_ms: f.frame_ms,
        };
        crate::dlss::DlssBackend::dispatch(self, list, &inputs)
    }

    fn release(&mut self) {
        crate::dlss::DlssBackend::release(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typeless_gets_a_typed_view() {
        assert_eq!(typed(DXGI_FORMAT_R16G16B16A16_TYPELESS), Some(DXGI_FORMAT_R16G16B16A16_FLOAT));
        assert_eq!(typed(DXGI_FORMAT_R11G11B10_FLOAT), Some(DXGI_FORMAT_R11G11B10_FLOAT));
        assert_eq!(typed(DXGI_FORMAT_R24G8_TYPELESS), None);
    }

    #[test]
    fn spatial_shader_compiles() {
        crate::host::compile(SPATIAL, s!("main"), s!("cs_5_1")).unwrap();
    }
}
