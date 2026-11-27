//! Startup dispatch for the Full HD FSR 4 provider, before it sees title frames.

use super::{
    Api, CreateUpscale, DISPATCH_UPSCALE, DispatchUpscale, FFX_COMPUTE_READ, FFX_UNORDERED_ACCESS,
    OK, Resource,
};
use crate::host::{Host, host, ram_headroom};
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

static RESET: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

static PENDING: AtomicBool = AtomicBool::new(false);

/// Transfer the reset to the first real dispatch, and forget destroyed contexts.
pub(super) fn take_reset(context: *mut *mut c_void) -> bool {
    if !PENDING.load(Ordering::Acquire) {
        return false;
    }
    // SAFETY: the caller checked the SDK handle slot.
    let handle = unsafe { *context } as usize;
    let mut reset = RESET.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pending) = reset.as_mut() else {
        return false;
    };
    let warmed = pending.remove(&handle);
    if pending.is_empty() {
        PENDING.store(false, Ordering::Release);
    }
    warmed
}

pub(super) fn run(api: &Api, context: *mut *mut c_void, desc: CreateUpscale, provider: &str) {
    if !provider.starts_with("4.") || std::env::var_os("VICTORY_FFX_WARM").is_some_and(|v| v == "0")
    {
        return;
    }
    // The synthetic frame is sized and configured for our Series S profile.
    const PROFILE_FLAGS: u32 = (1 << 0) | (1 << 3) | (1 << 5);
    if desc.flags != PROFILE_FLAGS
        || desc.max_render != [1280, 720]
        || desc.max_upscale != [1920, 1080]
        || ram_headroom().is_none_or(|capacity| capacity < 2 << 30)
    {
        tracing::info!("ffx: startup warm-up skipped for context settings or memory headroom");
        return;
    }
    let Some(host) = host() else { return };
    let began = std::time::Instant::now();
    let made = (|| {
        Some((
            texture(host, desc.max_render, DXGI_FORMAT_R16G16B16A16_FLOAT, false)?,
            texture(host, desc.max_render, DXGI_FORMAT_R32_FLOAT, false)?,
            texture(host, desc.max_render, DXGI_FORMAT_R16G16_FLOAT, false)?,
            texture(host, desc.max_upscale, DXGI_FORMAT_R16G16B16A16_FLOAT, true)?,
            host.new_list().ok()?,
        ))
    })();
    let Some((color, depth, motion, output, list)) = made else {
        tracing::warn!("ffx: could not prepare startup warm-up resources");
        return;
    };
    const EMPTY: Resource = Resource {
        resource: std::ptr::null_mut(),
        description: [0; 8],
        state: 0,
    };
    let d = DispatchUpscale {
        kind: DISPATCH_UPSCALE,
        next: std::ptr::null_mut(),
        command_list: list.list.as_raw(),
        resources: [
            sdk_texture(&color, desc.max_render, 4, false),
            sdk_texture(&depth, desc.max_render, 28, false),
            sdk_texture(&motion, desc.max_render, 18, false),
            EMPTY,
            EMPTY,
            EMPTY,
            sdk_texture(&output, desc.max_upscale, 4, true),
        ],
        jitter: [0.0; 2],
        motion_vector_scale: desc.max_render.map(|n| n as f32),
        render_size: desc.max_render,
        upscale_size: desc.max_upscale,
        sharpening: false,
        sharpness: 0.0,
        frame_time_delta: 16.667,
        pre_exposure: 1.0,
        reset: true,
        camera: [0.1, 1000.0, 1.0, 1.0],
        flags: 0,
    };
    // SAFETY: a fresh context and an open DX12 list. New, zero-initialized
    // textures stay alive through execution. No title resources are involved.
    let dispatch = std::time::Instant::now();
    let result = unsafe { (api.dispatch)(context, (&raw const d).cast()) };
    let cpu_us = dispatch.elapsed().as_micros() as u64;
    // SAFETY: successful creation filled the SDK handle slot.
    let handle = unsafe { *context } as usize;
    {
        let mut reset = RESET.lock().unwrap_or_else(|e| e.into_inner());
        reset.get_or_insert_with(HashSet::new).insert(handle);
        PENDING.store(true, Ordering::Release);
    }
    if result == OK {
        list.close();
        host.execute(&[&list]);
    }
    tracing::info!(
        result,
        cpu_us,
        wall_us = began.elapsed().as_micros() as u64,
        "ffx: startup warm-up"
    );
}

fn texture(
    host: &Host,
    size: [u32; 2],
    format: DXGI_FORMAT,
    output: bool,
) -> Option<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: size[0].into(),
        Height: size[1],
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: if output {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAG_NONE
        },
        ..Default::default()
    };
    let state = if output {
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS
    } else {
        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut texture = None;
    // SAFETY: valid single-mip resources with matching initial states.
    unsafe {
        host.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut texture,
        )
    }
    .ok()?;
    texture
}

fn sdk_texture(
    texture: &ID3D12Resource,
    [width, height]: [u32; 2],
    format: u32,
    output: bool,
) -> Resource {
    const TEXTURE_2D: u32 = 2;
    const USAGE_UAV: u32 = 1 << 1;
    Resource {
        resource: texture.as_raw(),
        description: [
            TEXTURE_2D,
            format,
            width,
            height,
            1,
            1,
            0,
            if output { USAGE_UAV } else { 0 },
        ],
        state: if output {
            FFX_UNORDERED_ACCESS
        } else {
            FFX_COMPUTE_READ
        },
    }
}
