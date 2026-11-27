//! Suballocate short-lived texture staging uploads from persistently mapped buffers.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering::Relaxed},
};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use std::sync::atomic::AtomicBool;

use crate::host::Host;

const CHUNK_BYTES: u64 = 2 << 20;
const MIN_SHIFT: u32 = 12;
const MAX_SHIFT: u32 = 19;
const CLASSES: usize = (MAX_SHIFT - MIN_SHIFT + 1) as usize;

struct Chunk {
    buffer: ID3D12Resource,
    address: usize,
}

struct Slots {
    chunk: Arc<Chunk>,
    free: Vec<u32>,
}

static POOL: Mutex<[Vec<Slots>; CLASSES]> = Mutex::new([const { Vec::new() }; CLASSES]);
static REUSE: AtomicU64 = AtomicU64::new(0);
static CREATED: AtomicU64 = AtomicU64::new(0);

/// The lifetime of a staging allocation, including every command list that
/// still holds its `Staging`. Its bytes cannot be recycled until that ends.
pub(super) struct Upload {
    chunk: Arc<Chunk>,
    slot: Option<(usize, u32)>,
    pub(super) offset: u64,
    /// Detiled on the card (`crate::gpudetile`): `chunk` then holds the
    /// title's bytes as they lie in its memory, and the copies read `linear`.
    gpu: Option<GpuSide>,
}

/// The card's side of an [`Upload`] on the card's way.
struct GpuSide {
    /// Device-local, laid out as the staging's footprints (offset 0).
    linear: ID3D12Resource,
    /// Its pool size (0: not pooled); the raw buffer's is `raw_pooled`.
    linear_pooled: u64,
    raw_pooled: u64,
    /// A submission that carried this revision's detiling has been
    /// submitted: the card's buffer holds it until the revision changes.
    detiled: AtomicBool,
}

/// Buffers of the card's way nothing holds, by pool size: the host-visible
/// ones with their mapping, and the device-local ones.
struct SpareBuf(ID3D12Resource, usize);
// SAFETY: a buffer only lists name; the mapping is plain memory it owns.
unsafe impl Send for SpareBuf {}
type SpareMap = std::collections::BTreeMap<u64, Vec<SpareBuf>>;
static SPARE_RAW: Mutex<(SpareMap, u64)> = Mutex::new((std::collections::BTreeMap::new(), 0));
static SPARE_LINEAR: Mutex<(SpareMap, u64)> = Mutex::new((std::collections::BTreeMap::new(), 0));
/// The most the spare buffers of each kind keep.
const SPARE_MOST: u64 = 256 << 20;

fn take_spare(pool: &Mutex<(SpareMap, u64)>, size: u64) -> Option<SpareBuf> {
    let mut all = pool.lock().unwrap_or_else(|e| e.into_inner());
    let taken = all.0.get_mut(&size).and_then(Vec::pop);
    if taken.is_some() {
        all.1 -= size;
    }
    taken
}

fn give_spare(pool: &Mutex<(SpareMap, u64)>, size: u64, buf: SpareBuf) {
    if size == 0 {
        return;
    }
    let mut all = pool.lock().unwrap_or_else(|e| e.into_inner());
    if all.1 + size <= SPARE_MOST {
        all.1 += size;
        all.0.entry(size).or_default().push(buf);
    }
}

impl Upload {
    pub(super) fn new(host: &Host, bytes: u64) -> Option<Self> {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let enabled = *ENABLED.get_or_init(|| {
            std::env::var_os("VICTORY_TEXTURE_UPLOAD_POOL").is_none_or(|v| v != "0")
        });
        if enabled && (1..=1 << MAX_SHIFT).contains(&bytes) {
            let size = bytes.max(1 << MIN_SHIFT).next_power_of_two();
            let class = (size.ilog2() - MIN_SHIFT) as usize;
            if let Some(upload) = Self::pooled(host, class, size) {
                // SAFETY: the pool reserved this slot exclusively and it has
                // no in-flight readers. A partial or failed refresh can leave
                // some bytes untouched, so start with the same zeros as a new
                // committed upload buffer.
                unsafe { upload.data().write_bytes(0, bytes as usize) };
                return Some(upload);
            }
        }
        let (buffer, data) = host.upload_buffer(bytes)?;
        Some(Self {
            chunk: Arc::new(Chunk {
                buffer,
                address: data as usize,
            }),
            slot: None,
            offset: 0,
            gpu: None,
        })
    }

    /// An upload on the card's way: a host-visible buffer of `raw` bytes
    /// for the title's bytes, and a device-local one of `linear` bytes the
    /// card detiles into and the copies read. The raw buffer starts with
    /// whatever it held (the caller writes all of it), `linear` likewise.
    pub(super) fn new_gpu(host: &Host, raw: u64, linear: u64) -> Option<Self> {
        let raw_size = raw.max(64 << 10).next_power_of_two();
        let linear_size = linear.max(64 << 10).next_power_of_two();
        let (raw_buf, data, raw_pooled) = match take_spare(&SPARE_RAW, raw_size) {
            Some(SpareBuf(b, p)) => (b, p as *mut u8, raw_size),
            None => {
                let (b, p) = host.upload_buffer(raw_size)?;
                crate::host::name(&b, || "texture detile source".into());
                (b, p, raw_size)
            }
        };
        let (linear_buf, linear_pooled) = match take_spare(&SPARE_LINEAR, linear_size) {
            Some(SpareBuf(b, _)) => (b, linear_size),
            None => (host.uav_buffer(linear_size)?, linear_size),
        };
        Some(Self {
            chunk: Arc::new(Chunk {
                buffer: raw_buf,
                address: data as usize,
            }),
            slot: None,
            offset: 0,
            gpu: Some(GpuSide {
                linear: linear_buf,
                linear_pooled,
                raw_pooled,
                detiled: AtomicBool::new(false),
            }),
        })
    }

    /// The host-visible buffer of the card's way (the title's bytes).
    pub(super) fn raw(&self) -> &ID3D12Resource {
        &self.chunk.buffer
    }

    /// The card's detiling is to be recorded: this upload is on the card's
    /// way and no submission has carried it yet.
    pub(super) fn gpu_pending(&self) -> Option<&ID3D12Resource> {
        let gpu = self.gpu.as_ref()?;
        (!gpu.detiled.load(Relaxed)).then_some(&gpu.linear)
    }

    pub(super) fn is_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    /// A submission carrying the detiling went out.
    pub(super) fn gpu_submitted(&self) {
        if let Some(gpu) = &self.gpu {
            gpu.detiled.store(true, Relaxed);
        }
    }

    /// The raw buffer is written anew: the card detiles it again.
    pub(super) fn gpu_changed(&self) {
        if let Some(gpu) = &self.gpu {
            gpu.detiled.store(false, Relaxed);
        }
    }

    fn pooled(host: &Host, class: usize, size: u64) -> Option<Self> {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        let slots = &mut pool[class];
        if let Some(page) = slots.iter_mut().find(|page| !page.free.is_empty()) {
            let index = page.free.pop()?;
            REUSE.fetch_add(1, Relaxed);
            return Some(Self::from_slot(page, class, index, size));
        }

        let (buffer, data) = host.upload_buffer(CHUNK_BYTES)?;
        crate::host::name(&buffer, || "texture staging pool".into());
        let page = Slots {
            chunk: Arc::new(Chunk {
                buffer,
                address: data as usize,
            }),
            free: (1..(CHUNK_BYTES / size) as u32).rev().collect(),
        };
        let upload = Self::from_slot(&page, class, 0, size);
        slots.push(page);
        CREATED.fetch_add(1, Relaxed);
        Some(upload)
    }

    fn from_slot(page: &Slots, class: usize, index: u32, size: u64) -> Self {
        Self {
            chunk: page.chunk.clone(),
            slot: Some((class, index)),
            offset: u64::from(index) * size,
            gpu: None,
        }
    }

    pub(super) fn data(&self) -> *mut u8 {
        (self.chunk.address + self.offset as usize) as *mut u8
    }

    /// The buffer the copies into the texture read.
    pub(super) fn buffer(&self) -> &ID3D12Resource {
        self.gpu.as_ref().map_or(&self.chunk.buffer, |g| &g.linear)
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if let Some(gpu) = self.gpu.take() {
            give_spare(&SPARE_RAW, gpu.raw_pooled, SpareBuf(self.chunk.buffer.clone(), self.chunk.address));
            give_spare(&SPARE_LINEAR, gpu.linear_pooled, SpareBuf(gpu.linear, 0));
            return;
        }
        let Some((class, index)) = self.slot else {
            return;
        };
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        let slots = &mut pool[class];
        let page_index = slots
            .iter()
            .position(|page| Arc::ptr_eq(&page.chunk, &self.chunk))
            .expect("a leased texture staging page must be in its size class");
        let capacity = (CHUNK_BYTES >> (MIN_SHIFT as usize + class)) as usize;
        slots[page_index].free.push(index);
        if slots[page_index].free.len() == capacity
            && slots
                .iter()
                .enumerate()
                .any(|(i, page)| i != page_index && page.free.len() == capacity)
        {
            // Keep at most one idle chunk per size class. This Upload still
            // holds the chunk, so final COM release occurs outside the lock.
            slots.swap_remove(page_index);
        }
    }
}

#[cfg(feature = "diagnostics")]
pub(super) fn totals() -> (u64, u64) {
    (CREATED.load(Relaxed), REUSE.load(Relaxed))
}
