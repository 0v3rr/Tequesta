//! `XboxBvhBuilder.dll`: console ray-tracing acceleration structure builder.
//!
//! When `VICTORY_BVH_BUILDER != "1"` (the default), translated shaders lower
//! `image_bvh*_intersect_ray` to immediate miss (`VICTORY_BVH_NO_HIT=1`) and
//! builder compute dispatches are dropped. Serving `XboxBvhBuilder.dll` here
//! avoids mapping the console DLL, compiling its ~35 compute PSOs, allocating
//! multi-GB scratch/result BVH buffers, and churning per-list upload memory,
//! while still recording valid post-build info sizes onto the command list.
//!
//! Set `VICTORY_BVH_BUILDER=1` or `VICTORY_BVH_HLE=0` to fall back to the
//! title's own `XboxBvhBuilder.dll`.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use victory_gpu::object::Object;

use crate::{HleExport, HleModule};

const MIN_AS_BYTES: u64 = 256;
const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;

static MORTON_64BIT: AtomicBool = AtomicBool::new(false);

#[repr(C)]
#[derive(Clone, Copy)]
struct BuildInputs {
    kind: u32,
    flags: u32,
    num_descs: u32,
    descs_layout: u32,
    data: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BuildDesc {
    dest_as: u64,
    inputs: BuildInputs,
    src_as: u64,
    scratch_as: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PostbuildInfoDesc {
    dest_buffer: u64,
    info_type: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BatchedDesc2 {
    desc: BuildDesc,
    num_postbuild: u32,
    _pad: u32,
    postbuild: *const PostbuildInfoDesc,
    stream_desc: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PrebuildInfo {
    result_data_max_size_in_bytes: u64,
    scratch_data_size_in_bytes: u64,
    update_scratch_data_size_in_bytes: u64,
}

const _: () = assert!(size_of::<BuildInputs>() == 0x18);
const _: () = assert!(size_of::<BuildDesc>() == 0x30);
const _: () = assert!(size_of::<PostbuildInfoDesc>() == 0x10);
const _: () = assert!(size_of::<BatchedDesc2>() == 0x50);
const _: () = assert!(size_of::<PrebuildInfo>() == 0x18);

/// Whether our native HLE serves `XboxBvhBuilder.dll` instead of the title's DLL.
pub fn serving() -> bool {
    let builder_on = std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1");
    let hle_off = std::env::var_os("VICTORY_BVH_HLE").is_some_and(|v| v == "0");
    !builder_on && !hle_off
}

fn emit_postbuild_infos(list: *const c_void, count: u32, descs: *const PostbuildInfoDesc) {
    if list.is_null() || count == 0 || descs.is_null() {
        return;
    }
    let list = list.cast::<Object>();
    for i in 0..count as usize {
        // SAFETY: title passes `count` valid `PostbuildInfoDesc` entries.
        let d = unsafe { *descs.add(i) };
        if d.dest_buffer == 0 {
            continue;
        }
        match d.info_type {
            // COMPACTED_SIZE (0), TOOLS_VISUALIZATION (1), CURRENT_SIZE (3)
            0 | 1 | 3 => {
                victory_gpu::list::write_u64_deferred(list, d.dest_buffer, MIN_AS_BYTES);
            }
            // SERIALIZATION (2): SerializedSizeInBytes + NumBottomLevelAccelerationStructurePointers
            2 => {
                victory_gpu::list::write_u64_deferred(list, d.dest_buffer, MIN_AS_BYTES);
                victory_gpu::list::write_u64_deferred(list, d.dest_buffer + 8, 0);
            }
            // 0x10 (Xbox internal) and unknown types are skipped.
            _ => {}
        }
    }
}

extern "system" fn ctor(this: *mut c_void, _device: *mut c_void, _flag: u8) -> *mut c_void {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::info!("XboxBvhBuilder HLE active (VICTORY_BVH_BUILDER != 1)");
    });
    if !this.is_null() {
        // SAFETY: title allocates the `XboxBvhBuilder` object (at least 3 pointers).
        unsafe { std::ptr::write_bytes(this.cast::<usize>(), 0, 3) };
    }
    this
}

extern "system" fn dtor(this: *mut c_void) {
    if !this.is_null() {
        // SAFETY: zero the 3 handle/state pointers on destruction.
        unsafe { std::ptr::write_bytes(this.cast::<usize>(), 0, 3) };
    }
}

extern "system" fn build_as(
    _this: *mut c_void,
    list: *const c_void,
    _desc: *const BuildDesc,
    num_postbuild: u32,
    postbuild: *const PostbuildInfoDesc,
) {
    emit_postbuild_infos(list, num_postbuild, postbuild);
}

extern "system" fn build_as_batched2x(
    _this: *mut c_void,
    list: *const c_void,
    num_descs: u32,
    descs: *const BatchedDesc2,
) {
    if num_descs == 0 || descs.is_null() {
        return;
    }
    for i in 0..num_descs as usize {
        // SAFETY: title passes `num_descs` valid `BatchedDesc2` entries.
        let b = unsafe { &*descs.add(i) };
        emit_postbuild_infos(list, b.num_postbuild, b.postbuild);
    }
}

extern "system" fn copy_as(
    _this: *mut c_void,
    _list: *const c_void,
    _dest: u64,
    _src: u64,
    _mode: u32,
) {
}

extern "system" fn cpu_validate_result_x(
    _this: *mut c_void,
    _list: *const c_void,
    _desc: *const BuildDesc,
) {
}

extern "system" fn get_prebuild_info(
    _this: *mut c_void,
    inputs: *const BuildInputs,
    _stream_desc: *const c_void,
    out: *mut PrebuildInfo,
) {
    if out.is_null() {
        return;
    }
    // SAFETY: `inputs` is valid when non-null.
    let allow_update = !inputs.is_null() && unsafe { ((*inputs).flags & 1) != 0 };
    // SAFETY: `out` is a valid caller-supplied `PrebuildInfo` pointer.
    unsafe {
        *out = PrebuildInfo {
            result_data_max_size_in_bytes: MIN_AS_BYTES,
            scratch_data_size_in_bytes: MIN_AS_BYTES,
            update_scratch_data_size_in_bytes: if allow_update { MIN_AS_BYTES } else { 0 },
        };
    }
}

extern "system" fn rehydrate_batched2x(
    _this: *mut c_void,
    _list: *const c_void,
    num_descs: u32,
    descs: *const BuildDesc,
    _src_ptrs: *const u64,
) -> i32 {
    if num_descs > 0 && descs.is_null() {
        E_INVALIDARG
    } else {
        S_OK
    }
}

extern "system" fn rehydrate_batched_x(
    _this: *mut c_void,
    _list: *const c_void,
    num_descs: u32,
    descs: *const BuildDesc,
) -> i32 {
    if num_descs > 0 && descs.is_null() {
        E_INVALIDARG
    } else {
        S_OK
    }
}

extern "system" fn rehydrate_x(
    _this: *mut c_void,
    _device: *mut c_void,
    desc: *const BuildDesc,
) -> i32 {
    if desc.is_null() {
        E_INVALIDARG
    } else {
        S_OK
    }
}

extern "system" fn set_64bit_morton_codes(_this: *mut c_void, enabled: u8) {
    MORTON_64BIT.store(enabled != 0, Ordering::Relaxed);
}

extern "system" fn validate_rehydrate_build_desc(
    _this: *mut c_void,
    desc: *const BuildDesc,
) -> i32 {
    if desc.is_null() {
        E_INVALIDARG
    } else {
        S_OK
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xboxbvhbuilder",
        dlls: &["xboxbvhbuilder.dll"],
        exports: vec![
            HleExport {
                name: "??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z",
                address: ctor as *const () as usize,
            },
            HleExport {
                name: "??1XboxBvhBuilder@@QEAA@XZ",
                address: dtor as *const () as usize,
            },
            HleExport {
                name: "?BuildRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@IPEBUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC@@@Z",
                address: build_as as *const () as usize,
            },
            HleExport {
                name: "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z",
                address: build_as_batched2x as *const () as usize,
            },
            HleExport {
                name: "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@_K1W4D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE@@@Z",
                address: copy_as as *const () as usize,
            },
            HleExport {
                name: "?CpuValidateResultX@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z",
                address: cpu_validate_result_x as *const () as usize,
            },
            HleExport {
                name: "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z",
                address: get_prebuild_info as *const () as usize,
            },
            HleExport {
                name: "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@PEB_K@Z",
                address: rehydrate_batched2x as *const () as usize,
            },
            HleExport {
                name: "?RehydrateRaytracingAccelerationStructureBatchedX@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z",
                address: rehydrate_batched_x as *const () as usize,
            },
            HleExport {
                name: "?RehydrateRaytracingAccelerationStructureX@XboxBvhBuilder@@QEAAJPEAUID3D12Device11@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z",
                address: rehydrate_x as *const () as usize,
            },
            HleExport {
                name: "?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z",
                address: set_64bit_morton_codes as *const () as usize,
            },
            HleExport {
                name: "?ValidateRehydrateAccelerationStructureBuildDesc@XboxBvhBuilder@@QEAAJPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z",
                address: validate_rehydrate_build_desc as *const () as usize,
            },
        ],
    }
}
