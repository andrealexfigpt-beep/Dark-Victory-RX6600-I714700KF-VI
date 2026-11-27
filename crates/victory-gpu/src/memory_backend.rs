//! Memory backend abstraction for switching between legacy and native backends.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

/// Trait for memory backend implementations.
pub trait MemoryBackend: Send + Sync {
    /// Allocate title virtual memory.
    fn allocate(&self, title_va: u64, size: u64, protect: u32, placeholder: bool) -> anyhow::Result<u64>;

    /// Free title virtual memory.
    fn free(&self, title_va: u64) -> bool;

    /// Create a GPU buffer resource bound to title virtual memory.
    fn create_gpu_resource(&self, title_va: u64, size: u64, uav: bool) -> anyhow::Result<u64>; // Returns GPU VA

    /// Create a GPU texture resource bound to title virtual memory.
    fn create_gpu_texture(&self, title_va: u64, desc: &D3D12_RESOURCE_DESC, uav: bool) -> anyhow::Result<u64>; // Returns GPU VA

    /// Upload data to GPU buffer resource.
    fn upload_to_gpu(&self, title_va: u64, offset: u64, data: &[u8]) -> anyhow::Result<()>;

    /// Upload texture data to GPU texture resource (all subresources).
    fn upload_full_texture_to_gpu(
        &self,
        title_va: u64,
        data: &[u8],
        subresource_data: &[D3D12_SUBRESOURCE_DATA],
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Read back data from GPU resource.
    fn readback_from_gpu(&self, title_va: u64, offset: u64, size: u64) -> anyhow::Result<Vec<u8>>;

    /// Get GPU virtual address for title VA.
    fn get_gpu_va(&self, title_va: u64) -> Option<u64>;

    /// Check if title VA has GPU binding.
    fn has_gpu_binding(&self, title_va: u64) -> bool;

    /// Advance frame counter.
    fn next_frame(&self);

    /// Create an aliased GPU resource (XMemPhysicalPages aliasing).
    fn create_aliased_resource(
        &self,
        title_va: u64,
        source_title_va: u64,
        size: u64,
        uav: bool,
    ) -> anyhow::Result<u64> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Create an aliased GPU texture.
    fn create_aliased_texture(
        &self,
        title_va: u64,
        source_title_va: u64,
        desc: &D3D12_RESOURCE_DESC,
        uav: bool,
    ) -> anyhow::Result<u64> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Create a bottom-level acceleration structure (BLAS) buffer.
    fn create_blas(
        &self,
        title_va: u64,
        size: u64,
        build_inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    ) -> anyhow::Result<u64> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Create a top-level acceleration structure (TLAS) buffer.
    fn create_tlas(&self, title_va: u64, size: u64) -> anyhow::Result<u64> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Get acceleration structure prebuild info.
    fn get_acceleration_structure_prebuild_info(
        &self,
        build_inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    ) -> D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO {
        D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default()
    }

    /// Create a shadow copy for a title buffer (legacy path).
    fn create_shadow_buffer(&self, title_va: u64, size: u64, window: bool) -> anyhow::Result<u64> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Register a title buffer for shadow tracking.
    fn register_buffer(&self, resource: usize, title_va: u64, size: u64) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Unregister a title buffer.
    fn unregister_buffer(&self, resource: usize, title_va: u64) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Find existing shadow for address.
    fn find_shadow(&self, address: u64, len: u64) -> anyhow::Result<Option<u64>> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Refresh a shadow copy range.
    fn refresh_shadow(&self, gpu_va: u64, address: u64, len: u64) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Declare GPU writes for a range.
    fn gpu_writes(&self, gpu_va: u64, address: u64, len: u64) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("Not implemented"))
    }

    /// Copy back GPU writes to title memory.
    fn copy_back(&self, gpu_va: u64, address: u64, len: u64) -> anyhow::Result<usize> {
        Err(anyhow::anyhow!("Not implemented"))
    }
}

/// Legacy backend wrapper (uses existing memory.rs, watch.rs, shadow.rs, upload.rs).
#[cfg(windows)]
pub struct LegacyBackend {
    initialized: AtomicBool,
}

#[cfg(windows)]
impl LegacyBackend {
    pub fn new() -> Self {
        LegacyBackend {
            initialized: AtomicBool::new(false),
        }
    }
}

#[cfg(windows)]
impl MemoryBackend for LegacyBackend {
    fn allocate(&self, title_va: u64, size: u64, protect: u32, placeholder: bool) -> anyhow::Result<u64> {
        // The actual allocation is done in xmem.rs via VirtualAlloc2
        // This just notifies the GPU subsystem
        if !placeholder {
            crate::title_memory_allocated(title_va as usize, size as usize);
        }
        Ok(title_va)
    }

    fn free(&self, title_va: u64) -> bool {
        crate::title_memory_freed(title_va as usize, 0);
        true
    }

    fn create_gpu_resource(&self, title_va: u64, size: u64, uav: bool) -> anyhow::Result<u64> {
        // Legacy path: create shadow copy via shadow.rs
        // This is used for by-address memory (ray tracing structures)
        // The GPU VA comes from the directory/shadow system in memory.rs
        Err(anyhow::anyhow!("Legacy backend uses memory.rs directory for by-address GPU VA"))
    }

    fn create_gpu_texture(&self, title_va: u64, _desc: &D3D12_RESOURCE_DESC, _uav: bool) -> anyhow::Result<u64> {
        // Legacy backend doesn't directly create GPU textures
        // Textures are handled through the shadow/by-address system
        Err(anyhow::anyhow!("Legacy backend uses shadow system for GPU textures"))
    }

    fn upload_to_gpu(&self, title_va: u64, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        // Legacy uses upload.rs + watch.rs + shadow.rs
        Err(anyhow::anyhow!("Legacy backend upload handled by shadow.rs refresh"))
    }

    fn readback_from_gpu(&self, title_va: u64, offset: u64, size: u64) -> anyhow::Result<Vec<u8>> {
        Err(anyhow::anyhow!("Legacy backend readback handled by shadow.rs copy_back"))
    }

    fn get_gpu_va(&self, title_va: u64) -> Option<u64> {
        // Legacy: look up in directory -> shadow copy -> GPU VA
        // This is handled by memory.rs::ready() which returns Ready{directory, faults, base}
        None
    }

    fn has_gpu_binding(&self, title_va: u64) -> bool {
        false
    }

    fn next_frame(&self) {
        // Legacy frame advancement happens in memory::ready()
    }

    fn create_shadow_buffer(&self, title_va: u64, size: u64, window: bool) -> anyhow::Result<u64> {
        // Create a shadow copy via shadow.rs lookup/creation
        // The shadow module uses register/find/lookup internally
        // For now, return the GPU VA that would be obtained via the directory
        Ok(title_va) // Placeholder - actual implementation uses shadow.rs
    }

    fn register_buffer(&self, resource: usize, title_va: u64, size: u64) -> anyhow::Result<()> {
        crate::shadow::register(resource, title_va, size);
        Ok(())
    }

    fn unregister_buffer(&self, resource: usize, title_va: u64) -> anyhow::Result<()> {
        crate::shadow::unregister(resource, title_va);
        Ok(())
    }

    fn find_shadow(&self, address: u64, len: u64) -> anyhow::Result<Option<u64>> {
        if let Some(shadow) = crate::shadow::find(address, len) {
            // Return the GPU address of the shadow copy
            let gpu_va = shadow.gpu(address);
            Ok(Some(gpu_va))
        } else {
            Ok(None)
        }
    }

    fn refresh_shadow(&self, gpu_va: u64, address: u64, len: u64) -> anyhow::Result<()> {
        // In legacy, this is done via HostList::defer_refresh which calls Shadow::refresh
        // The gpu_va here would be used to find the shadow
        Ok(())
    }

    fn gpu_writes(&self, gpu_va: u64, address: u64, len: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn copy_back(&self, gpu_va: u64, address: u64, len: u64) -> anyhow::Result<usize> {
        Ok(0)
    }
}

/// Native backend wrapper.
#[cfg(all(windows, feature = "native-memory"))]
pub struct NativeBackend {
    inner: Arc<crate::native_mem::NativeMemory>,
}

#[cfg(all(windows, feature = "native-memory"))]
impl NativeBackend {
    pub fn new(device: Arc<ID3D12Device>, copy_queue: Arc<ID3D12CommandQueue>) -> anyhow::Result<Self> {
        let inner = Arc::new(crate::native_mem::NativeMemory::new(device, copy_queue)?);
        Ok(NativeBackend { inner })
    }

    /// Get the inner NativeMemory for advanced operations.
    pub fn inner(&self) -> Arc<crate::native_mem::NativeMemory> {
        self.inner.clone()
    }
}

#[cfg(all(windows, feature = "native-memory"))]
impl MemoryBackend for NativeBackend {
    fn allocate(&self, title_va: u64, size: u64, protect: u32, placeholder: bool) -> anyhow::Result<u64> {
        self.inner.allocate(title_va, size, protect, placeholder)
    }

    fn free(&self, title_va: u64) -> bool {
        self.inner.free(title_va)
    }

    fn create_gpu_resource(&self, title_va: u64, size: u64, uav: bool) -> anyhow::Result<u64> {
        let binding = self.inner.create_gpu_resource(title_va, size, uav)?;
        Ok(binding.gpu_va)
    }

    fn create_gpu_texture(&self, title_va: u64, desc: &D3D12_RESOURCE_DESC, uav: bool) -> anyhow::Result<u64> {
        let binding = self.inner.create_gpu_texture(title_va, desc, uav)?;
        Ok(binding.gpu_va)
    }

    fn upload_to_gpu(&self, title_va: u64, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        self.inner.upload_to_gpu(title_va, offset, data)
    }

    fn upload_full_texture_to_gpu(
        &self,
        title_va: u64,
        data: &[u8],
        subresource_data: &[D3D12_SUBRESOURCE_DATA],
    ) -> anyhow::Result<()> {
        self.inner.upload_full_texture_to_gpu(title_va, data, subresource_data)
    }

    fn readback_from_gpu(&self, title_va: u64, offset: u64, size: u64) -> anyhow::Result<Vec<u8>> {
        self.inner.readback_from_gpu(title_va, offset, size)
    }

    fn get_gpu_va(&self, title_va: u64) -> Option<u64> {
        self.inner.get_gpu_va(title_va)
    }

    fn has_gpu_binding(&self, title_va: u64) -> bool {
        self.inner.has_gpu_binding(title_va)
    }

    fn next_frame(&self) {
        self.inner.next_frame();
    }

    fn create_aliased_resource(
        &self,
        title_va: u64,
        source_title_va: u64,
        size: u64,
        uav: bool,
    ) -> anyhow::Result<u64> {
        let binding = self.inner.create_aliased_resource(title_va, source_title_va, size, uav)?;
        Ok(binding.gpu_va)
    }

    fn create_aliased_texture(
        &self,
        title_va: u64,
        source_title_va: u64,
        desc: &D3D12_RESOURCE_DESC,
        uav: bool,
    ) -> anyhow::Result<u64> {
        let binding = self.inner.create_aliased_texture(title_va, source_title_va, desc, uav)?;
        Ok(binding.gpu_va)
    }

    fn create_blas(
        &self,
        title_va: u64,
        size: u64,
        build_inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    ) -> anyhow::Result<u64> {
        let binding = self.inner.create_blas(title_va, size, build_inputs)?;
        Ok(binding.gpu_va)
    }

    fn create_tlas(&self, title_va: u64, size: u64) -> anyhow::Result<u64> {
        let binding = self.inner.create_tlas(title_va, size)?;
        Ok(binding.gpu_va)
    }

    fn get_acceleration_structure_prebuild_info(
        &self,
        build_inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    ) -> D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO {
        self.inner.get_acceleration_structure_prebuild_info(build_inputs)
    }
}

/// Global backend instance.
static MEMORY_BACKEND: OnceLock<Box<dyn MemoryBackend>> = OnceLock::new();

/// Initialize the memory backend (legacy or native).
#[cfg(windows)]
pub fn init_memory_backend(use_native: bool, device: Option<Arc<ID3D12Device>>, copy_queue: Option<Arc<ID3D12CommandQueue>>) -> anyhow::Result<&'static dyn MemoryBackend> {
    if use_native {
        #[cfg(feature = "native-memory")]
        {
            let device = device.ok_or_else(|| anyhow::anyhow!("Native backend requires device"))?;
            let copy_queue = copy_queue.ok_or_else(|| anyhow::anyhow!("Native backend requires copy queue"))?;
            let backend = Box::new(NativeBackend::new(device, copy_queue)?);
            MEMORY_BACKEND.set(backend).map_err(|_| anyhow::anyhow!("Backend already initialized"))?;
            return Ok(MEMORY_BACKEND.get().unwrap().as_ref());
        }
        #[cfg(not(feature = "native-memory"))]
        {
            return Err(anyhow::anyhow!("Native memory feature not enabled"));
        }
    } else {
        let backend = Box::new(LegacyBackend::new());
        MEMORY_BACKEND.set(backend).map_err(|_| anyhow::anyhow!("Backend already initialized"))?;
        return Ok(MEMORY_BACKEND.get().unwrap().as_ref());
    }
}

/// Initialize the memory backend automatically based on environment variable.
/// Checks `VICTORY_NATIVE_MEMORY=1` to enable native backend.
#[cfg(windows)]
pub fn init_memory_backend_auto(device: Option<Arc<ID3D12Device>>, copy_queue: Option<Arc<ID3D12CommandQueue>>) -> anyhow::Result<&'static dyn MemoryBackend> {
    let use_native = std::env::var("VICTORY_NATIVE_MEMORY")
        .ok()
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(false);
    
    init_memory_backend(use_native, device, copy_queue)
}

/// Get the current memory backend.
#[cfg(windows)]
pub fn memory_backend() -> Option<&'static dyn MemoryBackend> {
    MEMORY_BACKEND.get().map(|b| b.as_ref())
}

/// Check if native backend is active.
#[cfg(windows)]
pub fn is_native_backend_active() -> bool {
    #[cfg(feature = "native-memory")]
    {
        crate::native_mem::is_native_memory_active()
    }
    #[cfg(not(feature = "native-memory"))]
    {
        false
    }
}

use std::sync::OnceLock;