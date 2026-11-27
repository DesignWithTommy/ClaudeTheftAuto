//! Textures shaders write that the title's own code then reads on the CPU.
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read. Here it stays
//! in the texture's host copy. The title's ocean is one: its wave passes write
//! a 256x256 height map (R32_FLOAT) and a displacement map (R32G32_FLOAT), and
//! its CPU code samples both, which is what boats float on; it read memory
//! nothing had written (run rw1, `VICTORY_READ_WATCH=1`: about 30 places in
//! the title read those two kinds), and boats sank.
//!
//! For textures of the kinds in `VICTORY_TEXTURE_BACK` (comma-separated
//! `<width>x<height>f<format>`; `0` for none; by default the kinds the read
//! watch found the title reading), a list in which shaders write one copies it
//! into a read-back buffer of ours when it closes, and once the list has run
//! the texels are tiled into the texture's title memory in the console's
//! layout, as the console's GPU would have left them.
//!
//! shortcut: by kind, not by texture: textures of those kinds the title never
//! reads are copied back too (about a megabyte per such list). The copy shows
//! what the list's last write left; a title read in between sees the frame
//! before, as it would before the console's GPU has run.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// The kinds copied back: (width, height, format).
fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";
        let list = std::env::var("VICTORY_TEXTURE_BACK").unwrap_or_else(|_| DEFAULT.to_owned());
        let kinds: Vec<(u64, u32, u32)> = list
            .split(',')
            .filter_map(|k| {
                let (size, format) = k.trim().split_once('f')?;
                let (w, h) = size.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
            })
            .collect();
        if !kinds.is_empty() {
            tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (M1LK_TEXTURE_BACK)");
        }
        kinds
    })
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty()
        && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

/// A texture's copy into a read-back buffer, recorded into a list, and where
/// its texels go once the list has run.
pub(crate) struct Back {
    // Own the guest memory through every execution of the closed command list.
    _resource: crate::resource::ResourceRef,
    mapping: crate::inplace::ReadbackMapping,
    buffer: ID3D12Resource,
    buffer_bytes: usize,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
    layout: victory_formats::texture::Layout,
    /// The texture's title memory and its layout's bytes.
    at: usize,
    size: usize,
    /// Width, height, format (for the log).
    kind: Option<(u64, u32, u32)>,
}

fn subresource_count(desc: &D3D12_RESOURCE_DESC) -> u32 {
    let slices = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        u32::from(desc.DepthOrArraySize)
    };
    u32::from(desc.MipLevels) * slices
}

impl Back {
    pub(crate) fn destination(&self) -> (usize, usize) {
        (self.at, self.at + self.size)
    }
}

/// Expand a submission's intervals through currently mapped title aliases,
/// after releasing each list lock. CPU preparation may use any such view.
pub(crate) fn add_destination_aliases(ranges: &mut Vec<(usize, usize)>) {
    const PAGE: usize = 64 << 10;
    for index in 0..ranges.len() {
        let (at, end) = ranges[index];
        let mut page = at & !(PAGE - 1);
        while page < end {
            let lo = at.saturating_sub(page);
            let hi = (end - page).min(PAGE);
            for alias in crate::inplace::pages_of(page) {
                if alias != page {
                    ranges.push((alias + lo, alias + hi));
                }
            }
            page = page.saturating_add(PAGE);
        }
    }
}

/// Records into `list` the copy of `texture` (title resource `resource`'s
/// host copy, in the copy-source state) into a read-back buffer.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, retained: crate::resource::ResourceRef, texture: &ID3D12Resource) -> Option<Back> {
    let host = crate::host::host()?;
    let resource = retained.as_ptr();
    let (layout, at, size) = crate::resource::title_texture(resource)?;
    at.checked_add(size)?;
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    let mips = u32::from(desc.MipLevels);
    let subresources = subresource_count(&desc);
    let n = subresources as usize;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut total = 0u64;
    // SAFETY: out arrays of `n` entries.
    unsafe {
        host.device.GetCopyableFootprints(
            &desc,
            0,
            subresources,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        )
    };
    let buffer_bytes = usize::try_from(total).ok()?;
    if buffer_bytes == 0 || buffer_bytes > isize::MAX as usize || at == 0 || size == 0 {
        return None;
    }
    let mapping = crate::inplace::ReadbackMapping::record(at, size)?;
    let buffer = host.read_back_buffer(total)?;
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    COPIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let kind = crate::resource::texture_kind(resource);
    Some(Back { _resource: retained, mapping, buffer, buffer_bytes, footprints, rows, mips, layout, at, size, kind })
}

/// Textures copied back since the start.
pub(crate) static COPIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether texture read-backs are written into title memory by a thread of
/// their own (`VICTORY_TEXBACK_ASYNC`; on unless `0`).
///
/// Writing one takes the lock against mapping changes, which the title takes
/// all the time while it streams the world; on the thread that lands
/// submissions the wait stood at 100-400 ms in some frames (measured by the
/// VIctory-NVIDIA fork), and the landing of everything else with it. Here the
/// landing thread only copies the texels out of the read-back buffer; the
/// waiting, tiling and writing go to [`writer`]. Of several copies of one
/// texture waiting there only the newest is written: the title reads the
/// frame before at most. `VICTORY_TEXBACK_ASYNC=0` writes where they land.
fn asynchronous() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXBACK_ASYNC").is_none_or(|v| v != "0"))
}

/// A read-back's texels taken out of its buffer, to be tiled into title memory.
struct Landing {
    mapping: crate::inplace::ReadbackMapping,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
    layout: victory_formats::texture::Layout,
    at: usize,
    size: usize,
    kind: Option<(u64, u32, u32)>,
    /// The read-back buffer's bytes (`buffer_bytes` of them).
    data: Vec<u8>,
}

/// Once the list that recorded `back` has run: its texels, tiled into the
/// title's layout, into the texture's title memory (on [`writer`]'s thread
/// when [`asynchronous`]).
pub(crate) fn land(back: &Back) {
    let writer = asynchronous().then(writer).flatten();
    if writer.is_some() && !back.mapping.live() {
        return;
    }
    let mut mapped = std::ptr::null_mut();
    // SAFETY: our read-back buffer, written by the list that has run; it was
    // created with exactly `buffer_bytes` bytes.
    let data = unsafe {
        if back.buffer.Map(0, None, Some(&mut mapped)).is_err() || mapped.is_null() {
            return;
        }
        // GetCopyableFootprints omits the final row's trailing padding.
        let bytes = std::slice::from_raw_parts(mapped.cast::<u8>().cast_const(), back.buffer_bytes).to_vec();
        back.buffer.Unmap(0, None);
        bytes
    };
    let landing = Landing {
        mapping: back.mapping.clone(),
        footprints: back.footprints.clone(),
        rows: back.rows.clone(),
        mips: back.mips,
        layout: back.layout.clone(),
        at: back.at,
        size: back.size,
        kind: back.kind,
        data,
    };
    match writer {
        Some(writer) => writer.send(landing),
        None => write(&landing),
    }
}

/// Tiles a read-back's texels into its title allocation.
fn write(back: &Landing) {
    let Some(_mapping) = back.mapping.for_write() else { return };
    let mut tiled = vec![0u8; back.size];
    // Bytes of title memory the subresources do not cover stay as they are.
    crate::host::copy_mapped(tiled.as_mut_ptr(), back.at as *const u8, back.size, false);
    let mapped = &back.data[..];
    for (i, (footprint, &rows)) in back.footprints.iter().zip(&back.rows).enumerate() {
        let (mip, slice) = (i as u32 % back.mips, i as u32 / back.mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        // A slice ends where the next footprint begins, or at the actual buffer end.
        let end = back.footprints.get(i + 1).map_or(mapped.len(), |next| next.Offset as usize);
        let linear = &mapped[footprint.Offset as usize..end];
        if let Err(e) = back.layout.tile(&mut tiled, mip, slice, linear, pitch, pitch * rows as usize) {
            static SEEN: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
            let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.contains(&back.at) {
                seen.push(back.at);
                tracing::warn!(
                    ?e,
                    mip,
                    slice,
                    kind = ?back.kind,
                    at = format_args!("{:#x}", back.at),
                    desc = ?back.layout.desc,
                    "a texture copied back could not be tiled (logged once per texture)"
                );
            }
            return;
        }
    }
    crate::host::copy_mapped(back.at as *mut u8, tiled.as_ptr(), back.size, true);
    WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    verify_first_landing(back, &tiled);
}

/// Read-backs written into title memory, and those replaced by a newer copy
/// of the same texture before the writer got to them.
static WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REPLACED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The thread that writes read-backs into title memory, and what waits for it.
struct Writer {
    /// By title address: the newest landing for that allocation.
    waiting: std::sync::Mutex<std::collections::HashMap<usize, Landing>>,
    more: std::sync::Condvar,
    /// Set (under `waiting`) while a drained batch is being written.
    working: std::sync::atomic::AtomicBool,
    /// Signalled when the writer has nothing waiting and nothing in hand.
    idle: std::sync::Condvar,
}

impl Writer {
    fn send(&self, landing: Landing) {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        if waiting.insert(landing.at, landing).is_some() {
            let replaced = REPLACED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if replaced.is_power_of_two() {
                tracing::info!(
                    replaced,
                    written = WRITTEN.load(std::sync::atomic::Ordering::Relaxed),
                    "texture read-backs replaced by a newer copy before they were written (M1LK_TEXBACK_ASYNC=0 writes each)"
                );
            }
        }
        drop(waiting);
        self.more.notify_one();
    }
}

/// Waits until every read-back handed to [`writer`] has been written (a test's
/// barrier; the title never waits for it).
#[cfg(test)]
pub(crate) fn settle_writes_for_test() {
    let Some(writer) = asynchronous().then(writer).flatten() else { return };
    let mut waiting = writer.waiting.lock().unwrap_or_else(|e| e.into_inner());
    while !waiting.is_empty() || writer.working.load(std::sync::atomic::Ordering::SeqCst) {
        waiting = writer.idle.wait_timeout(waiting, std::time::Duration::from_secs(10)).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// The writer, if its thread could be started (if not, landings are written
/// where they are made).
fn writer() -> Option<&'static Writer> {
    static WRITER: OnceLock<Option<&'static Writer>> = OnceLock::new();
    *WRITER.get_or_init(|| {
        let writer: &'static Writer =
            Box::leak(Box::new(Writer {
            waiting: Default::default(),
            more: std::sync::Condvar::new(),
            working: Default::default(),
            idle: std::sync::Condvar::new(),
        }));
        let started = std::thread::Builder::new().name("texture read-backs".into()).spawn(move || loop {
            let batch: Vec<Landing> = {
                let mut waiting = writer.waiting.lock().unwrap_or_else(|e| e.into_inner());
                while waiting.is_empty() {
                    waiting = writer.more.wait(waiting).unwrap_or_else(|e| e.into_inner());
                }
                writer.working.store(true, std::sync::atomic::Ordering::SeqCst);
                waiting.drain().map(|(_, landing)| landing).collect()
            };
            for landing in &batch {
                write(landing);
            }
            let _waiting = writer.waiting.lock().unwrap_or_else(|e| e.into_inner());
            writer.working.store(false, std::sync::atomic::Ordering::SeqCst);
            writer.idle.notify_all();
        });
        match started {
            Ok(_) => Some(writer),
            Err(e) => {
                tracing::warn!(error = %e, "no thread for texture read-backs: they are written where they land");
                None
            }
        }
    })
}

#[cfg(feature = "diagnostics")]
fn verify_first_landing(back: &Landing, tiled: &[u8]) {
    static SEEN: std::sync::Mutex<Vec<(u64, u32, u32)>> = std::sync::Mutex::new(Vec::new());
    let Some(kind) = back.kind else { return };
    {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.contains(&kind) {
            return;
        }
        seen.push(kind);
    }
    // Check the bytes in the title destination; copy_mapped also accepts sparse ranges.
    let mut guest = vec![0; tiled.len()];
    let complete = crate::host::read(back.at as *const u8, guest.as_mut_ptr(), guest.len());
    let verified = complete && guest == tiled;
    // Sample actual mip-zero texels, excluding alignment and mip padding.
    let desc = back.layout.desc;
    let (mut finite, mut nonzero, mut lo, mut hi) = (0usize, 0usize, f32::INFINITY, f32::NEG_INFINITY);
    if verified && matches!(kind.2, 16 | 41) {
        for gy in 0..5 {
            for gx in 0..5 {
                let (x, y) = ((desc.width - 1) * gx / 4, (desc.height - 1) * gy / 4);
                let Ok(offset) = back.layout.offset(0, x, y, 0) else { continue };
                let at = offset as usize;
                let Some(texel) = guest.get(at..at + desc.element_bytes as usize) else { continue };
                for bytes in texel.chunks_exact(4) {
                    let value = f32::from_le_bytes(bytes.try_into().unwrap());
                    if value.is_finite() {
                        finite += 1;
                        nonzero += usize::from(value != 0.0);
                        lo = lo.min(value);
                        hi = hi.max(value);
                    }
                }
            }
        }
    }
    let sample_range = (finite != 0).then_some((lo, hi));
    tracing::info!(?kind, at = format_args!("{:#x}", back.at), bytes = tiled.len(), verified, finite, nonzero, ?sample_range, "first texture copy-back checked in title memory");
}
