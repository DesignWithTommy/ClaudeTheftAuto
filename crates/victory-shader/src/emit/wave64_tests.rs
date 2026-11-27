//! A console wave64 compute program on a host of 32-lane waves: a wave reduction inside a
//! per-lane branch, with that branch taken wave-wide ([`crate::structure::structure_wide`])
//! so that the two host waves can exchange their halves inside it. Lowered, validated by
//! `dxil.dll` when present, and run on the host GPU when it takes waves of 32.

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// 64 threads in one console wave. Each thread t holds the bit `1 << (t >> 1)`; the threads
/// below s0 (a root constant) OR theirs over the wave inside `if (t < s0) { ... }` and store
/// the result to dword 0 of the raw UAV.
/// `skip`: the branch past the body when no thread is below s0: `s_cbranch_execz`, or
/// `s_cbranch_scc0` on the SCC `s_and_saveexec` sets ("some lane is left").
fn program(skip: u32) -> NativeProgram {
    let mut words = vec![
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        skip | 23,   // the branch to the restore of EXEC
    ];
    words.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    native(&words)
}

/// A wave64 compute program of 64 threads (one console wave; the thread index in v0) with 4
/// root constants in s0..s3 and a raw UAV's descriptor in s[8:11], then `body`.
fn native(body: &[u32]) -> NativeProgram {
    native_of(body, true, 64)
}

/// The same for a console of 32-lane waves when `wave64` is not set, with `threads` threads.
fn native_of(body: &[u32], wave64: bool, threads: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `program()` for a host of 32-lane waves, with the branches closing `wide` taken wave-wide.
fn lowered(native: &NativeProgram, p: &ir::Program, wide: &HashSet<usize>) -> Result<HostShader, ShaderError> {
    lowered_for_width(native, p, wide, 32)
}

fn lowered_for_width(native: &NativeProgram, p: &ir::Program, wide: &HashSet<usize>, host_wave: u32) -> Result<HostShader, ShaderError> {
    lowered_for_dispatch(native, p, wide, host_wave, false)
}

fn lowered_for_dispatch(native: &NativeProgram, p: &ir::Program, wide: &HashSet<usize>, host_wave: u32, partial: bool) -> Result<HostShader, ShaderError> {
    let st = structure::structure_wide(p, wide).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let lowered = lower::lower_with_thread_limit(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    }, partial);
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered?;
    Ok(HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        written_root_uavs: lowered.written_root_uavs,
        writes_guest_memory: lowered.writes_guest_memory,
        root_indexed_views: res.root_indexed_views,
    })
}

#[test]
fn partial_thread_dispatch_masks_the_final_groups_memory_writes() {
    // Each active thread increments counter[local X]. With two groups the
    // tail's first N lanes report two increments, the others only one.
    let native = native(&[
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x7e04_0281, // v_mov_b32 v2, 1
        0xe0c8_1000, 0x8002_0201, // buffer_atomic_add v2, v1, s[8:11], 0 offen
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let shader = lowered_for_dispatch(&native, &p, &HashSet::new(), 64, true).unwrap();
    let limit = shader.user_data.iter().find(|u| u.root == RootType::Other(super::USER_DISPATCH_THREADS_X)).unwrap();
    assert_eq!((limit.slot, limit.dword), (0, 64));
    let mut validator = validator::Validator::load(validator::Validator::default_path().as_deref()).unwrap();
    validator.validate(&mut shader.dxil.clone()).unwrap();
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let (root, pso) = host.compute_pipeline(&shader).unwrap();
    for count in [0u32, 1, 7, 63, 64, 65, 74, 77, 127, 128] {
        let mut constants = vec![0; shader.user_data_dwords() as usize];
        constants[limit.index as usize] = count;
        let result = host.dispatch(&root, &pso, &Dispatch {
            roots: vec![(0, Root::Constants(constants))],
            views: &[View::RawUav(68)],
            samplers: &[],
            groups: [2, 1, 1],
        }).unwrap();
        let active = if count == 0 { 128 } else { count };
        for (i, &word) in result[0].iter().enumerate() {
            let expected = if i < 64 {
                u32::from((i as u32) < active) + u32::from((i as u32 + 64) < active)
            } else { 0 };
            assert_eq!(word, expected, "count {count}, word {i}");
        }
    }
}

/// Move a joined scalar mask through two SGPR pairs while only four lanes
/// execute. Both copied words must still describe all 64 console lanes.
#[test]
fn copied_mask_keeps_both_numeric_words_inside_a_lane_branch() {
    let native = native(&[
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE94_046A, // s_mov_b64 s[20:21], vcc
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0001, // s_cbranch_scc1 to join
        0xBE94_0402, // s_mov_b64 s[20:21], s[2:3]
        0xBE96_0414, // join: s_mov_b64 s[22:23], s[20:21]
        0x7D88_0084, // v_cmp_gt_u32 vcc, 4, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0009, // s_cbranch_execz to restore
        0xBE98_0416, // s_mov_b64 s[24:25], s[22:23]
        0xBE9A_0418, // s_mov_b64 s[26:27], s[24:25]
        0x87FE_1A7E, // s_and_b64 exec, exec, s[26:27]
        0x7E02_021A, // v_mov_b32 v1, s26
        0x7E04_021B, // v_mov_b32 v2, s27
        0xE070_0000, 0x8002_0100, // buffer_store_dword v1, off, s[8:11], 0
        0xE070_0004, 0x8002_0200, // buffer_store_dword v2, off, s[8:11], 4
        0xBEFE_040C, // restore: s_mov_b64 exec, s[12:13]
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let shader = lowered_for_width(&native, &p, &HashSet::new(), 64).expect("lower");
    if let Ok(mut validator) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        validator.validate(&mut shader.dxil.clone()).expect("validate");
    }
    let _lock = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else { eprintln!("no hardware compute device"); return };
    let Ok((root, pipeline)) = host.compute_pipeline(&shader) else {
        eprintln!("hardware does not take this 64-lane pipeline"); return;
    };
    for (constants, expected) in [
        ([37, 0, 0, 0], [u32::MAX, 31]),
        ([9, 0, 0, 0], [511, 0]),
        ([1, 1, 0x8000_0001, 0xa5a5_55aa], [0x8000_0001, 0xa5a5_55aa]),
        ([1, 1, 0x3456_7892, 0x8020_1000], [0x3456_7892, 0x8020_1000]),
    ] {
        let mut ud = vec![0; shader.user_data_dwords().max(1) as usize];
        for input in &shader.user_data {
            if input.root == RootType::Constants {
                ud[input.index as usize] = constants[input.dword as usize];
            }
        }
        let out = host.dispatch(&root, &pipeline, &Dispatch {
            roots: vec![(0, Root::Constants(ud))], views: &[View::RawUav(2)],
            samplers: &[], groups: [1,1,1],
        }).expect("dispatch and read both words");
        assert_eq!(out[0][..2], expected);
    }
    eprintln!("four native wave64 low/high word readbacks passed");
}

#[test]
fn a_sum_of_halves_over_the_wave_is_the_consoles_tree_bit_for_bit() {
    // thread t holds the 16-bit float with bits 0x3C00 + 37 t (1.0 and up, with odd
    // mantissas, so that additions round); all of them store the wave's sum
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    // (the wave OR's words with v_add_f16 for v_or_b32: opcode 50 for 28)
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(p.blocks.iter().flat_map(|b| &b.insts).filter(|i| i.name() == "v_add_f16").count(), 6);
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // the console's sum: lanes 2k and 2k+1, then pairs, ... each addition rounded to 16 bits
    // (to nearest, ties to even); values here are normal numbers
    let wide = |h: u16| f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13);
    let narrow = |f: f32| {
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let terms: Vec<u16> = (0..64u16).map(|t| 0x3C00 + 37 * t).collect();
    let mut tree = terms.clone();
    while tree.len() > 1 {
        tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
    }
    let in_a_row = terms.iter().copied().reduce(add).unwrap();
    assert_ne!(tree[0], in_a_row, "these terms tell the tree from another order");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return;
        }
    };
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    // (the high half is lane 0's term's: zero)
    assert_eq!(out[0][0], u32::from(tree[0]), "tree {:#x}, in a row {in_a_row:#x}", tree[0]);
    eprintln!("ran on the hardware device: the sum {:#x} of the console's tree", tree[0]);
}

/// One `v_add_f16` whose exact result lies between two halves: 1 + 3/1024 (0x3C03) plus 2
/// (0x4000) is 3 + 1.5 steps of that range, which the console rounds up to 0x4202. The
/// host's `f32tof16` may give 0x4201 (toward zero); with `VICTORY_F16_NEAREST=1` the
/// translation gives the console's whatever the host does.
#[test]
fn a_16_bit_sum_rounds_to_nearest_when_asked_to() {
    let words = [
        0x7E0E_02FF,
        0x0000_3C03, // v_mov_b32 v7, 0x3c03
        0x7E0C_02FF,
        0x0000_4000, // v_mov_b32 v6, 0x4000
        0x6400_0D07, // v_add_f16 v0, v7, v6
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert!(p.blocks.iter().flat_map(|b| &b.insts).any(|i| i.name() == "v_add_f16"));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for nearest in [false, true] {
        let was = lower::TEST_F16_NEAREST.replace(Some(nearest));
        let shader = lowered(&native, &p, &HashSet::new());
        lower::TEST_F16_NEAREST.set(was);
        let shader = shader.expect("lower");
        if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; not run");
            return;
        };
        let Ok((root, pso)) = host.compute_pipeline(&shader) else {
            eprintln!("the hardware device does not take this program; not run");
            return;
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let got = out[0][0] & 0xFFFF;
        eprintln!("v_add_f16 0x3c03 + 0x4000 on the hardware device, nearest {nearest}: {got:#x}");
        if nearest {
            assert_eq!(got, 0x4202);
        } else {
            assert!(got == 0x4201 || got == 0x4202, "{got:#x}");
        }
    }
}

#[test]
fn a_wave_or_inside_a_lane_branch_needs_the_branch_wave_wide() {
    for (skip, name) in [(0xBF88_0000, "s_cbranch_execz"), (0xBF84_0000, "s_cbranch_scc0")] {
        a_wave_or_inside(skip, name);
    }
}

fn a_wave_or_inside(skip: u32, name: &str) {
    let native = program(skip);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == name))
        .expect("the branch");
    // per lane, the two host waves cannot meet inside the branch
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a wave reduction where not every lane runs"),
        "{:?}",
        per_lane.err()
    );
    // ... and the lowering names the block, around which that branch is the one to widen
    let needs = lower::take_needs();
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &needs), Some(vec![branch]));
    // (taken, past the body, when every lane's bit is clear)
    assert_eq!(st.wave_any[branch], Some(false));
    // wave-wide, every lane runs the reduction (EXEC stays a guard on what it does there)
    let wide = HashSet::from([branch]);
    let st = structure::structure_wide(&p, &wide).expect("structure");
    let inside = match p.blocks[branch].exit {
        ir::Exit::Branch { not_taken, .. } => not_taken,
        _ => unreachable!(),
    };
    assert!(st.guards[inside].iter().any(Option::is_some));
    let shader = lowered(&native, &p, &wide).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // On a GPU that takes waves of 32 (two host waves for the console's one): the OR is of
    // both halves, whichever half's lanes take the branch.
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return;
        }
    };
    // (threads below s0 take the branch; thread t gives bit t >> 1)
    for (below, expected) in [(64u32, u32::MAX), (40, 0x000F_FFFF), (33, 0x0001_FFFF), (10, 0x1F), (1, 1)] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        assert_eq!(out[0][0], expected, "{name}: {below} threads in the branch");
    }
    eprintln!("ran on the hardware device ({name}): the OR of both halves in every case");
}

/// Runs `shader` on the hardware device with the root constants `s0`, `s1` and gives dword 0
/// of the UAV; None (and a line) without a device that takes it.
fn on_the_card(shader: &HostShader, constants: [u32; 2]) -> Option<u32> {
    whole_uav(shader, constants, 1).map(|out| out[0])
}

/// As [`on_the_card`], and gives the first `dwords` dwords of the UAV.
fn whole_uav(shader: &HostShader, constants: [u32; 2], dwords: u32) -> Option<Vec<u32>> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(dwords)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0].clone())
}

/// The fewest lanes a wave of the device [`on_the_card`] runs on has, if there is one.
fn wave_lanes() -> Option<u32> {
    use windows::Win32::Graphics::Direct3D12::{D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1};
    let host = Host::new(false).ok()?;
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: a plain query that fills a structure of the given size.
    unsafe {
        host.device
            .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS1, (&raw mut options).cast(), size_of_val(&options) as u32)
            .ok()?
    };
    Some(options.WaveLaneCountMin)
}

/// The FidelityFX upscaler's wait (17af85f170e66f30, 0x40-0x22c), on a group of one console
/// wave64: `v_cmpx exec` on a range computed from the wave's first thread, `v0 - v_mbcnt(-1)`
/// (here plus s0: entered when s0 is not 0), then a loop "while some lane still sees busy (its
/// thread below s1) and fewer than s2 tries" on SCC of `s_and_b64 ..., exec` read by
/// `s_cselect`. Every lane stores the number of tries. On the console the wave decides
/// together: s2 tries once any lane is busy, whichever half it is in; one try with none.
fn the_wait(entry: u32) -> NativeProgram {
    native(&[
        0xD765_0001,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v1, -1, 0
        0xD766_0001,
        0x0002_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1
        0x4C04_0300, // v_sub_nc_u32 v2, v0, v1: the wave's first thread (0)
        0x4A04_0400, // v_add_nc_u32 v2, s0, v2
        entry,       // v_cmpx_ne_u32 exec, 0, v2 (or another entry)
        0xBF88_000D, // s_cbranch_execz to the end
        0xBE90_0380, // s_mov_b32 s16, 0
        0x7D88_0001, // head: v_cmp_gt_u32 vcc, s1, v0 (busy)
        0x8794_7E6A, // s_and_b64 s[20:21], vcc, exec
        0x8594_80C1, // s_cselect_b64 s[20:21], -1, 0
        0x8010_8110, // s_add_u32 s16, s16, 1
        0xBF09_0210, // s_cmp_ge_u32 s16, s2
        0x8596_80C1, // s_cselect_b64 s[22:23], -1, 0
        0x8B94_1416, // s_orn2_b64 s[20:21], s[22:23], s[20:21]
        0xBF84_FFF8, // s_cbranch_scc0 head
        0x7E08_0210, // v_mov_b32 v4, s16
        0x340A_0082, // v_lshlrev_b32 v5, 2, v0
        0xE070_1000,
        0x8002_0405, // buffer_store_dword v4, v5, s[8:11], 0 offen
    ])
}

/// [`the_wait`] on the hardware device with s0..s2: what each of the 64 threads stored.
fn the_wait_on_the_card(shader: &HostShader, constants: [u32; 3]) -> Option<Vec<u32>> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=2) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(64)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..64].to_vec())
}

#[test]
fn a_wait_the_wave_enters_together_exchanges_inside_its_lane_branch() {
    let native = the_wait(0x7DAA_0480);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    // the entry is a per-lane branch to the structuriser; the lowering finds that every lane
    // computes its condition alike
    let st = structure::structure(&p).expect("structure");
    let entry = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == "s_cbranch_execz"))
        .expect("the entry");
    assert!(!st.wide.contains(&entry));
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // (s0: entered unless 0; s1: threads below it busy; s2: the limit of tries)
    for (s, tries) in [([1u32, 0, 9], 1u32), ([1, 10, 5], 5), ([1, 40, 3], 3), ([1, 64, 7], 7), ([0, 10, 5], 0)] {
        let Some(got) = the_wait_on_the_card(&shader, s) else { return };
        assert!(got.iter().all(|&g| g == tries), "s0..s2 {s:?}: got {got:?}, the console's {tries} in every lane");
    }
    eprintln!("ran on the hardware device: the console's number of tries in both halves");
}

/// [`the_wait`] entered on a lane's own thread number (`v_cmpx_ne_u32 exec, 0, v0`): the
/// lanes go their own ways, and the halves cannot meet inside.
#[test]
fn a_wait_entered_per_lane_is_still_refused() {
    let native = the_wait(0x7DAA_0080);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask exchanged where not every lane runs"),
        "{:?}",
        per_lane.err()
    );
}

/// A 32-lane console wave. A register holds a lane mask on one path (the lanes below s0)
/// and a number on the other, and is read as a number after the join, as the trace programs
/// keep their lanes' masks and their node numbers on one stack
/// (`deferred_tiled:CS_LaserLightShadowRaytracing`, s46 at 0x104, 0x75c and 0x188). The
/// mask arrives as the ballot of its lanes.
#[test]
fn a_lane_mask_joined_with_a_number_is_read_as_the_number_it_is() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE94_036A, // s_mov_b32 s20, vcc_lo
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0001, // s_cbranch_scc1 over the next
        0xBE94_0385, // s_mov_b32 s20, 5
        0x7E00_0214, // v_mov_b32 v0, s20
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // The program is a 32-lane wave's: a device whose waves are narrower
    // (the software adapter's have 4 lanes) runs it as several waves, and
    // a ballot of 32 lanes is no mask of one.
    if let Some(lanes) = wave_lanes().filter(|&lanes| lanes < 32) {
        eprintln!("the device's waves have {lanes} lanes, fewer than this program's 32; not run");
        return;
    }
    for (below, other, expected) in [(32u32, 0u32, u32::MAX), (10, 0, 0x3FF), (0, 0, 0), (10, 1, 5)] {
        let Some(got) = on_the_card(&shader, [below, other]) else { return };
        assert_eq!(got, expected, "{below} lanes in the mask, s1 = {other}");
    }
    eprintln!("ran on the hardware device: the mask's bits, or the number");
}

/// A 32-lane console wave. `if (t < s0) { ... SCC = (s1 == 7) }` and a branch on SCC after
/// the join: the skip brings the SCC of `s_and_saveexec` ("some lane is left") along
/// (`rtdirectionalshadows:CS_RTShadowTrace32HQ_TiledFarOnly`, 0x360-0x39c). On the console
/// the skip is taken by the wave, so with some lane inside every lane sees the inner test.
/// A lane that skipped on its own would see "some lane is left" instead: refused per lane,
/// right with the branch taken wave-wide. Lane 31 stores what it saw.
#[test]
fn scc_of_a_mask_operation_joined_with_a_scalar_test_needs_the_skip_wave_wide() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE8D_3C6A, // s_and_saveexec_b32 s13, vcc_lo
        0xBF88_0001, // s_cbranch_execz over the next
        0xBF06_8701, // s_cmp_eq_u32 s1, 7
        0xBEFE_030D, // s_mov_b32 exec_lo, s13
        0xBF85_0002, // s_cbranch_scc1 to "seven"
        0x7E0A_0281, // v_mov_b32 v5, 1
        0xBF82_0001, // s_branch over the next
        0x7E0A_0282, // seven: v_mov_b32 v5, 2
        0x7DA4_009F, // v_cmpx_eq_u32 exec_lo, 31, v0
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16 | 5 << 8, // buffer_store_dword v5, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == "s_cbranch_execz"))
        .expect("the skip");
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask read as data"),
        "{:?}",
        per_lane.err()
    );
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &lower::take_needs()), Some(vec![branch]));
    let shader = lowered(&native, &p, &HashSet::from([branch])).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // (lanes below s0 inside, s1) -> what lane 31 stores: 2 if SCC was set at the branch
    for (below, s1, expected) in [(0u32, 7u32, 1u32), (10, 7, 2), (10, 3, 1), (32, 3, 1), (32, 7, 2)] {
        let Some(got) = on_the_card(&shader, [below, s1]) else { return };
        assert_eq!(got, expected, "{below} lanes inside, s1 = {s1}");
    }
    eprintln!("ran on the hardware device: the inner test's SCC for every lane once some lane is inside");
}

/// The particle and water programs' idiom (369cbe906ea72764 at 0x494, 5dc3b25986622949 at
/// 0x230-0x4c4): in a group of one console wave64, inside a per-lane branch, a lane writes
/// LDS and another lane of the same wave reads it with no barrier between, relying on the
/// wave's lockstep; then a loop closed by a plain jump (`s_branch` back, its exit an
/// `s_cbranch_execz` inside) that lanes leave one by one, adding to an LDS counter at every
/// trip, and a read of the counter after it. Lane t writes t to LDS[t], reads LDS[t ^ 32]
/// (the other host half's lane), makes (t >> 4) + 1 trips (the halves leave the loop after
/// different trips) and stores LDS[t ^ 32] + (trips << 16) + (all lanes' trips << 24).
fn lds_in_a_lane_branch() -> NativeProgram {
    let words = [
        0x7E0E_0280, // v_mov_b32 v7, 0
        0x7E10_0280, // v_mov_b32 v8, 0
        0xD834_0100,
        0x0000_0807, // ds_write_b32 v7, v8 offset:256: the counter, 0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0: the lanes below s0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_001F, // s_cbranch_execz to the end
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xD834_0000,
        0x0000_0001, // ds_write_b32 v1, v0
        0x3A04_00A0, // v_xor_b32 v2, 32, v0
        0x3404_0482, // v_lshlrev_b32 v2, 2, v2
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD8D8_0000,
        0x0300_0002, // ds_read_b32 v3, v2
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x7E08_0280, // v_mov_b32 v4, 0
        0x2C0A_0084, // v_lshrrev_b32 v5, 4, v0
        0x7E0C_0281, // v_mov_b32 v6, 1
        0xBE8E_047E, // s_mov_b64 s[14:15], exec
        0x4A08_0881, // head: v_add_nc_u32 v4, 1, v4
        0xD800_0100,
        0x0000_0607, // ds_add_u32 v7, v6 offset:256
        0x7D88_0B04, // v_cmp_gt_u32 vcc, v4, v5
        0x8AFE_6A7E, // s_andn2_b64 exec, exec, vcc
        0xBF88_0001, // s_cbranch_execz to the loop's exit
        0xBF82_FFF9, // s_branch head
        0xBEFE_040E, // exit: s_mov_b64 exec, s[14:15]
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD8D8_0100,
        0x0900_0007, // ds_read_b32 v9, v7 offset:256
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x3408_0890, // v_lshlrev_b32 v4, 16, v4
        0x4A06_0903, // v_add_nc_u32 v3, v3, v4
        0x3412_1298, // v_lshlrev_b32 v9, 24, v9
        0x4A06_1303, // v_add_nc_u32 v3, v3, v9
        0xE070_1000,
        0x8002_0301, // buffer_store_dword v3, v1, s[8:11], 0 offen
        0xBEFE_040C, // end: s_mov_b64 exec, s[12:13]
    ];
    let mut native = native(&words);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15; // 512 bytes of LDS
    }
    native
}

/// [`lds_in_a_lane_branch`]: kept on 64-lane waves per lane (the LDS write and read share a
/// lane-branch region, `lower::take_lds_region`; its loop closed by a jump lies in that
/// branch, [`lower::loops_together`]), translated by default for 32-lane waves, and on a card
/// of 32-lane waves every lane reads what the other half's lane wrote, counts its own trips
/// and reads every lane's.
#[test]
fn lds_shared_in_a_lane_branch_runs_on_two_host_waves() {
    let native = lds_in_a_lane_branch();
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let io = io::raise(&p, &native).expect("io::raise");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    lower::take_lds_region();
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let per_lane = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    let region = lower::take_lds_region();
    let header = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.first().is_some_and(|i| i.name() == "v_add_nc_u32"))
        .expect("the loop");
    assert_eq!(lower::loops_together(&p, &st), Some(vec![header]));
    // the whole translation
    let bytes = crate::dxbc::build(&[]);
    let c = crate::dxbc::Container::parse(&bytes).expect("container");
    let shader = super::emit(&c, &native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    assert!(per_lane.is_ok_and(|l| l.wave_size == Some(64)), "per lane: 64-lane waves");
    assert!(region, "kept on 64 lanes for the LDS region");
    let shader = shader.expect("translated");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    lds_in_a_lane_branch_on_the_card(&shader);
}

/// [`lds_in_a_lane_branch`] on a hardware device of 32-lane waves, against the console's
/// results for 64, 48, 40 and 1 lanes in the branch.
fn lds_in_a_lane_branch_on_the_card(shader: &HostShader) {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = host.compute_pipeline(shader).expect("a pipeline of 32-lane waves");
    for below in [64u32, 48, 40, 1] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(64)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let total: u32 = (0..below).map(|l| (l >> 4) + 1).sum();
        for t in 0..64u32 {
            let got = out[0][t as usize];
            let trips = (t >> 4) + 1;
            if t >= below {
                assert_eq!(got, 0, "lane {t} is outside the branch ({below} inside)");
            } else if t ^ 32 < below {
                let want = (t ^ 32) | trips << 16 | total << 24;
                assert_eq!(got, want, "lane {t} of {below}: {got:#x}, the console's {want:#x}");
            } else {
                assert_eq!(got >> 16, trips | total << 8, "lane {t} of {below}: its trips and all lanes'");
            }
        }
    }
    eprintln!("ran on the hardware device: every lane read the other half's write and every lane's trips");
}

/// The wave-wide form itself ([`super::widen`]'s) of [`lds_in_a_lane_branch`]: the branch
/// around the LDS accesses and the loop's exit taken wave-wide, so that the wave enters and
/// leaves the loop together (each half asks the other whether a lane is left), the halves
/// meet before every LDS access, inside the loop too, as often as each other, and the result
/// is the console's. (With the loop per lane the halves met at different trips: the read
/// after it missed the upper half's last adds, 144 for 160.)
#[test]
fn lds_shared_in_a_lane_branch_taken_wave_wide_with_its_loop() {
    let native = lds_in_a_lane_branch();
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let entry = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == "s_cbranch_execz"))
        .expect("the branch");
    let wide = HashSet::from([entry]);
    let st = structure::structure_wide(&p, &wide).expect("structure");
    let header = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.first().is_some_and(|i| i.name() == "v_add_nc_u32"))
        .expect("the loop");
    // the loop's exit test closes its first block, which the lanes that stay run again: a
    // lane loop still, which `widen` takes wave-wide too
    assert_eq!(lower::loops_together(&p, &st), Some(vec![header]));
    assert_eq!(lower::branches_around(&st, &[header]), Some(vec![header]));
    let wide = HashSet::from([entry, header]);
    let st = structure::structure_wide(&p, &wide).expect("structure");
    assert_eq!(lower::loops_together(&p, &st), Some(vec![]));
    let shader = lowered(&native, &p, &wide).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    lds_in_a_lane_branch_on_the_card(&shader);
}

/// The prefix of wave totals the console's structure builder computes: each console
/// wave of a 256-thread group writes its total (10 + its index) to LDS, then, under
/// `if (id >> 6 != 0)`, loops over the totals of the waves before it with lanes leaving at their
/// own wave's index, reading the running sum's first lane (`v_readfirstlane`) every iteration.
/// (The total is read from the wave's last lane first, as the builder's scan does: a read
/// across the two host waves, which makes them exchange.)
/// The condition and the sum are the same in every lane of a console wave, so each host wave
/// takes its own first lane; before, the read was refused ("a first-lane read where not every
/// lane runs"), which kept the builder off on 32-lane GPUs. Every thread stores the sum of the
/// waves before its own: 0, 10, 10 + 11, 10 + 11 + 12.
fn wave_prefix_program(lane_dependent: bool) -> NativeProgram {
    let lds_address = if lane_dependent {
        0x3410_0082 // v_lshlrev_b32 v8, 2, v0
    } else {
        0x7E10_0210 // v_mov_b32 v8, s16
    };
    let words = [
        0x2C0E_0086, // v_lshrrev_b32 v7, 6, v0             (the console wave's index)
        0x3410_0E82, // v_lshlrev_b32 v8, 2, v7
        0x4A12_0E8A, // v_add_nc_u32 v9, 10, v7
        0xD760_0013,
        0x0201_7F09, // v_readlane_b32 s19, v9, 63   (the wave's last lane: the other host wave)
        0x7E12_0213, // v_mov_b32 v9, s19
        0xD834_0000,
        0x0000_0908, // ds_write_b32 v8, v9
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xBF8A_0000, // s_barrier
        0x7E10_0280, // v_mov_b32 v8, 0
        0xBE80_047E, // s_mov_b64 s[0:1], exec
        0x7DAA_0E80, // v_cmpx_ne_u32 0, v7
        0xBF88_0011, // s_cbranch_execz END
        0xBE86_047E, // s_mov_b64 s[6:7], exec
        0xBE92_0380, // s_mov_b32 s18, 0
        0xBE91_0380, // s_mov_b32 s17, 0
        0xBF82_0002, // s_branch BODY
        0x87FE_0614, // LOOPTOP: s_and_b64 exec, s[20:21], s[6:7]
        0x7E22_0508, // v_readfirstlane_b32 s17, v8
        0x8F10_8212, // BODY: s_lshl_b32 s16, s18, 2
        0x8112_8112, // s_add_i32 s18, s18, 1
        lds_address, // LDS address for the next subtotal
        0xD8D8_0000,
        0x0800_0008, // ds_read_b32 v8, v8
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x4A10_1011, // v_add_nc_u32 v8, s17, v8
        0xBE94_047E, // s_mov_b64 s[20:21], exec
        0x7DA4_0E12, // v_cmpx_eq_u32 s18, v7
        0x8A86_7E06, // s_andn2_b64 s[6:7], s[6:7], exec
        0xBF85_FFF3, // s_cbranch_scc1 LOOPTOP
        0xBEFE_0400, // END: s_mov_b64 exec, s[0:1]
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xE070_1000,
        0x8002_0801, // buffer_store_dword v8, v1, s[8:11], 0 offen
    ];
    let mut native = native_of(&words, true, 256);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 2 << 15; // 1 KiB covers the lane-varying address as well.
    }
    native
}

#[test]
fn a_first_lane_read_of_a_wave_alike_sum_under_a_wave_alike_loop() {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
    };

    let native = wave_prefix_program(false);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert!(
        p.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| i.name() == "v_readfirstlane_b32")
    );
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
        eprintln!("DXIL validator accepted the wave32 prefix shader");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let mut waves = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: the output has the layout and size required by OPTIONS1.
    unsafe {
        host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut waves).cast(),
            size_of_val(&waves) as u32,
        )
    }.expect("query wave capabilities");
    if !(waves.WaveLaneCountMin..=waves.WaveLaneCountMax).contains(&32) {
        eprintln!("the device does not support wave32; hardware dispatch skipped");
        return;
    }
    let (root, pso) = host.compute_pipeline(&shader).expect("wave32 prefix pipeline");
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(256)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    let expected = |t: u32| (0..(t >> 6)).map(|w| 10 + w).sum::<u32>();
    let got: Vec<u32> = (0..256).map(|t| out[0][t]).collect();
    for t in 0..256u32 {
        assert_eq!(got[t as usize], expected(t), "thread {t}: {got:?}");
    }
    eprintln!("ran on the hardware device: every console wave got the sum of the waves before it");
}

#[test]
fn a_lane_dependent_prefix_cannot_take_its_own_first_lane() {
    let native = wave_prefix_program(true);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let Err(error) = lowered(&native, &p, &HashSet::new()) else {
        panic!("the two host waves may read different subtotals");
    };
    assert!(error.to_string().contains("a first-lane read where not every lane runs"), "{error}");
}

/// A spilled 64-thread wave64 compute program in a partial final group
/// (`partial`, `USER_DISPATCH_THREADS_X`), as the structure builder's Box
/// rehydration kernels are: it reads `s_bcnt1_i32_b64(exec)` on entry,
/// takes a whole-wave OR of `1 << (lane >> 1)` (which crosses the 32-lane
/// halves), and stores `(count, or)` at `uav[lane * 2..+2]` for every lane.
///
/// On a host of 32-lane waves it must run as two host waves (waves of 32,
/// so the pipeline is made: NVIDIA refuses `WaveSize(64)`), and both must
/// stay alive for the barriers of the halves' exchange while the lanes past
/// the limit count for nothing and store nothing (adapted from
/// fxceless/victory-fork#646's test).
#[test]
fn partial_dispatch_with_a_cross_half_wave_op_runs_in_32_lane_waves() {
    let mut words = vec![
        // Read descriptor-table offset (user_data 16) from spilled table s[0:1] + 0.
        0x8703_FF01,
        0x0000_00FF, // s_and_b32 s3, s1, 0xff
        0xBE82_0300, // s_mov_b32 s2, s0
        0x3D << 26 | 4 << 6 | 1,
        125 << 25, // s_load_dword s4, s[2:3], 0
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
        0xBE86_107E, // s_bcnt1_i32_b64 s6, exec
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
    ];
    words.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    words.extend_from_slice(&[
        0x3402_0083, // v_lshlrev_b32 v1, 3, v0 (lane * 8 bytes)
        0x7E04_0206, // v_mov_b32 v2, s6 (popcnt(exec))
        0x7E06_0207, // v_mov_b32 v3, s7 (wave OR)
        0x38 << 26 | 0x1D << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 2 << 8 | 1, // buffer_store_dwordx2 v[2:3], v1, s[8:11], 0 offen
        0xBF81_0000, // s_endpgm
    ]);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 16,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 16,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 16,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo { rsrc2: 2 << 1, threads: [64, 1, 1] }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = crate::spill::build(&native).expect("spill::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let lowered = lower::lower_with_thread_limit(
        &lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        },
        true,
    );
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered.unwrap_or_else(|e| panic!("lower: {e}"));
    // NVIDIA (32 lanes) refuses WaveSize(64): the program must not keep the console's.
    assert_eq!(lowered.wave_size, Some(32), "the partial program kept 64-lane waves");
    let limit = *lowered
        .user_data
        .iter()
        .find(|u| u.root == RootType::Other(super::USER_DISPATCH_THREADS_X))
        .expect("USER_DISPATCH_THREADS_X");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        written_root_uavs: lowered.written_root_uavs,
        writes_guest_memory: lowered.writes_guest_memory,
        root_indexed_views: res.root_indexed_views.clone(),
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).unwrap_or_else(|e| panic!("DXIL validation: {e}"));
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else { return };
    let (root, pso) = host.compute_pipeline(&shader).expect("the host makes the 32-lane pipeline");
    for (count, active, expected_or) in [(45u32, 45usize, 0x007F_FFFFu32), (1, 1, 1), (33, 33, 0x1_FFFF), (0, 64, u32::MAX)] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        ud[limit.index as usize] = count;
        let out = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(128)],
                samplers: &[],
                groups: [1, 1, 1],
            })
            .unwrap_or_else(|e| panic!("dispatch (count {count}): {e}"));
        for lane in 0..64 {
            let got = (out[0][lane * 2], out[0][lane * 2 + 1]);
            let want = if lane < active { (active as u32, expected_or) } else { (0, 0) };
            assert_eq!(got, want, "count {count}, lane {lane}");
        }
    }
    eprintln!("ran on the hardware device: partial counts 45, 1, 33, 0 exact in 32-lane waves");
}

/// One console wave of 64 threads with 512 bytes of LDS. The threads clear
/// the flag, then the threads below s0 raise it and read it back inside
/// their branch. Every thread stores the final flag plus one at its own
/// UAV offset. `second` adds another write in the branch after the read.
fn flag_program(second: Option<u32>) -> NativeProgram {
    let mut body = vec![
        0x7E12_0280, // v_mov_b32 v9, 0
    ];
    body.extend_from_slice(&ds(DS_WRITE_B32, 0, 9, 9)); // ds_write_b32 v9, v9
    let mut branch = vec![0x7E04_0281]; // v_mov_b32 v2, 1
    branch.extend_from_slice(&ds(DS_WRITE_B32, 0, 9, 2)); // ds_write_b32 v9, v2
    // (a scalar exit from the branch makes it blocks of their own, which only its lanes run)
    let mut rest = ds(DS_READ_B32, 3, 9, 0).to_vec(); // ds_read_b32 v3, v9
    if let Some(k) = second {
        rest.push(0x7E0C_02FF); // v_mov_b32 v6, lit
        rest.push(k);
        rest.extend_from_slice(&ds(DS_WRITE_B32, 0, 9, 6)); // ds_write_b32 v9, v6
    }
    branch.extend_from_slice(&[
        0xBF07_8001,                       // s_cmp_lg_u32 s1, 0
        0xBF85_0000 | rest.len() as u32, // s_cbranch_scc1 past the rest
    ]);
    branch.extend_from_slice(&rest);
    body.extend_from_slice(&[
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | branch.len() as u32, // s_cbranch_execz past the branch
    ]);
    body.extend_from_slice(&branch);
    body.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    body.extend_from_slice(&ds(DS_READ_B32, 5, 9, 0)); // ds_read_b32 v5, v9
    body.extend_from_slice(&[
        0x4A0A_0A81, // v_add_nc_u32 v5, 1, v5
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 5 << 8 | 1, // buffer_store_dword v5, v1, s[8:11], 0 offen
    ]);
    let mut native = native(&body);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15; // 512 bytes of LDS
    }
    native
}

/// DS `op vdst, addr, data0` at offset 0.
const fn ds(op: u8, vdst: u8, addr: u8, data0: u8) -> [u32; 2] {
    [0x36 << 26 | (op as u32) << 18, (vdst as u32) << 24 | (data0 as u32) << 8 | addr as u32]
}

const DS_WRITE_B32: u8 = 0x0D;
const DS_READ_B32: u8 = 0x36;

/// The wave size `native` lowers to on a host of 32-lane waves.
fn flag_wave(native: &NativeProgram) -> Option<u32> {
    let p = crate::spill::build(native).expect("build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let lowered = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    lowered.expect("lower").wave_size
}

#[test]
fn a_branch_that_only_raises_an_lds_flag_runs_in_waves_of_32() {
    // a read that misses the other host wave's write changes nothing the branch leaves behind
    assert_eq!(flag_wave(&flag_program(None)), Some(32));
    // after the read the branch writes the flag again, with the same constant
    assert_eq!(flag_wave(&flag_program(Some(1))), Some(32));
}

#[test]
fn a_branch_that_writes_lds_two_different_values_keeps_64_lanes() {
    // a second value after the read: what the flag ends up holding depends on what was read
    assert_eq!(flag_wave(&flag_program(Some(2))), Some(64));
}

#[test]
fn both_host_waves_observe_the_lds_flag_after_the_join() {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
    };

    let native = flag_program(Some(1));
    let p = crate::spill::build(&native).expect("build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut validator) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut dxil = shader.dxil.clone();
        validator.validate(&mut dxil).expect("validate the split wave shader");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware adapter, skipping flag dispatches");
        return;
    };
    let mut waves = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: the output points at a feature structure of the specified size.
    unsafe {
        host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut waves).cast(),
            size_of_val(&waves) as u32,
        )
    }
    .expect("query host wave sizes");
    if !waves.WaveOps.as_bool() || waves.WaveLaneCountMax != 32 {
        eprintln!("not a 32-lane host, skipping flag dispatches");
        return;
    }
    let (root, pso) = host.compute_pipeline(&shader).expect("create the split wave pipeline");
    for active in [0, 1, 31, 32, 33, 63, 64] {
        // Exercise both sides of the branch's scalar skip.
        for skip in [0, 1] {
            let mut ud = vec![0; shader.user_data_dwords() as usize];
            for u in &shader.user_data {
                if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
                    ud[u.index as usize] = [active, skip][d as usize];
                }
            }
            let out = host
                .dispatch(&root, &pso, &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(64)],
                    samplers: &[],
                    groups: [1, 1, 1],
                })
                .expect("dispatch the split wave pipeline");
            assert_eq!(out[0], [1 + u32::from(active != 0); 64], "{active} active lanes, skip {skip}");
        }
    }
    eprintln!("all 14 flag dispatches passed on a 32-lane hardware adapter");
}

/// A wave64 program of 128 threads (two console waves) that leaves early when no lane of its
/// wave is below s0, as the console's compiler does when the end of the program is too far for
/// a branch: `s_cbranch_execnz` over a far jump to the end. `extra` are words that go into the
/// far jump's block before it, and `work_at_end` puts an instruction between the jump's target
/// and the end of the program. Then the gate: every thread stores 1, or 2 when some lane of its
/// console wave that is still running has the thread number s1 (the lanes that fail a test).
fn far_early_out(extra: &[u32], work_at_end: bool) -> NativeProgram {
    let gate = [
        0x7D84_0001, // v_cmp_eq_u32 vcc, s1, v0
        0xBE90_106A, // s_bcnt1_i32_b64 s16, vcc
        0xBF06_8010, // s_cmp_eq_u32 s16, 0
        0x8511_8281, // s_cselect_b32 s17, 1, 2
        0x7E02_0211, // v_mov_b32 v1, s17
        0x3404_0082, // v_lshlrev_b32 v2, 2, v0
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 1 << 8 | 2, // buffer_store_dword v1, v2, s[8:11], 0 offen
    ];
    let jump = [
        0xBE8E_1F00, // s_getpc_b64 s[14:15]
        0x800E_FF0E,
        4 * (4 + gate.len() as u32), // s_add_u32 s14, s14, the way to the end
        0x820F_800F, // s_addc_u32 s15, s15, 0
        0xBE80_200E, // s_setpc_b64 s[14:15]
    ];
    let mut words = vec![
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF89_0000 | (extra.len() + jump.len()) as u32, // s_cbranch_execnz over the jump
    ];
    words.extend_from_slice(extra);
    words.extend_from_slice(&jump);
    words.extend_from_slice(&gate);
    if work_at_end {
        words.push(0x7E06_0281); // v_mov_b32 v3, 1
    }
    native_of(&words, true, 128)
}

fn branches_named(p: &ir::Program, name: &str) -> usize {
    p.blocks.iter().filter(|b| b.insts.last().is_some_and(|i| i.name() == name)).count()
}

/// What the gate of [`far_early_out`] stores in each of 128 threads.
fn gate_expected(below: u32, failing: u32) -> Vec<u32> {
    (0..128u32)
        .map(|t| {
            let some_fail = failing < below && failing / 64 == t / 64;
            match (t < below, some_fail) {
                (false, _) => 0,
                (true, false) => 1,
                (true, true) => 2,
            }
        })
        .collect()
}

#[test]
fn a_far_early_out_is_left_out_and_the_gate_counts_the_lanes_of_both_halves() {
    let native = far_early_out(&[], false);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(branches_named(&p, "s_cbranch_execnz"), 1);
    lower::take_needs();
    let kept = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&kept, Err(ShaderError::Unsupported { what, .. }) if what.ends_with("where not every lane runs")),
        "{:?}",
        kept.err()
    );
    for (name, form) in [("early-outs", ir::without_early_outs(&p)), ("skips", ir::without_skips(&p, false))] {
        let form = form.unwrap_or_else(|| panic!("without {name}: the far early-out stays"));
        assert_eq!(branches_named(&form, "s_cbranch_execnz"), 0, "without {name}");
    }
    let form = ir::without_early_outs(&p).expect("left out");
    let shader = lowered(&native, &form, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // (threads below s0 are in bounds; the one thread s1 fails the test) A failing thread of the
    // upper half gives 2 to the lower half's threads as well, and a failing thread out of
    // bounds counts for nothing
    for (below, failing) in [(128u32, 40u32), (128, 100), (128, 63), (40, 50), (40, 10), (33, 32), (1, 0), (0, 5)] {
        let Some(got) = whole_uav(&shader, [below, failing], 128) else { return };
        assert_eq!(got, gate_expected(below, failing), "{below} threads in bounds, thread {failing} fails");
    }
    eprintln!("ran on the hardware device: the gate's count over both halves of each console wave");
}

#[test]
fn a_far_jump_that_does_work_is_not_an_early_out() {
    // work in the jump's block, and work between the jump's target and the end
    for (name, native) in [
        ("work in the jump", far_early_out(&[0xBE94_0385], false)), // s_mov_b32 s20, 5
        ("work at the target", far_early_out(&[], true)),
    ] {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        assert_eq!(branches_named(&p, "s_cbranch_execnz"), 1, "{name}");
        assert!(ir::without_early_outs(&p).is_none(), "{name}");
        assert!(ir::without_skips(&p, false).is_none(), "{name}");
        assert!(ir::without_skips(&p, true).is_none(), "{name}");
    }
}

/// `if (t < s0) { park s[20:21]; if (s1 == 0) {..} else {..}; take it back }` and a read of
/// s[20:21] after the join, as the console's compiler leaves a parked scalar across an inner
/// branch. `then` is the inner branch's first arm; the second one leaves the parked scalar alone.
fn parked_across_an_inner_branch(then: u32) -> ir::Program {
    let body = [
        0xBE96_0414, // s_mov_b64 s[22:23], s[20:21]
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0002, // s_cbranch_scc1 to the second arm
        then,
        0xBF82_0001, // s_branch to the join
        0xBE98_0382, // s_mov_b32 s24, 2
        0xBE94_0416, // s_mov_b64 s[20:21], s[22:23]
    ];
    let mut words = vec![
        0xBE94_0406, // s_mov_b64 s[20:21], s[6:7]
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | body.len() as u32, // s_cbranch_execz past the body
    ];
    words.extend_from_slice(&body);
    words.extend_from_slice(&[
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7E02_0214, // v_mov_b32 v1, s20
    ]);
    let native = native(&words);
    ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build")
}

#[test]
fn a_skip_whose_parked_scalars_pass_an_inner_branch_unchanged_is_left_out() {
    let p = parked_across_an_inner_branch(0xBE96_0314); // s_mov_b32 s22, s20
    assert_eq!(branches_named(&p, "s_cbranch_execz"), 1);
    // (s22 comes back through a phi of the inner join whose two arguments are copies of one
    // value: only `deep` follows it)
    assert!(ir::without_skips(&p, false).is_none());
    let deep = ir::without_skips(&p, true).expect("left out");
    assert_eq!(branches_named(&deep, "s_cbranch_execz"), 0);
}

#[test]
fn a_skip_whose_parked_scalar_an_inner_branch_changes_is_kept() {
    let p = parked_across_an_inner_branch(0xBE96_0387); // s_mov_b32 s22, 7
    assert_eq!(branches_named(&p, "s_cbranch_execz"), 1);
    assert!(ir::without_skips(&p, false).is_none());
    assert!(ir::without_skips(&p, true).is_none());
}

/// `v_writelane_b32 vN, sX, K`.
fn writelane(n: u32, x: u32, lane: u32) -> Vec<u32> {
    vec![0xD761_0000 | n, (128 + lane) << 9 | x]
}

/// `v_readlane_b32 sX, vN, K`.
fn readlane(x: u32, n: u32, lane: u32) -> Vec<u32> {
    vec![0xD760_0000 | x, (128 + lane) << 9 | (256 + n)]
}

fn parked(words: &[Vec<u32>]) -> ir::Program {
    let native = native(&words.concat());
    ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build")
}

fn instructions<'a>(p: &'a ir::Program, name: &str) -> Vec<&'a ir::Inst> {
    p.blocks.iter().flat_map(|b| &b.insts).filter(|i| i.name() == name).collect()
}

fn regs<T>(of: &[(ir::Reg, T)]) -> Vec<ir::Reg> {
    of.iter().map(|r| r.0).collect()
}

#[test]
#[ignore = "needs the source tree's parked-pair lifetime tracking in `unspill`, which this tree does not have"]
fn a_pair_parked_in_lanes_that_hold_other_scalars_elsewhere_moves_as_a_pair() {
    // s[20:21] goes into lanes 4 and 5 of v10 and comes back into s[22:23]; later lane 4
    // holds s30 for a while: no lane of the two is the pair's alone
    let p = parked(&[
        writelane(10, 20, 4),
        writelane(10, 21, 5),
        vec![0x7E04_0281], // v_mov_b32 v2, 1
        readlane(22, 10, 4),
        readlane(23, 10, 5),
        writelane(10, 30, 4),
        readlane(31, 10, 4),
    ]);
    let wide = instructions(&p, "s_mov_b64");
    let [save, restore] = wide[..] else { panic!("one pair save and one pair restore: {} moves", wide.len()) };
    assert_eq!(regs(&save.reads), [ir::Reg::S(20), ir::Reg::S(21)]);
    assert_eq!(regs(&restore.writes), [ir::Reg::S(22), ir::Reg::S(23)]);
    assert_eq!(regs(&restore.reads), regs(&save.writes));
    // the single scalar has its own lane register, not the pair's
    let narrow = instructions(&p, "s_mov_b32");
    let lone_save = narrow.iter().find(|i| regs(&i.reads) == [ir::Reg::S(30)]).expect("the save of s30");
    let lone_restore = narrow.iter().find(|i| regs(&i.writes) == [ir::Reg::S(31)]).expect("the restore into s31");
    assert_eq!(regs(&lone_restore.reads), regs(&lone_save.writes));
    assert!(regs(&lone_save.writes).iter().all(|r| !regs(&save.writes).contains(r)));
}

#[test]
fn a_parked_pair_another_save_or_restore_shares_stays_two_scalars() {
    let cases = [
        (
            "a lone restore of the lane",
            vec![
                writelane(10, 20, 4),
                writelane(10, 21, 5),
                readlane(22, 10, 4),
                readlane(23, 10, 5),
                readlane(24, 10, 4),
            ],
        ),
        (
            "another save of the lane on one path to the restore",
            vec![
                writelane(10, 20, 4),
                writelane(10, 21, 5),
                vec![0xBF06_8001, 0xBF85_0002], // s_cmp_eq_u32 s1, 0 and s_cbranch_scc1 over the save
                writelane(10, 30, 4),
                readlane(22, 10, 4),
                readlane(23, 10, 5),
            ],
        ),
    ];
    for (name, words) in cases {
        let p = parked(&words);
        assert!(instructions(&p, "s_mov_b64").is_empty(), "{name}");
        assert!(!instructions(&p, "s_mov_b32").is_empty(), "{name}");
    }
}

#[test]
fn only_a_provably_empty_mask_needs_no_exchange_in_a_lane_branch() {
    // Threads below s0 read both dwords of a scalar mask and store 1 + (lo | hi).
    // XOR of EXEC with itself has zero bits in the full console wave, while
    // OR with itself retains the dynamic EXEC bits of both host waves.
    let lower_mask = |op, on| {
        let native = native(&[
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0007, // s_cbranch_execz to the restore of EXEC
            op,
            0x7E0A_020E, // v_mov_b32 v5, s14
            0x7E0C_020F, // v_mov_b32 v6, s15
            0x380A_0D05, // v_or_b32 v5, v5, v6
            0x4A0A_0A81, // v_add_nc_u32 v5, 1, v5
            0x38 << 26 | 0x1C << 18,
            0x80 << 24 | 2 << 16 | 5 << 8, // buffer_store_dword v5, off, s[8:11], 0
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        ]);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let previous = lower::TEST_ZERO_MASK.replace(Some(on));
        let result = lowered(&native, &p, &HashSet::new());
        lower::TEST_ZERO_MASK.set(previous);
        result
    };
    const XOR_EXEC: u32 = 0x898E_7E7E; // s_xor_b64 s[14:15], exec, exec
    let baseline = lower_mask(XOR_EXEC, false);
    assert!(matches!(
        baseline,
        Err(ShaderError::Unsupported { what, .. }) if what == "a lane mask exchanged where not every lane runs"
    ));
    let dynamic = lower_mask(0x888E_7E7E, true); // s_or_b64 s[14:15], exec, exec
    assert!(
        matches!(&dynamic, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask exchanged where not every lane runs"),
        "{:?}",
        dynamic.err()
    );
    let shader = lower_mask(XOR_EXEC, true).expect("empty mask");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for below in [1, 40] {
        if let Some(actual) = on_the_card(&shader, [below, 0]) {
            assert_eq!(actual, 1, "{below} active lanes");
        }
    }
}

/// The ray-tracing builder's scans refuse on `v_readfirstlane_b32` under a
/// per-lane branch: the cross-lane retry ([`super::widen`]) has to take the
/// branch around it wave-wide so the two host waves meet at the exchange
/// barrier, instead of skipping the other half's value.
#[test]
fn a_first_lane_refusal_is_retried_wave_wide() {
    use crate::native::HwStage;
    assert!(super::needs_wave_wide(HwStage::Cs, "a first-lane read where not every lane runs", 32));
    assert!(super::needs_wave_wide(HwStage::Cs, "a lane read where not every lane runs", 32));
    // (64-lane hosts run the console wave whole: nothing to retry there)
    assert!(!super::needs_wave_wide(HwStage::Cs, "a first-lane read where not every lane runs", 64));
    assert!(!super::needs_wave_wide(HwStage::Ps, "a first-lane read where not every lane runs", 32));
    assert!(!super::needs_wave_wide(HwStage::Cs, "a descriptor chosen by control flow", 32));
}

/// Lowers `body` as a console program of one 64-lane wave (or 32 when `wave64` is not set)
/// for a host of 32-lane waves, validates it and runs it there; each lane stores v3 to dword
/// v0 of the raw UAV. None where the host device does not run it.
fn permuted(body: &[u32], wave64: bool) -> Option<Vec<u32>> {
    let lanes = if wave64 { 64 } else { 32 };
    let mut words = body.to_vec();
    words.extend_from_slice(&[
        0x340C_0082, // v_lshlrev_b32 v6, 2, v0
        0xE070_1000,
        0x8002_0306, // buffer_store_dword v3, v6, s[8:11], 0 offen
    ]);
    let native = native_of(&words, wave64, lanes);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // The same program for a host of 64-lane waves lowers too.
    let wide = lowered_for_width(&native, &p, &HashSet::new(), 64).expect("lower for 64-lane waves");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = wide.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed (64-lane waves)");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(vec![0; shader.user_data_dwords().max(1) as usize]))],
                views: &[View::RawUav(lanes)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..lanes as usize].to_vec())
}

/// A wave64 `ds_bpermute_b32` permutes lanes 0..31 and 32..63 apart (RDNA 2 ISA, section
/// 10.4.4): v1 = ((lane & 31) << 2) + 0x103 with offset 12 drops the high and sub-dword
/// address bits and wraps inside each half (lanes 29..31 read 0..2, lanes 61..63 read 32..34,
/// lane 32 reads lane 35, not lane 3). Run in host waves of 32, the program needs no exchange
/// between them.
#[test]
fn a_wave64_backward_permute_stays_within_each_32_lane_half() {
    let body = [
        0x4A04_00FF,
        0x0000_4000, // v_add_nc_u32 v2, 0x4000, v0
        0x3602_009F, // v_and_b32 v1, 31, v0
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
        0x4A02_02FF,
        0x0000_0103, // v_add_nc_u32 v1, 0x103, v1
        0xDACC_000C,
        0x0300_0201, // ds_bpermute_b32 v3, v1, v2 offset:12
    ];
    let Some(got) = permuted(&body, true) else { return };
    let want: Vec<u32> = (0..64).map(|l| 0x4000 + ((l & !31) | ((l + 3) & 31))).collect();
    assert_eq!(got, want);
    eprintln!("ran on the hardware device: each half permuted on its own");
}

/// EXEC is per half too: with lanes 0..39 active, lanes 32..39 read upper-half lanes 40..47,
/// which are inactive, and get 0 although lower-half lanes 8..15 are active. Lanes 40..63
/// keep their v3.
#[test]
fn a_wave64_backward_permute_reads_exec_within_each_half() {
    let body = [
        0x4A04_00FF,
        0x0000_5000, // v_add_nc_u32 v2, 0x5000, v0
        0x4A06_00FF,
        0xCAFE_0000, // v_add_nc_u32 v3, 0xcafe0000, v0
        0x4A02_0088, // v_add_nc_u32 v1, 8, v0
        0x3602_029F, // v_and_b32 v1, 31, v1
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
        0xDACC_0000,
        0x0300_0201, // ds_bpermute_b32 v3, v1, v2
        0xBEFE_0414, // s_mov_b64 exec, s[20:21]
    ];
    let Some(got) = permuted(&body, true) else { return };
    let want: Vec<u32> = (0..64)
        .map(|l| match l {
            0..32 => 0x5000 + ((l + 8) & 31),
            32..40 => 0,
            _ => 0xCAFE_0000 + l,
        })
        .collect();
    assert_eq!(got, want);
    eprintln!("ran on the hardware device: inactive lanes of the same half read 0");
}

/// A wave32 backward permute names lanes by bits 6:2 of the address: the high bits (0x100)
/// and sub-dword bits (+3) are dropped.
#[test]
fn a_wave32_backward_permute_drops_the_high_and_sub_dword_address_bits() {
    let body = [
        0x4A04_00FF,
        0x0000_3000, // v_add_nc_u32 v2, 0x3000, v0
        0x4A02_0088, // v_add_nc_u32 v1, 8, v0
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
        0x4A02_02FF,
        0x0000_0103, // v_add_nc_u32 v1, 0x103, v1
        0xDACC_0000,
        0x0300_0201, // ds_bpermute_b32 v3, v1, v2
    ];
    let Some(got) = permuted(&body, false) else { return };
    let want: Vec<u32> = (0..32).map(|l| 0x3000 + ((l + 8) & 31)).collect();
    assert_eq!(got, want);
}

/// A console wave32 program with console lanes (`v_mbcnt`) in a 32 x 2 group: thread (x, y)
/// stores 1 at dword 32y + x. Every dword must be written once: the host's relabelled thread
/// ids are a bijection of the group. A 32-lane host need not give a host wave a 32-aligned run
/// of flat ids in a 2-D group (NVIDIA fills them from tiles), so the first flat id of a wave
/// rounded down gave two waves the same ids (TaynanGT's tree, #1243); the waves now take their
/// threads from a group counter (`VICTORY_THREAD_TICKET`). The old rounding's count of dwords
/// written is printed, not checked: it depends on the card. (After TaynanGT's tree via PiloteBox d193085a.)
#[test]
fn the_console_threads_of_a_two_dimensional_group_are_each_given_once() {
    let words = [
        0xD765_0003, // v_mbcnt_lo_u32_b32 v3, exec_lo, 0
        0x0001_007E,
        0x3408_0285, // v_lshlrev_b32 v4, 5, v1
        0x4A08_0104, // v_add_nc_u32 v4, v4, v0
        0x3408_0882, // v_lshlrev_b32 v4, 2, v4
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 5 << 8 | 4, // buffer_store_dword v5, v4, s[8:11], 0 offen
    ];
    let mut native = native_of(&words, false, 64);
    native.io = StageIo::Cs(ComputeIo {
        rsrc2: 5 << 1 | 1 << 11, // 5 user SGPRs, thread ids x and y in v0, v1
        threads: [32, 2, 1],
    });
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let lower_with = |ticket: bool| {
        let was = lower::TEST_THREAD_TICKET.replace(Some(ticket));
        let shader = lowered_for_width(&native, &p, &HashSet::new(), 32);
        lower::TEST_THREAD_TICKET.set(was);
        shader.expect("lower")
    };
    let shader = lower_with(true);
    let old = lower_with(false);
    assert_ne!(shader.dxil, old.dxil, "the counter is used for a group of two host waves");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let run = |shader: &HostShader| -> Option<Vec<u32>> {
        let (root, pso) = match host.compute_pipeline(shader) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("the hardware device does not take this program: {e}; not run");
                return None;
            }
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(64)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        Some(out[0][..64].to_vec())
    };
    let Some(got) = run(&shader) else { return };
    let missing: Vec<usize> = (0..64).filter(|&i| got[i] != 1).collect();
    assert!(missing.is_empty(), "dwords not written once: {missing:?}");
    if let Some(before) = run(&old) {
        let written = before.iter().filter(|&&d| d == 1).count();
        eprintln!("the old rounding on this device: {written} of 64 dwords written");
    }
    eprintln!("ran on the hardware device: 64 of 64 console threads, each once");
}

/// The thread ticket's counter in the program's own LDS when the program takes all 32 KiB, and
/// a group of one dimension (whose host waves hold runs of 32 flat ids) without it.
#[test]
fn the_thread_ticket_fits_a_full_lds_and_leaves_one_dimensional_groups() {
    let words = [
        0xD765_0003, // v_mbcnt_lo_u32_b32 v3, exec_lo, 0
        0x0001_007E,
        0x3408_0285, // v_lshlrev_b32 v4, 5, v1
        0x4A08_0104, // v_add_nc_u32 v4, v4, v0
        0x3408_0882, // v_lshlrev_b32 v4, 2, v4
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 5 << 8 | 4, // buffer_store_dword v5, v4, s[8:11], 0 offen
    ];
    let shaped = |threads: [u32; 3], lds_blocks: u32| {
        let mut native = native_of(&words, false, 64);
        // LDS_SIZE (rsrc2 bits 15..23) in blocks of 512 bytes
        native.io = StageIo::Cs(ComputeIo { rsrc2: 5 << 1 | 1 << 11 | lds_blocks << 15, threads });
        native
    };
    let lower_with = |native: &NativeProgram, ticket: bool| {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let was = lower::TEST_THREAD_TICKET.replace(Some(ticket));
        let shader = lowered_for_width(native, &p, &HashSet::new(), 32);
        lower::TEST_THREAD_TICKET.set(was);
        shader.expect("lower")
    };
    let full = shaped([32, 2, 1], 64);
    let (on, off) = (lower_with(&full, true), lower_with(&full, false));
    assert_ne!(on.dxil, off.dxil, "the counter in a full LDS");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = on.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed (a full LDS)");
    }
    let row = shaped([64, 1, 1], 0);
    assert_eq!(lower_with(&row, true).dxil, lower_with(&row, false).dxil, "a row keeps the rounding");
}

/// A console wave64 program with console lanes (`v_mbcnt`) in a 6 x 6 group: two host waves
/// of 32 and 4 lanes. Thread (x, y) stores 1 at dword 6y + x; every one of the 36 dwords must
/// be written once and none past them. With a fixed 32 a wave, the 4-lane wave took base 0
/// when it came first and the full wave threads 32..63, past the group; the waves now count
/// their lanes (`VICTORY_THREAD_TICKET`). A group of one host wave (4 x 4) is as before.
#[test]
fn a_partial_host_wave_of_a_console_wave_takes_the_threads_after_the_full_ones() {
    let words = [
        0xD765_0003, // v_mbcnt_lo_u32_b32 v3, exec_lo, 0
        0x0001_007E,
        0x1608_0286, // v_mul_u32_u24 v4, 6, v1
        0x4A08_0104, // v_add_nc_u32 v4, v4, v0
        0x3408_0882, // v_lshlrev_b32 v4, 2, v4
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 5 << 8 | 4, // buffer_store_dword v5, v4, s[8:11], 0 offen
    ];
    let shaped = |threads: [u32; 3]| {
        let mut native = native_of(&words, true, 64);
        native.io = StageIo::Cs(ComputeIo { rsrc2: 5 << 1 | 1 << 11, threads });
        native
    };
    let lower_with = |native: &NativeProgram, ticket: bool| {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let was = lower::TEST_THREAD_TICKET.replace(Some(ticket));
        let shader = lowered_for_width(native, &p, &HashSet::new(), 32);
        lower::TEST_THREAD_TICKET.set(was);
        shader.expect("lower")
    };
    let small = shaped([4, 4, 1]);
    assert_eq!(lower_with(&small, true).dxil, lower_with(&small, false).dxil, "one host wave");
    let native = shaped([6, 6, 1]);
    let shader = lower_with(&native, true);
    assert_ne!(shader.dxil, lower_with(&native, false).dxil, "two host waves, the last partial");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program: {e}; not run");
            return;
        }
    };
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(64)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    let got = &out[0][..64];
    let wrong: Vec<usize> = (0..64).filter(|&i| got[i] != u32::from(i < 36)).collect();
    assert!(wrong.is_empty(), "dwords not as 36 threads write them: {wrong:?}");
    eprintln!("ran on the hardware device: 36 of 36 console threads, each once");
}
