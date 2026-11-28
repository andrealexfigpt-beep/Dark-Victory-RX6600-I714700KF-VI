//! Native Windows XAudio2 exports. The system DLL owns every returned COM reference.
use crate::{HleModule, hle_export};
use std::ffi::c_void;

#[link(name = "xaudio2_9", kind = "raw-dylib")]
unsafe extern "system" {
    fn XAudio2Create(out: *mut *mut c_void, flags: u32, processor: u32) -> i32;
    fn XAudio2CreateWithVersionInfo(out: *mut *mut c_void, flags: u32, processor: u32, version: u32) -> i32;
    fn X3DAudioInitialize(mask: u32, speed: f32, handle: *mut u8) -> i32;
    fn X3DAudioCalculate(handle: *const u8, listener: *const c_void, emitter: *const c_void, flags: u32, settings: *mut c_void);
    fn CreateFX(clsid: *const c_void, effect: *mut *mut c_void, data: *const c_void, size: u32) -> i32;
}

/// XAudio2 module registration.
pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xaudio2",
        dlls: &["xaudio2.dll", "xaudio2_9.dll", "xaudio2_8.dll", "xaudio2_7.dll"],
        exports: vec![
            hle_export!(XAudio2Create),
            hle_export!(XAudio2CreateWithVersionInfo),
            hle_export!(X3DAudioInitialize),
            hle_export!(X3DAudioCalculate),
            crate::HleExport { name: "XMAudioInitialize", address: X3DAudioInitialize as *const () as usize },
            crate::HleExport { name: "XMAudioCalculate", address: X3DAudioCalculate as *const () as usize },
            hle_export!(CreateFX),
        ],
    }
}
