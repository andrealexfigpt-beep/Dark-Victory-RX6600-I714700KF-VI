//! `xaudio2.dll` and `xaudio2_9.dll`: native Windows XAudio2.
//!
//! The console's XAudio2 is the same COM API as the host's. We provide
//! thin wrappers that forward to the host's XAudio2, while handling
//! the console-specific differences (XMA decoding, APU heap).
//!
//! The title's XMA audio goes through `audio::XMA` decoder (APU/SHAPE);
//! only PC-style XAudio2 usage (WAV, ADPCM, etc.) reaches here.

#[cfg(windows)]
mod xaudio2_impl;

#[cfg(windows)]
use xaudio2_impl::{XAudio2Engine, XAudio2Voice, XAudio2SourceVoice, XAudio2SubmixVoice, XAudio2MasteringVoice};

#[cfg(windows)]
use windows::Win32::Media::XAudio2::*;
#[cfg(windows)]
use windows::core::{Interface, GUID};

use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};
use std::collections::HashMap;

use crate::{HleModule, hle_export};

/// Global XAudio2 engine instance.
static XAUDIO2_ENGINE: OnceLock<Arc<XAudio2Engine>> = OnceLock::new();

/// Get or create the XAudio2 engine.
fn xaudio2_engine() -> Arc<XAudio2Engine> {
    XAUDIO2_ENGINE.get_or_init(|| {
        Arc::new(XAudio2Engine::new().expect("Failed to create XAudio2 engine"))
    }).clone()
}

/// XAudio2Create: ordinal 1, the factory function.
pub extern "system" fn XAudio2Create(
    out: *mut *mut c_void,
    flags: u32,
    processor: u32,
) -> i32 {
    if out.is_null() {
        return -2147024809; // E_INVALIDARG
    }

    let engine = xaudio2_engine();
    let iface = engine.create_interface();
    
    unsafe { out.write(iface) };
    0 // S_OK
}

/// XAudio2CreateWithVersionInfo: extended factory.
pub extern "system" fn XAudio2CreateWithVersionInfo(
    out: *mut *mut c_void,
    flags: u32,
    processor: u32,
    version: *const XAUDIO2_VERSION_INFO,
) -> i32 {
    if out.is_null() {
        return -2147024809; // E_INVALIDARG
    }

    let engine = xaudio2_engine();
    let iface = engine.create_interface();
    
    unsafe { out.write(iface) };
    0 // S_OK
}

/// X3DAudioInitialize: 3D audio support.
pub extern "system" fn X3DAudioInitialize(
    speaker_channel_mask: u32,
    speed_of_sound: f32,
    handle: *mut c_void,
) -> i32 {
    if handle.is_null() {
        return -2147024809; // E_INVALIDARG
    }
    
    // Initialize X3DAudio handle
    unsafe {
        // The handle is an X3DAUDIO_HANDLE (opaque struct)
        // We just zero it for now - real implementation would initialize
        std::ptr::write_bytes(handle, 0, std::mem::size_of::<usize>());
    }
    0 // S_OK
}

/// X3DAudioCalculate: calculate 3D audio DSP settings.
pub extern "system" fn X3DAudioCalculate(
    handle: *const c_void,
    emitter: *const X3DAUDIO_EMITTER,
    listener: *const X3DAUDIO_LISTENER,
    flags: u32,
    dsp_settings: *mut X3DAUDIO_DSP_SETTINGS,
) -> i32 {
    // Forward to host's X3DAudioCalculate
    // For now, return S_OK with zeroed settings
    if dsp_settings.is_null() {
        return -2147024809;
    }
    unsafe { std::ptr::write_bytes(dsp_settings, 0, std::mem::size_of::<X3DAUDIO_DSP_SETTINGS>()) };
    0
}

/// XMAudioInitialize: Xbox-specific audio.
pub extern "system" fn XMAudioInitialize(
    speaker_channel_mask: u32,
    speed_of_sound: f32,
    handle: *mut c_void,
) -> i32 {
    X3DAudioInitialize(speaker_channel_mask, speed_of_sound, handle)
}

/// XMAudioCalculate: Xbox-specific 3D audio.
pub extern "system" fn XMAudioCalculate(
    handle: *const c_void,
    emitter: *const X3DAUDIO_EMITTER,
    listener: *const X3DAUDIO_LISTENER,
    flags: u32,
    dsp_settings: *mut X3DAUDIO_DSP_SETTINGS,
) -> i32 {
    X3DAudioCalculate(handle, emitter, listener, flags, dsp_settings)
}

/// CreateFX: create audio effects (reverb, etc.).
pub extern "system" fn CreateFX(
    effect_id: *const GUID,
    effect_flags: u32,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return -2147024809;
    }
    
    // For now, return not implemented for effects
    // Real implementation would create the effect
    unsafe { out.write(std::ptr::null_mut()) };
    -2147467263 // E_NOTIMPL
}

/// XAudio2 version info structure.
#[repr(C)]
pub struct XAUDIO2_VERSION_INFO {
    pub major_version: u32,
    pub minor_version: u32,
    pub build_version: u32,
    pub revision_version: u32,
    pub debug_version: u32,
}

/// X3DAudio structures (opaque on host, but titles expect them).
#[repr(C)]
pub struct X3DAUDIO_HANDLE(pub usize);

#[repr(C)]
pub struct X3DAUDIO_EMITTER {
    pub channel_count: u32,
    pub channel_radius: f32,
    pub channel_azimuths: *const f32,
    pub curve_distance_scaler: f32,
    pub curve_doppler_scaler: f32,
    pub curve_lpf_reverb_scaler: f32,
    pub curve_lpf_direct_scaler: f32,
    pub curve_reverb_scaler: f32,
    pub channel_radius_min: f32,
    pub channel_radius_max: f32,
    pub position: X3DAUDIO_VECTOR,
    pub velocity: X3DAUDIO_VECTOR,
    pub orient_front: X3DAUDIO_VECTOR,
    pub orient_top: X3DAUDIO_VECTOR,
}

#[repr(C)]
pub struct X3DAUDIO_LISTENER {
    pub position: X3DAUDIO_VECTOR,
    pub velocity: X3DAUDIO_VECTOR,
    pub orient_front: X3DAUDIO_VECTOR,
    pub orient_top: X3DAUDIO_VECTOR,
}

#[repr(C)]
pub struct X3DAUDIO_VECTOR {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
pub struct X3DAUDIO_DSP_SETTINGS {
    pub src_channel_count: u32,
    pub dst_channel_count: u32,
    pub matrix_coefficients: *mut f32,
    pub delay_times: *mut f32,
    pub lpf_direct_coefficient: f32,
    pub lpf_reverb_coefficient: f32,
    pub reverb_level: f32,
    pub dopler_factor: f32,
    pub emitter_azimuth: f32,
    pub emitter_angle: f32,
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
            hle_export!(XMAudioInitialize),
            hle_export!(XMAudioCalculate),
            hle_export!(CreateFX),
        ],
    }
}

#[cfg(windows)]
mod xaudio2_impl {
    use std::sync::{Arc, Mutex, OnceLock};
    use std::ffi::c_void;

    use windows::Win32::Media::XAudio2::*;
    use windows::core::{Interface, IUnknown, GUID, ComInterface};
    use windows::Win32::Foundation::HRESULT;

    /// Wrapper around the host's IXAudio2.
    pub struct XAudio2Engine {
        inner: IXAudio2,
        voices: Mutex<HashMap<usize, Arc<dyn XAudio2Voice>>>,
    }

    use std::collections::HashMap;

    impl XAudio2Engine {
        pub fn new() -> Result<Self, windows::core::Error> {
            // Create the host XAudio2 engine
            let inner: IXAudio2 = unsafe { XAudio2Create(0, 0)? };
            Ok(Self {
                inner,
                voices: Mutex::new(HashMap::new()),
            })
        }

        pub fn create_interface(&self) -> *mut c_void {
            // Return the IXAudio2 interface pointer
            let ptr = std::ptr::from_ref(&self.inner).cast_mut().cast();
            // AddRef for the caller
            unsafe { self.inner.AddRef() };
            ptr
        }

        pub fn create_source_voice(
            &self,
            format: &WAVEFORMATEX,
            flags: u32,
            max_frequency_ratio: f32,
            callback: *mut IXAudio2VoiceCallback,
        ) -> Result<Arc<dyn XAudio2Voice>, windows::core::Error> {
            let mut voice: Option<IXAudio2SourceVoice> = None;
            unsafe {
                self.inner.CreateSourceVoice(&mut voice, format, flags, max_frequency_ratio, callback, None, None)?;
            }
            let voice = voice.ok_or(windows::core::Error::from_win32())?;
            
            let wrapper = Arc::new(XAudio2SourceVoiceImpl { inner: voice });
            
            let mut voices = self.voices.lock().unwrap();
            let id = wrapper.inner.as_raw() as usize;
            voices.insert(id, wrapper.clone());
            
            Ok(wrapper)
        }

        pub fn create_submix_voice(
            &self,
            input_channels: u32,
            input_sample_rate: u32,
            flags: u32,
            processing_stage: u32,
            send_list: *const XAUDIO2_SEND_DESCRIPTOR,
        ) -> Result<Arc<dyn XAudio2Voice>, windows::core::Error> {
            let mut voice: Option<IXAudio2SubmixVoice> = None;
            unsafe {
                self.inner.CreateSubmixVoice(&mut voice, input_channels, input_sample_rate, flags, processing_stage, send_list)?;
            }
            let voice = voice.ok_or(windows::core::Error::from_win32())?;
            
            let wrapper = Arc::new(XAudio2SubmixVoiceImpl { inner: voice });
            
            let mut voices = self.voices.lock().unwrap();
            let id = wrapper.inner.as_raw() as usize;
            voices.insert(id, wrapper.clone());
            
            Ok(wrapper)
        }

        pub fn create_mastering_voice(
            &self,
            input_channels: u32,
            input_sample_rate: u32,
            flags: u32,
            device_id: u32,
            send_list: *const XAUDIO2_SEND_DESCRIPTOR,
            effect_chain: *const XAUDIO2_EFFECT_CHAIN,
        ) -> Result<Arc<dyn XAudio2Voice>, windows::core::Error> {
            let mut voice: Option<IXAudio2MasteringVoice> = None;
            unsafe {
                self.inner.CreateMasteringVoice(&mut voice, input_channels, input_sample_rate, flags, device_id, send_list, effect_chain)?;
            }
            let voice = voice.ok_or(windows::core::Error::from_win32())?;
            
            let wrapper = Arc::new(XAudio2MasteringVoiceImpl { inner: voice });
            
            let mut voices = self.voices.lock().unwrap();
            let id = wrapper.inner.as_raw() as usize;
            voices.insert(id, wrapper.clone());
            
            Ok(wrapper)
        }
    }

    /// Common trait for all voice types.
    pub trait XAudio2Voice: Send + Sync {
        fn start(&self, flags: u32, operation_set: u32) -> HRESULT;
        fn stop(&self, flags: u32, operation_set: u32) -> HRESULT;
        fn submit_source_buffer(&self, buffer: &XAUDIO2_BUFFER, buffer_wma: *const XAUDIO2_BUFFER_WMA) -> HRESULT;
        fn flush_source_buffers(&self) -> HRESULT;
        fn get_state(&self, state: *mut XAUDIO2_VOICE_STATE, flags: u32) -> HRESULT;
        fn set_volume(&self, volume: f32, operation_set: u32) -> HRESULT;
        fn set_frequency_ratio(&self, ratio: f32, operation_set: u32) -> HRESULT;
    }

    /// Source voice implementation.
    pub struct XAudio2SourceVoiceImpl {
        inner: IXAudio2SourceVoice,
    }

    impl XAudio2Voice for XAudio2SourceVoiceImpl {
        fn start(&self, flags: u32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.Start(flags, operation_set) }.into()
        }

        fn stop(&self, flags: u32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.Stop(flags, operation_set) }.into()
        }

        fn submit_source_buffer(&self, buffer: &XAUDIO2_BUFFER, buffer_wma: *const XAUDIO2_BUFFER_WMA) -> HRESULT {
            unsafe { self.inner.SubmitSourceBuffer(buffer, buffer_wma) }.into()
        }

        fn flush_source_buffers(&self) -> HRESULT {
            unsafe { self.inner.FlushSourceBuffers() }.into()
        }

        fn get_state(&self, state: *mut XAUDIO2_VOICE_STATE, flags: u32) -> HRESULT {
            unsafe { self.inner.GetState(state, flags) }.into()
        }

        fn set_volume(&self, volume: f32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.SetVolume(volume, operation_set) }.into()
        }

        fn set_frequency_ratio(&self, ratio: f32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.SetFrequencyRatio(ratio, operation_set) }.into()
        }
    }

    /// Submix voice implementation.
    pub struct XAudio2SubmixVoiceImpl {
        inner: IXAudio2SubmixVoice,
    }

    impl XAudio2Voice for XAudio2SubmixVoiceImpl {
        fn start(&self, flags: u32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.Start(flags, operation_set) }.into()
        }

        fn stop(&self, flags: u32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.Stop(flags, operation_set) }.into()
        }

        fn submit_source_buffer(&self, _buffer: &XAUDIO2_BUFFER, _buffer_wma: *const XAUDIO2_BUFFER_WMA) -> HRESULT {
            HRESULT(0) // Not applicable for submix
        }

        fn flush_source_buffers(&self) -> HRESULT {
            HRESULT(0)
        }

        fn get_state(&self, state: *mut XAUDIO2_VOICE_STATE, flags: u32) -> HRESULT {
            unsafe { self.inner.GetState(state, flags) }.into()
        }

        fn set_volume(&self, volume: f32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.SetVolume(volume, operation_set) }.into()
        }

        fn set_frequency_ratio(&self, _ratio: f32, _operation_set: u32) -> HRESULT {
            HRESULT(0) // Not applicable for submix
        }
    }

    /// Mastering voice implementation.
    pub struct XAudio2MasteringVoiceImpl {
        inner: IXAudio2MasteringVoice,
    }

    impl XAudio2Voice for XAudio2MasteringVoiceImpl {
        fn start(&self, flags: u32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.Start(flags, operation_set) }.into()
        }

        fn stop(&self, flags: u32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.Stop(flags, operation_set) }.into()
        }

        fn submit_source_buffer(&self, _buffer: &XAUDIO2_BUFFER, _buffer_wma: *const XAUDIO2_BUFFER_WMA) -> HRESULT {
            HRESULT(0) // Not applicable for mastering
        }

        fn flush_source_buffers(&self) -> HRESULT {
            HRESULT(0)
        }

        fn get_state(&self, state: *mut XAUDIO2_VOICE_STATE, flags: u32) -> HRESULT {
            unsafe { self.inner.GetState(state, flags) }.into()
        }

        fn set_volume(&self, volume: f32, operation_set: u32) -> HRESULT {
            unsafe { self.inner.SetVolume(volume, operation_set) }.into()
        }

        fn set_frequency_ratio(&self, _ratio: f32, _operation_set: u32) -> HRESULT {
            HRESULT(0) // Not applicable for mastering
        }
    }
}