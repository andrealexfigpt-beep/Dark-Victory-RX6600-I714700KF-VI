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

    /// Create a GPU resource bound to title virtual memory.
    fn create_gpu_resource(&self, title_va: u64, size: u64, uav: bool) -> anyhow::Result<u64>; // Returns GPU VA

    /// Upload data to GPU resource.
    fn upload_to_gpu(&self, title_va: u64, offset: u64, data: &[u8]) -> anyhow::Result<()>;

    /// Read back data from GPU resource.
    fn readback_from_gpu(&self, title_va: u64, offset: u64, size: u64) -> anyhow::Result<Vec<u8>>;

    /// Get GPU virtual address for title VA.
    fn get_gpu_va(&self, title_va: u64) -> Option<u64>;

    /// Check if title VA has GPU binding.
    fn has_gpu_binding(&self, title_va: u64) -> bool;

    /// Advance frame counter.
    fn next_frame(&self);
}

/// Legacy backend wrapper (uses existing memory.rs, watch.rs, shadow.rs, upload.rs).
#[cfg(windows)]
pub struct LegacyBackend {
    // The legacy backend uses the existing global state in memory.rs, watch.rs, etc.
    // We just need to ensure the existing functions are called.
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
        // Legacy backend uses XMemVirtualAlloc in xmem.rs which calls
        // victory_gpu::title_memory_allocated
        // For now, return success - the actual allocation happens in xmem
        Ok(title_va)
    }

    fn free(&self, title_va: u64) -> bool {
        // Legacy uses XMemVirtualFree
        true
    }

    fn create_gpu_resource(&self, title_va: u64, size: u64, uav: bool) -> anyhow::Result<u64> {
        // Legacy creates shadow copies via shadow.rs
        // GPU VA is obtained through the directory/shadow system
        Err(anyhow::anyhow!("Legacy backend GPU resource creation not directly exposed"))
    }

    fn upload_to_gpu(&self, title_va: u64, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        // Legacy uses upload.rs + watch.rs + shadow.rs
        Err(anyhow::anyhow!("Legacy backend upload not directly exposed"))
    }

    fn readback_from_gpu(&self, title_va: u64, offset: u64, size: u64) -> anyhow::Result<Vec<u8>> {
        Err(anyhow::anyhow!("Legacy backend readback not directly exposed"))
    }

    fn get_gpu_va(&self, title_va: u64) -> Option<u64> {
        // Legacy: look up in directory -> shadow copy -> GPU VA
        None
    }

    fn has_gpu_binding(&self, title_va: u64) -> bool {
        false
    }

    fn next_frame(&self) {
        // Legacy frame advancement happens in memory::ready()
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

    fn upload_to_gpu(&self, title_va: u64, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        self.inner.upload_to_gpu(title_va, offset, data)
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