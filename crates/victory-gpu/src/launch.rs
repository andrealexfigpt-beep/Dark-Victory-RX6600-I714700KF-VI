//! Ported from Kelvaroq e062a82, 0041-mesh-launch-wide-hash (GPL-3.0-or-later).

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::HostList;

/// The amplification shader's code (see `launch.hlsl`).
const CODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/launch.dxil"));

/// The root constant that has the card read the record.
pub(crate) const DIRECT_NONE: u32 = u32::MAX;

/// Where the amplification shader binds: `b0`, `t0`, `u0` in this space.
const SPACE: u32 = victory_shader::emit::LAUNCH_SPACE;

/// Whether mesh pipelines get the amplification stage and indirect mesh
/// records are launched by it (`VICTORY_MESH_LAUNCH=0`: not).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let requested = std::env::var_os("VICTORY_MESH_LAUNCH").is_some_and(|v| v == "1");
        if requested && CODE.is_empty() {
            tracing::warn!("mesh launcher unavailable: rebuild with the Windows SDK's DXC");
        }
        requested && !CODE.is_empty()
    })
}

/// The amplification stage's code.
pub(crate) fn code() -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE { pShaderBytecode: CODE.as_ptr().cast(), BytecodeLength: CODE.len() }
}

/// The bytes of [`code`], for the pipeline library's names.
pub(crate) fn bytes() -> &'static [u8] {
    CODE
}

/// The three root parameters the stage reads, in order: the direct group
/// count (one constant), the record (a root SRV), the list's budget (a
/// root UAV). Five root words.
pub(crate) fn params() -> [D3D12_ROOT_PARAMETER1; 3] {
    let descriptor = |kind| D3D12_ROOT_PARAMETER1 {
        ParameterType: kind,
        Anonymous: D3D12_ROOT_PARAMETER1_0 {
            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                ShaderRegister: 0,
                RegisterSpace: SPACE,
                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_AMPLIFICATION,
    };
    [
        D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: SPACE, Num32BitValues: 1 },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_AMPLIFICATION,
        },
        descriptor(D3D12_ROOT_PARAMETER_TYPE_SRV),
        descriptor(D3D12_ROOT_PARAMETER_TYPE_UAV),
    ]
}

/// Records one indirect record through the amplification stage.
pub(crate) fn record(
    list: &HostList,
    roots: [u32; 3],
    pipeline: &ID3D12PipelineState,
    plain: &ID3D12PipelineState,
    address: u64,
    budget: u64,
) -> bool {
    use windows::core::Interface;
    let Ok(mesh) = list.list.cast::<ID3D12GraphicsCommandList6>() else {
        return false;
    };
    // SAFETY: the command list is open, and its root signature is compatible
    // with both pipelines. The caller keeps the argument buffer in the list.
    unsafe {
        list.list.SetPipelineState(pipeline);
        list.list.SetGraphicsRoot32BitConstant(roots[0], DIRECT_NONE, 0);
        list.list.SetGraphicsRootShaderResourceView(roots[1], address);
        list.list.SetGraphicsRootUnorderedAccessView(roots[2], budget);
        mesh.DispatchMesh(1, 1, 1);
        list.list.SetPipelineState(plain);
    }
    LAUNCHED.fetch_add(1, Ordering::Relaxed);
    true
}

static LAUNCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn line(frames: f64) {
    let launched = LAUNCHED.swap(0, Ordering::Relaxed);
    if launched != 0 {
        tracing::info!(per_frame = launched as f64 / frames.max(1.0), "indirect mesh records launched by amplification");
    }
}
