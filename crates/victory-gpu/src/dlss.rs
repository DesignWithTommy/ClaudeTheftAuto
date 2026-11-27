//! DLSS Super Resolution through NVIDIA NGX, bound at run time.
//!
//! Ported from the rival-apex fork's DLSS backend (3af9ba6, c77bd1f), with
//! one change: that port linked `nvsdk_ngx_d.lib` (NVIDIA's static SDK
//! library). This one links nothing from NVIDIA and needs no SDK to build.
//! It loads NVIDIA's own DLLs when `VICTORY_UPSCALER=dlss` is chosen:
//!
//! * the NGX core that ships with the display driver (`_nvngx.dll` /
//!   `nvngx.dll`), whose exports carry the `NVSDK_NGX_D3D12_*` entry points
//!   (the feature DLL `nvngx_dlss.dll` does not export them);
//! * `nvngx_dlss.dll`, from `VICTORY_DLSS_DLL_DIR` or `<exe dir>\dlss`
//!   (`tools/Setup-Dlss.ps1` puts it there), whose folder is handed to the
//!   core as its feature search path.
//!
//! The parameter block NGX hands out is a C++ object; the SDK's `_SetUI`-style
//! helpers are calls through its vtable, which is called here the same way
//! (slot order of `NVSDK_NGX_Parameter`, checked with a round trip at start).

use std::ffi::{CStr, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12GraphicsCommandList, ID3D12Resource};
use windows::core::Interface;

mod ngx {
    use std::ffi::{c_char, c_void};

    pub type Result = u32;
    pub const SUCCESS: u32 = 1;
    pub const FEATURE_SUPER_SAMPLING: i32 = 1;
    pub const ENGINE_CUSTOM: i32 = 0;
    pub const VERSION_API: i32 = 0x15;

    pub const PERF_MAX_PERF: i32 = 0;
    pub const PERF_BALANCED: i32 = 1;
    pub const PERF_MAX_QUALITY: i32 = 2;
    pub const PERF_ULTRA_PERFORMANCE: i32 = 3;

    pub const FLAG_IS_HDR: i32 = 1 << 0;
    pub const FLAG_MV_LOW_RES: i32 = 1 << 1;
    pub const FLAG_MV_JITTERED: i32 = 1 << 2;
    pub const FLAG_DEPTH_INVERTED: i32 = 1 << 3;
    pub const FLAG_AUTO_EXPOSURE: i32 = 1 << 6;

    /// `NVSDK_NGX_Parameter`: an abstract C++ class, used through its vtable.
    #[repr(C)]
    pub struct Parameter {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct Handle {
        _p: [u8; 0],
    }

    #[repr(C)]
    pub struct PathListInfo {
        pub path: *const *const u16,
        pub length: u32,
    }
    pub type LogCallback = extern "C" fn(*const c_char, i32, i32);
    #[repr(C)]
    pub struct LoggingInfo {
        pub callback: Option<LogCallback>,
        pub minimum_level: i32,
        pub disable_other_sinks: bool,
    }
    #[repr(C)]
    pub struct FeatureCommonInfo {
        pub path_list: PathListInfo,
        pub internal: *mut c_void,
        pub logging: LoggingInfo,
    }

    pub type ParamCallback = unsafe extern "C" fn(*mut Parameter) -> Result;

    // Slots of the parameter vtable, in declaration order of the C++ class:
    // Set(u64, f32, f64, u32, i32, ID3D11Resource*, ID3D12Resource*, void*),
    // Get(the same eight), Reset.
    pub const SLOT_SET_F32: usize = 1;
    pub const SLOT_SET_U32: usize = 3;
    pub const SLOT_SET_I32: usize = 4;
    pub const SLOT_SET_D3D12: usize = 6;
    pub const SLOT_GET_U32: usize = 11;
    pub const SLOT_GET_I32: usize = 12;
    pub const SLOT_GET_VOID: usize = 15;

    type Init = unsafe extern "C" fn(*const c_char, i32, *const c_char, *const u16, *mut c_void, *const FeatureCommonInfo, i32) -> Result;
    type Shutdown = unsafe extern "C" fn(*mut c_void) -> Result;
    type GetCaps = unsafe extern "C" fn(*mut *mut Parameter) -> Result;
    type Destroy = unsafe extern "C" fn(*mut Parameter) -> Result;
    type Create = unsafe extern "C" fn(*mut c_void, i32, *mut Parameter, *mut *mut Handle) -> Result;
    type Evaluate = unsafe extern "C" fn(*mut c_void, *const Handle, *const Parameter, *const c_void) -> Result;
    type Release = unsafe extern "C" fn(*mut Handle) -> Result;

    /// The entry points, resolved from the NGX core at run time.
    pub struct Api {
        pub init: Init,
        pub shutdown: Shutdown,
        pub get_capability_parameters: GetCaps,
        pub destroy_parameters: Destroy,
        pub create_feature: Create,
        pub evaluate_feature: Evaluate,
        pub release_feature: Release,
    }

    /// The function in vtable slot `slot` of `this`.
    ///
    /// # Safety
    /// `this` is a live `NVSDK_NGX_Parameter` (or a vtable-first object) and
    /// `F` is the slot's function type.
    pub unsafe fn slot<F: Copy>(this: *mut Parameter, slot: usize) -> F {
        // SAFETY: the object's first word is its vtable pointer.
        unsafe {
            let vtable = *this.cast::<*const usize>();
            std::mem::transmute_copy(&*vtable.add(slot))
        }
    }

    impl Parameter {
        /// # Safety
        /// `this` is a live parameter block; `name` is NUL terminated.
        pub unsafe fn set_u32(this: *mut Parameter, name: *const c_char, v: u32) {
            unsafe { slot::<unsafe extern "C" fn(*mut Parameter, *const c_char, u32)>(this, SLOT_SET_U32)(this, name, v) }
        }
        /// # Safety
        /// As [`Parameter::set_u32`].
        pub unsafe fn set_i32(this: *mut Parameter, name: *const c_char, v: i32) {
            unsafe { slot::<unsafe extern "C" fn(*mut Parameter, *const c_char, i32)>(this, SLOT_SET_I32)(this, name, v) }
        }
        /// # Safety
        /// As [`Parameter::set_u32`].
        pub unsafe fn set_f32(this: *mut Parameter, name: *const c_char, v: f32) {
            unsafe { slot::<unsafe extern "C" fn(*mut Parameter, *const c_char, f32)>(this, SLOT_SET_F32)(this, name, v) }
        }
        /// # Safety
        /// As [`Parameter::set_u32`]; `v` is a live `ID3D12Resource` or null.
        pub unsafe fn set_d3d12(this: *mut Parameter, name: *const c_char, v: *mut c_void) {
            unsafe { slot::<unsafe extern "C" fn(*mut Parameter, *const c_char, *mut c_void)>(this, SLOT_SET_D3D12)(this, name, v) }
        }
        /// # Safety
        /// As [`Parameter::set_u32`].
        pub unsafe fn get_u32(this: *mut Parameter, name: *const c_char, v: *mut u32) -> Result {
            unsafe { slot::<unsafe extern "C" fn(*mut Parameter, *const c_char, *mut u32) -> Result>(this, SLOT_GET_U32)(this, name, v) }
        }
        /// # Safety
        /// As [`Parameter::set_u32`].
        pub unsafe fn get_i32(this: *mut Parameter, name: *const c_char, v: *mut i32) -> Result {
            unsafe { slot::<unsafe extern "C" fn(*mut Parameter, *const c_char, *mut i32) -> Result>(this, SLOT_GET_I32)(this, name, v) }
        }
        /// # Safety
        /// As [`Parameter::set_u32`].
        pub unsafe fn get_void(this: *mut Parameter, name: *const c_char, v: *mut *mut c_void) -> Result {
            unsafe { slot::<unsafe extern "C" fn(*mut Parameter, *const c_char, *mut *mut c_void) -> Result>(this, SLOT_GET_VOID)(this, name, v) }
        }
    }
}

/// The NGX entry points, resolved once and kept for the process.
struct Core {
    api: ngx::Api,
}

// SAFETY: plain function pointers into DLLs that stay loaded.
unsafe impl Send for Core {}
// SAFETY: as above.
unsafe impl Sync for Core {}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn load_library(path: &Path) -> Option<windows_sys::Win32::Foundation::HMODULE> {
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW};
    let path = std::path::absolute(path).ok()?;
    let w = wide(&path);
    // SAFETY: a NUL-terminated absolute path; the module is kept loaded.
    let module = unsafe { LoadLibraryExW(w.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) };
    (!module.is_null()).then_some(module)
}

/// Resolves the D3D12 entry points in `module`; `None` if any is missing.
// Each `get!` takes its target type from the `ngx::Api` field it fills, so a turbofish would repeat every signature.
#[allow(clippy::missing_transmute_annotations)]
fn resolve(module: windows_sys::Win32::Foundation::HMODULE) -> Option<ngx::Api> {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    macro_rules! get {
        ($name:literal) => {
            // SAFETY: a NUL-terminated name; the signature is the SDK's.
            unsafe { std::mem::transmute(GetProcAddress(module, concat!($name, "\0").as_ptr())?) }
        };
    }
    Some(ngx::Api {
        init: get!("NVSDK_NGX_D3D12_Init_ProjectID"),
        shutdown: get!("NVSDK_NGX_D3D12_Shutdown1"),
        get_capability_parameters: get!("NVSDK_NGX_D3D12_GetCapabilityParameters"),
        destroy_parameters: get!("NVSDK_NGX_D3D12_DestroyParameters"),
        create_feature: get!("NVSDK_NGX_D3D12_CreateFeature"),
        evaluate_feature: get!("NVSDK_NGX_D3D12_EvaluateFeature"),
        release_feature: get!("NVSDK_NGX_D3D12_ReleaseFeature"),
    })
}

/// Where the driver's NGX core may be, best first: `VICTORY_NGX_CORE`, the
/// folder NGX registers, System32, the driver store, then the feature DLL.
fn core_candidates(dll_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("VICTORY_NGX_CORE") {
        out.push(PathBuf::from(p));
    }
    if let Some(dir) = registered_core_dir() {
        out.push(dir.join("_nvngx.dll"));
        out.push(dir.join("nvngx.dll"));
    }
    let windir = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    out.push(windir.join("System32").join("nvngx.dll"));
    // newest first among the driver packages that carry one
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(windir.join(r"System32\DriverStore\FileRepository")) {
        for e in entries.flatten() {
            if !e.file_name().to_string_lossy().to_ascii_lowercase().starts_with("nv") {
                continue;
            }
            for name in ["_nvngx.dll", "nvngx.dll"] {
                let p = e.path().join(name);
                if let Ok(t) = p.metadata().and_then(|m| m.modified()) {
                    found.push((t, p));
                }
            }
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    out.extend(found.into_iter().map(|f| f.1));
    out.push(dll_dir.join("nvngx_dlss.dll"));
    out
}

/// `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore`, `FullPath`: the folder
/// of the driver's NGX core, as the SDK's own loader reads it.
fn registered_core_dir() -> Option<PathBuf> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let key: Vec<u16> = r"SOFTWARE\NVIDIA Corporation\Global\NGXCore".encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = "FullPath".encode_utf16().chain(Some(0)).collect();
    let mut buf = [0u16; 520];
    let mut bytes = std::mem::size_of_val(&buf) as u32;
    // SAFETY: a registry read into our buffer, whose size is passed.
    let status = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
    };
    if status != 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    (!s.is_empty()).then(|| PathBuf::from(s))
}

fn core(dll_dir: &Path) -> Result<&'static Core> {
    static CORE: std::sync::OnceLock<Option<Core>> = std::sync::OnceLock::new();
    CORE.get_or_init(|| {
        // keep the feature DLL resident (and check it loads) before the core asks for it
        let feature = dll_dir.join("nvngx_dlss.dll");
        if load_library(&feature).is_none() {
            tracing::warn!(target: "victory::dlss", "{} did not load (needs the 64-bit NVIDIA DLL)", feature.display());
        }
        for path in core_candidates(dll_dir) {
            if !path.is_file() {
                continue;
            }
            let Some(module) = load_library(&path) else { continue };
            if let Some(api) = resolve(module) {
                tracing::info!(target: "victory::dlss", core = %path.display(), "NGX core loaded");
                return Some(Core { api });
            }
        }
        None
    })
    .as_ref()
    .context("no NGX core with the D3D12 entry points (an NVIDIA driver provides nvngx.dll / _nvngx.dll)")
}

const PROJECT_ID: &CStr = c"5c1f6a3e-0b7d-4e2a-9c41-7d2b8e6f1a90";

macro_rules! name {
    ($s:literal) => {
        concat!($s, "\0").as_ptr().cast::<c_char>()
    };
}

fn ok(what: &str, r: ngx::Result) -> Result<()> {
    if r == ngx::SUCCESS { Ok(()) } else { bail!("ngx {what} failed: {r:#010x}") }
}

/// The DLSS quality mode, from VICTORY_DLSS=quality|balanced|performance|ultra.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DlssQuality {
    Quality,
    Balanced,
    Performance,
    UltraPerformance,
}

impl DlssQuality {
    /// none when VICTORY_DLSS is unset, empty, 0 or off.
    pub fn from_env() -> Option<Self> {
        Self::parse(&std::env::var("VICTORY_DLSS").ok()?)
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "quality" | "1" => Some(Self::Quality),
            "balanced" => Some(Self::Balanced),
            "performance" | "perf" => Some(Self::Performance),
            "ultra" | "ultra_performance" | "ultraperformance" => Some(Self::UltraPerformance),
            _ => None,
        }
    }

    fn ngx(self) -> i32 {
        match self {
            Self::Quality => ngx::PERF_MAX_QUALITY,
            Self::Balanced => ngx::PERF_BALANCED,
            Self::Performance => ngx::PERF_MAX_PERF,
            Self::UltraPerformance => ngx::PERF_ULTRA_PERFORMANCE,
        }
    }
}

/// Fixed at feature creation; a change recreates the feature on the next upscale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DlssSettings {
    pub quality: DlssQuality,
    pub hdr: bool,
    pub depth_inverted: bool,
    pub mv_jittered: bool,
    pub auto_exposure: bool,
    pub mv_low_res: bool,
}

impl DlssSettings {
    pub fn new(quality: DlssQuality) -> Self {
        Self { quality, hdr: false, depth_inverted: false, mv_jittered: false, auto_exposure: false, mv_low_res: true }
    }

    /// from ffx context flags: 0x1 hdr, 0x2 display-res mvs, 0x4 jittered mvs, 0x8 depth inverted, 0x20 auto exposure.
    pub fn from_ffx_flags(quality: DlssQuality, flags: u32) -> Self {
        Self {
            quality,
            hdr: flags & 0x1 != 0,
            mv_low_res: flags & 0x2 == 0,
            mv_jittered: flags & 0x4 != 0,
            depth_inverted: flags & 0x8 != 0,
            auto_exposure: flags & 0x20 != 0,
        }
    }

    fn flags(&self) -> i32 {
        let mut f = 0;
        if self.mv_low_res {
            f |= ngx::FLAG_MV_LOW_RES;
        }
        if self.hdr {
            f |= ngx::FLAG_IS_HDR;
        }
        if self.mv_jittered {
            f |= ngx::FLAG_MV_JITTERED;
        }
        if self.depth_inverted {
            f |= ngx::FLAG_DEPTH_INVERTED;
        }
        if self.auto_exposure {
            f |= ngx::FLAG_AUTO_EXPOSURE;
        }
        f
    }
}

/// Render sizes DLSS wants for one output size and quality mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderSize {
    pub optimal: [u32; 2],
    pub min: [u32; 2],
    pub max: [u32; 2],
}

/// inputs in pixel|non-pixel shader-resource state, output in unordered access; ngx changes the list's heaps/root state.
pub struct UpscaleInputs<'a> {
    pub color: &'a ID3D12Resource,
    pub depth: &'a ID3D12Resource,
    pub motion_vectors: &'a ID3D12Resource,
    pub exposure: Option<&'a ID3D12Resource>,
    pub output: &'a ID3D12Resource,
    /// jitter in render pixels, as the title applied it
    pub jitter: [f32; 2],
    /// scale taking the title's vectors to render pixels (fsr's motion vector scale)
    pub mv_scale: [f32; 2],
    /// the rendered part of `color` (top-left), for dynamic resolution
    pub render_size: [u32; 2],
    pub reset: bool,
    pub pre_exposure: f32,
    pub frame_time_ms: f32,
}

struct Feature {
    handle: *mut ngx::Handle,
    input: [u32; 2],
    output: [u32; 2],
    settings: DlssSettings,
}

static LIVE: AtomicBool = AtomicBool::new(false);

/// DLSS Super Resolution on one host device.
pub struct DlssUpscaler {
    core: &'static Core,
    device: ID3D12Device,
    params: *mut ngx::Parameter,
    feature: Option<Feature>,
    settings: DlssSettings,
    _paths: Box<(Vec<u16>, [*const u16; 1], ngx::FeatureCommonInfo)>,
}

// SAFETY: ngx's d3d12 calls are made from one thread at a time through &mut self.
unsafe impl Send for DlssUpscaler {}

extern "C" fn log_line(msg: *const c_char, _level: i32, _feature: i32) {
    if !msg.is_null() {
        // SAFETY: ngx passes a nul-terminated line that lives for the call.
        let line = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
        tracing::debug!(target: "victory::dlss", "{}", line.trim_end());
    }
}

/// Where `nvngx_dlss.dll` is looked for: `VICTORY_DLSS_DLL_DIR`, else
/// `<folder of the executable>\dlss`.
pub fn dll_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("VICTORY_DLSS_DLL_DIR") {
        return Some(d.into());
    }
    Some(std::env::current_exe().ok()?.parent()?.join("dlss"))
}

impl DlssUpscaler {
    /// none (and nothing loaded) unless VICTORY_DLSS names a mode.
    pub fn from_env(device: &ID3D12Device) -> Option<Result<Self>> {
        DlssQuality::from_env().map(|q| Self::new(device, DlssSettings::new(q)))
    }

    /// inits ngx on the device and checks dlss is available there.
    pub fn new(device: &ID3D12Device, settings: DlssSettings) -> Result<Self> {
        if LIVE.swap(true, Ordering::AcqRel) {
            bail!("one dlss upscaler at a time");
        }
        match Self::init(device, settings) {
            Ok(u) => Ok(u),
            Err(e) => {
                LIVE.store(false, Ordering::Release);
                Err(e)
            }
        }
    }

    fn init(device: &ID3D12Device, settings: DlssSettings) -> Result<Self> {
        let dir = dll_dir().context("no M1LK_DLSS_DLL_DIR and no executable folder")?;
        if !dir.join("nvngx_dlss.dll").is_file() {
            bail!("no nvngx_dlss.dll in {} (tools/Setup-Dlss.ps1 installs it)", dir.display());
        }
        let core = core(&dir)?;
        let data = std::env::var_os("VICTORY_DLSS_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("victory-ngx"));
        std::fs::create_dir_all(&data).with_context(|| format!("ngx data dir {}", data.display()))?;
        let verbose = std::env::var("VICTORY_DLSS_LOG").is_ok_and(|v| v == "1");

        let mut paths = Box::new((
            wide(&dir),
            [std::ptr::null(); 1],
            ngx::FeatureCommonInfo {
                path_list: ngx::PathListInfo { path: std::ptr::null(), length: 1 },
                internal: std::ptr::null_mut(),
                logging: ngx::LoggingInfo {
                    callback: Some(log_line),
                    minimum_level: if verbose { 1 } else { 0 },
                    disable_other_sinks: false,
                },
            },
        ));
        paths.1[0] = paths.0.as_ptr();
        paths.2.path_list.path = paths.1.as_ptr();
        let data_w = wide(&data);

        // SAFETY: the device outlives ngx (we hold a reference until shutdown); paths live in the box.
        unsafe {
            ok(
                "init",
                (core.api.init)(
                    PROJECT_ID.as_ptr(),
                    ngx::ENGINE_CUSTOM,
                    c"1.0".as_ptr(),
                    data_w.as_ptr(),
                    device.as_raw(),
                    &paths.2,
                    ngx::VERSION_API,
                ),
            )?;
        }
        let mut params = std::ptr::null_mut();
        // SAFETY: ngx is initialised on this device.
        let caps = unsafe { (core.api.get_capability_parameters)(&mut params) };
        if caps != ngx::SUCCESS || params.is_null() {
            // SAFETY: undo the init above.
            unsafe { (core.api.shutdown)(device.as_raw()) };
            bail!("ngx capability parameters failed: {caps:#010x}");
        }
        let up = Self { core, device: device.clone(), params, feature: None, settings, _paths: paths };
        if !up.vtable_checks_out() {
            bail!("ngx parameter block does not behave as the expected C++ layout (a driver or DLL this build does not know)");
        }
        let (available, why) = up.availability();
        if !available {
            bail!("dlss not available: {why}");
        }
        tracing::info!(target: "victory::dlss", quality = ?settings.quality, "dlss ready (nvidia ngx, bound at run time)");
        Ok(up)
    }

    /// A value written and read back through the vtable: `false` if the slot
    /// order is not the one assumed (the call would have gone elsewhere).
    fn vtable_checks_out(&self) -> bool {
        let (mut u, mut i) = (0u32, 0i32);
        // SAFETY: a live parameter block; names are ours.
        unsafe {
            ngx::Parameter::set_u32(self.params, name!("M1LK.Probe.U"), 0x4d31_4c4b);
            ngx::Parameter::set_i32(self.params, name!("M1LK.Probe.I"), -7);
            let a = ngx::Parameter::get_u32(self.params, name!("M1LK.Probe.U"), &mut u);
            let b = ngx::Parameter::get_i32(self.params, name!("M1LK.Probe.I"), &mut i);
            a == ngx::SUCCESS && b == ngx::SUCCESS && u == 0x4d31_4c4b && i == -7
        }
    }

    /// whether dlss sr runs here, and why not.
    pub fn availability(&self) -> (bool, String) {
        let (mut avail, mut needs_driver, mut major, mut minor, mut init) = (0i32, 0i32, 0u32, 0u32, 0i32);
        // SAFETY: params is ngx's capability block; missing names leave the zeros.
        unsafe {
            ngx::Parameter::get_i32(self.params, name!("SuperSampling.Available"), &mut avail);
            ngx::Parameter::get_i32(self.params, name!("SuperSampling.NeedsUpdatedDriver"), &mut needs_driver);
            ngx::Parameter::get_u32(self.params, name!("SuperSampling.MinDriverVersionMajor"), &mut major);
            ngx::Parameter::get_u32(self.params, name!("SuperSampling.MinDriverVersionMinor"), &mut minor);
            ngx::Parameter::get_i32(self.params, name!("SuperSampling.FeatureInitResult"), &mut init);
        }
        if avail != 0 {
            return (true, String::new());
        }
        let why = if needs_driver != 0 {
            format!("driver too old, needs {major}.{minor}")
        } else {
            format!("feature init result {:#010x}", init as u32)
        };
        (false, why)
    }

    pub fn settings(&self) -> DlssSettings {
        self.settings
    }

    /// takes effect on the next upscale (the feature is recreated).
    pub fn set_settings(&mut self, settings: DlssSettings) {
        self.settings = settings;
    }

    /// render sizes for an output size at the current quality mode.
    pub fn optimal_render_size(&self, output: [u32; 2]) -> Result<RenderSize> {
        let p = self.params;
        let mut cb: *mut c_void = std::ptr::null_mut();
        // SAFETY: ngx's capability block holds the callback; we call it the way the sdk helper does.
        unsafe {
            ngx::Parameter::get_void(p, name!("DLSSOptimalSettingsCallback"), &mut cb);
            if cb.is_null() {
                bail!("dlss optimal-settings callback missing (old dlss dll)");
            }
            let cb: ngx::ParamCallback = std::mem::transmute(cb);
            ngx::Parameter::set_u32(p, name!("Width"), output[0]);
            ngx::Parameter::set_u32(p, name!("Height"), output[1]);
            ngx::Parameter::set_i32(p, name!("PerfQualityValue"), self.settings.quality.ngx());
            ngx::Parameter::set_i32(p, name!("RTXValue"), 0);
            ok("optimal settings", cb(p))?;
            let mut optimal = [0u32; 2];
            ngx::Parameter::get_u32(p, name!("OutWidth"), &mut optimal[0]);
            ngx::Parameter::get_u32(p, name!("OutHeight"), &mut optimal[1]);
            let (mut min, mut max) = (optimal, optimal);
            ngx::Parameter::get_u32(p, name!("DLSS.Get.Dynamic.Max.Render.Width"), &mut max[0]);
            ngx::Parameter::get_u32(p, name!("DLSS.Get.Dynamic.Max.Render.Height"), &mut max[1]);
            ngx::Parameter::get_u32(p, name!("DLSS.Get.Dynamic.Min.Render.Width"), &mut min[0]);
            ngx::Parameter::get_u32(p, name!("DLSS.Get.Dynamic.Min.Render.Height"), &mut min[1]);
            if optimal[0] == 0 || optimal[1] == 0 {
                bail!("dlss gave no render size for {}x{}", output[0], output[1]);
            }
            Ok(RenderSize { optimal, min, max })
        }
    }

    fn ensure_feature(&mut self, list: &ID3D12GraphicsCommandList, input: [u32; 2], output: [u32; 2]) -> Result<()> {
        let want = (input, output, self.settings);
        if let Some(f) = &self.feature
            && (f.input, f.output, f.settings) == want
        {
            return Ok(());
        }
        self.release_feature();
        let p = self.params;
        let mut handle = std::ptr::null_mut();
        // SAFETY: plain parameter writes and a create recorded on the caller's open list.
        unsafe {
            ngx::Parameter::set_u32(p, name!("CreationNodeMask"), 1);
            ngx::Parameter::set_u32(p, name!("VisibilityNodeMask"), 1);
            ngx::Parameter::set_u32(p, name!("Width"), input[0]);
            ngx::Parameter::set_u32(p, name!("Height"), input[1]);
            ngx::Parameter::set_u32(p, name!("OutWidth"), output[0]);
            ngx::Parameter::set_u32(p, name!("OutHeight"), output[1]);
            ngx::Parameter::set_i32(p, name!("PerfQualityValue"), self.settings.quality.ngx());
            ngx::Parameter::set_i32(p, name!("DLSS.Feature.Create.Flags"), self.settings.flags());
            ngx::Parameter::set_i32(p, name!("DLSS.Enable.Output.Subrects"), 0);
            ok(
                "create dlss",
                (self.core.api.create_feature)(list.as_raw(), ngx::FEATURE_SUPER_SAMPLING, p, &mut handle),
            )?;
        }
        tracing::info!(target: "victory::dlss", "dlss feature {}x{} -> {}x{}", input[0], input[1], output[0], output[1]);
        self.feature = Some(Feature { handle, input, output, settings: self.settings });
        Ok(())
    }

    /// records dlss into an open direct list; creates or recreates the feature as sizes change.
    pub fn upscale(&mut self, list: &ID3D12GraphicsCommandList, inputs: &UpscaleInputs) -> Result<()> {
        // SAFETY: reading resource descriptions.
        let (cd, od) = unsafe { (inputs.color.GetDesc(), inputs.output.GetDesc()) };
        let input = [cd.Width as u32, cd.Height];
        let output = [od.Width as u32, od.Height];
        let render = [inputs.render_size[0].min(input[0]), inputs.render_size[1].min(input[1])];
        if render[0] == 0 || render[1] == 0 {
            bail!("dlss render size is zero");
        }
        self.ensure_feature(list, input, output)?;
        let handle = self.feature.as_ref().map(|f| f.handle).unwrap_or(std::ptr::null_mut());
        let p = self.params;
        let exposure = inputs.exposure.map_or(std::ptr::null_mut(), |r| r.as_raw());
        let mv = |v: f32| if v == 0.0 { 1.0 } else { v };
        // SAFETY: the resources are alive for the call; ngx records into the caller's list.
        unsafe {
            ngx::Parameter::set_d3d12(p, name!("Color"), inputs.color.as_raw());
            ngx::Parameter::set_d3d12(p, name!("Output"), inputs.output.as_raw());
            ngx::Parameter::set_d3d12(p, name!("Depth"), inputs.depth.as_raw());
            ngx::Parameter::set_d3d12(p, name!("MotionVectors"), inputs.motion_vectors.as_raw());
            ngx::Parameter::set_d3d12(p, name!("ExposureTexture"), exposure);
            ngx::Parameter::set_f32(p, name!("Jitter.Offset.X"), inputs.jitter[0]);
            ngx::Parameter::set_f32(p, name!("Jitter.Offset.Y"), inputs.jitter[1]);
            ngx::Parameter::set_f32(p, name!("MV.Scale.X"), mv(inputs.mv_scale[0]));
            ngx::Parameter::set_f32(p, name!("MV.Scale.Y"), mv(inputs.mv_scale[1]));
            ngx::Parameter::set_f32(p, name!("Sharpness"), 0.0);
            ngx::Parameter::set_i32(p, name!("Reset"), i32::from(inputs.reset));
            ngx::Parameter::set_u32(p, name!("DLSS.Input.Color.Subrect.Base.X"), 0);
            ngx::Parameter::set_u32(p, name!("DLSS.Input.Color.Subrect.Base.Y"), 0);
            ngx::Parameter::set_u32(p, name!("DLSS.Input.Depth.Subrect.Base.X"), 0);
            ngx::Parameter::set_u32(p, name!("DLSS.Input.Depth.Subrect.Base.Y"), 0);
            ngx::Parameter::set_u32(p, name!("DLSS.Input.MV.Subrect.Base.X"), 0);
            ngx::Parameter::set_u32(p, name!("DLSS.Input.MV.Subrect.Base.Y"), 0);
            ngx::Parameter::set_u32(p, name!("DLSS.Render.Subrect.Dimensions.Width"), render[0]);
            ngx::Parameter::set_u32(p, name!("DLSS.Render.Subrect.Dimensions.Height"), render[1]);
            let pre = if inputs.pre_exposure == 0.0 { 1.0 } else { inputs.pre_exposure };
            ngx::Parameter::set_f32(p, name!("DLSS.Pre.Exposure"), pre);
            ngx::Parameter::set_f32(p, name!("DLSS.Exposure.Scale"), 1.0);
            ngx::Parameter::set_f32(p, name!("FrameTimeDeltaInMsec"), inputs.frame_time_ms);
            ok(
                "evaluate dlss",
                (self.core.api.evaluate_feature)(list.as_raw(), handle, p, std::ptr::null()),
            )
        }
    }

    /// drops the feature; only once the gpu is done with lists that used it.
    pub fn release_feature(&mut self) {
        if let Some(f) = self.feature.take() {
            // SAFETY: the handle came from create and is released once.
            let r = unsafe { (self.core.api.release_feature)(f.handle) };
            if r != ngx::SUCCESS {
                tracing::warn!(target: "victory::dlss", "ngx release feature: {r:#010x}");
            }
        }
    }
}

impl Drop for DlssUpscaler {
    fn drop(&mut self) {
        self.release_feature();
        // SAFETY: params came from ngx; shutdown matches the init on this device.
        unsafe {
            (self.core.api.destroy_parameters)(self.params);
            (self.core.api.shutdown)(self.device.as_raw());
        }
        LIVE.store(false, Ordering::Release);
    }
}

/// The upscaler shape the host's ffx routing uses: setup/dispatch/release return bools, ngx is brought up on first setup.
pub struct DlssBackend {
    quality: DlssQuality,
    up: Option<DlssUpscaler>,
    failed: bool,
}

impl DlssBackend {
    /// none unless VICTORY_DLSS names a mode.
    pub fn from_env() -> Option<Self> {
        DlssQuality::from_env().map(|quality| Self { quality, up: None, failed: false })
    }

    pub fn name(&self) -> &'static str {
        "dlss"
    }

    pub fn setup(&mut self, device: &ID3D12Device, ffx_flags: u32) -> bool {
        if self.failed {
            return false;
        }
        let settings = DlssSettings::from_ffx_flags(self.quality, ffx_flags);
        if let Some(up) = &mut self.up {
            up.set_settings(settings);
            return true;
        }
        match DlssUpscaler::new(device, settings) {
            Ok(up) => {
                self.up = Some(up);
                true
            }
            Err(e) => {
                tracing::warn!(target: "victory::dlss", "dlss off: {e:#}");
                self.failed = true;
                false
            }
        }
    }

    pub fn dispatch(&mut self, list: &ID3D12GraphicsCommandList, inputs: &UpscaleInputs) -> bool {
        let Some(up) = &mut self.up else { return false };
        match up.upscale(list, inputs) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(target: "victory::dlss", "dlss skipped: {e:#}");
                false
            }
        }
    }

    pub fn optimal_render_size(&self, output: [u32; 2]) -> Option<RenderSize> {
        self.up.as_ref()?.optimal_render_size(output).ok()
    }

    /// only once the gpu is done with lists that used the feature.
    pub fn release(&mut self) {
        if let Some(up) = &mut self.up {
            up.release_feature();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_names() {
        assert_eq!(DlssQuality::parse("quality"), Some(DlssQuality::Quality));
        assert_eq!(DlssQuality::parse("Balanced"), Some(DlssQuality::Balanced));
        assert_eq!(DlssQuality::parse("performance"), Some(DlssQuality::Performance));
        assert_eq!(DlssQuality::parse("ultra"), Some(DlssQuality::UltraPerformance));
        assert_eq!(DlssQuality::parse("off"), None);
        assert_eq!(DlssQuality::parse(""), None);
        let s = DlssSettings::from_ffx_flags(DlssQuality::Quality, 0x1 | 0x8);
        assert!(s.hdr && s.depth_inverted && s.mv_low_res && !s.mv_jittered);
        assert!(!DlssSettings::from_ffx_flags(DlssQuality::Quality, 0x2).mv_low_res);
    }

    /// A stand-in parameter object: slot `n` records that it was called.
    #[test]
    fn vtable_slots_are_called_by_declaration_order() {
        use std::sync::atomic::AtomicUsize;
        static LAST: AtomicUsize = AtomicUsize::new(usize::MAX);
        macro_rules! stub {
            ($n:literal) => {{
                unsafe extern "C" fn f(_: *mut ngx::Parameter, _: *const c_char) {
                    LAST.store($n, Ordering::SeqCst);
                }
                f as unsafe extern "C" fn(*mut ngx::Parameter, *const c_char) as usize
            }};
        }
        let vtable: [usize; 17] = [
            stub!(0), stub!(1), stub!(2), stub!(3), stub!(4), stub!(5), stub!(6), stub!(7), stub!(8), stub!(9), stub!(10), stub!(11), stub!(12),
            stub!(13), stub!(14), stub!(15), stub!(16),
        ];
        let object: [*const usize; 1] = [vtable.as_ptr()];
        let this = object.as_ptr().cast_mut().cast::<ngx::Parameter>();
        // SAFETY: the stubs ignore their arguments past `this`; the object is vtable-first.
        unsafe {
            let f = |slot: usize| ngx::slot::<unsafe extern "C" fn(*mut ngx::Parameter, *const c_char)>(this, slot)(this, std::ptr::null());
            for (call, expect) in [
                (ngx::SLOT_SET_F32, 1),
                (ngx::SLOT_SET_U32, 3),
                (ngx::SLOT_SET_I32, 4),
                (ngx::SLOT_SET_D3D12, 6),
                (ngx::SLOT_GET_U32, 11),
                (ngx::SLOT_GET_I32, 12),
                (ngx::SLOT_GET_VOID, 15),
            ] {
                f(call);
                assert_eq!(LAST.load(Ordering::SeqCst), expect);
            }
        }
    }

    /// With an NVIDIA driver installed, its NGX core carries the D3D12 entry
    /// points: they resolve without `nvngx_dlss.dll` and without an SDK.
    #[test]
    fn the_drivers_ngx_core_exports_the_d3d12_entry_points() {
        let none = std::env::temp_dir().join("victory-no-dlss");
        let found = core_candidates(&none).into_iter().filter(|p| p.is_file()).find_map(|p| {
            let module = load_library(&p)?;
            resolve(module).map(|_| p)
        });
        match found {
            Some(p) => eprintln!("ngx core with D3D12 entry points: {}", p.display()),
            None => eprintln!("ngx core: skipped, no NVIDIA driver here"),
        }
    }
}
