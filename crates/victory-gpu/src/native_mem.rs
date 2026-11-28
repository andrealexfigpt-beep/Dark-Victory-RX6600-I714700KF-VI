//! Native Windows memory backend for Dark Victory.
//!
//! Replaces the unified-memory translation layer (memory.rs, watch.rs, shadow.rs, upload.rs)
//! with direct Windows/D3D12 allocations. The Xbox Series X|S has unified GDDR6 memory;
//! PC has discrete system RAM + VRAM. This backend eliminates the translation overhead
//! by using native host allocations and D3D12 heaps directly.
//!
//! Key differences from the translation layer:
//! - Sparse allocation registry (DashMap) instead of 67M-entry dense directory
//! - Direct GPU resource binding: title VA -> ID3D12Resource (no shadow copies)
//! - Persistent upload/readback ring buffers with batch fence synchronization
//! - No page faults, no write watching, no journal, no ASKING mutex
//! - CPU/GPU coherence via explicit Map/Unmap + fence ordering (standard D3D12)

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::ptr;

use dashmap::DashMap;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HANDLE, CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Memory::{VirtualFree, MEM_COMMIT, MEM_RESERVE, MEM_RELEASE, PAGE_READWRITE, PAGE_NOACCESS};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use windows::core::Interface;
use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT as SYS_MEM_COMMIT, MEM_RESERVE as SYS_MEM_RESERVE, PAGE_READWRITE as SYS_PAGE_READWRITE, PAGE_NOACCESS as SYS_PAGE_NOACCESS, MEM_RELEASE as SYS_MEM_RELEASE};

use crate::host::{host, Host, HostList};
use crate::resource::{AllocationInfo, footprint};

/// Page size matching the console's 64 KiB page.
pub const PAGE: u64 = 64 * 1024;

/// Maximum title address space (4 GiB = 2^26 pages).
pub const MAX_TITLE_PAGES: u64 = 1 << 26;

/// Upload ring buffer size (64 MiB default).
const UPLOAD_RING_SIZE: u64 = 64 * 1024 * 1024;

/// Readback ring buffer size (16 MiB default).
const READBACK_RING_SIZE: u64 = 16 * 1024 * 1024;

/// Minimum heap allocation size (64 KiB).
const MIN_HEAP_SIZE: u64 = 64 * 1024;

/// Arena size for GPU heaps (256 MiB).
const GPU_ARENA_SIZE: u64 = 256 * 1024 * 1024;

/// Upload heap alignment (64 KiB).
pub const UPLOAD_ALIGNMENT: u64 = 64 * 1024;

/// Default heap alignment (64 KiB).
const DEFAULT_ALIGNMENT: u64 = 64 * 1024;

/// A host allocation backing title virtual memory.
pub struct HostAllocation {
    /// Base address in title virtual address space.
    pub title_va: u64,
    /// Size in bytes.
    pub size: u64,
    /// Protection flags (PAGE_READWRITE, etc.).
    protect: u32,
    /// Whether this is a placeholder (reserved but not committed).
    placeholder: AtomicBool,
    /// Underlying Windows allocation (for VirtualFree on drop).
    pub base_ptr: *mut std::ffi::c_void,
    /// Actual committed size.
    committed_size: AtomicU64,
}

impl std::fmt::Debug for HostAllocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostAllocation")
            .field("title_va", &self.title_va)
            .field("size", &self.size)
            .field("protect", &self.protect)
            .field("placeholder", &self.is_placeholder())
            .field("base_ptr", &self.base_ptr)
            .field("committed_size", &self.committed_size())
            .finish()
    }
}

impl HostAllocation {
    pub fn is_placeholder(&self) -> bool {
        self.placeholder.load(Ordering::Acquire)
    }

    pub fn committed_size(&self) -> u64 {
        self.committed_size.load(Ordering::Acquire)
    }

    pub(crate) fn set_committed(&self, size: u64) {
        self.placeholder.store(false, Ordering::Release);
        self.committed_size.store(size, Ordering::Release);
    }
}

impl Drop for HostAllocation {
    fn drop(&mut self) {
        if !self.base_ptr.is_null() {
            // SAFETY: We allocated this with VirtualAlloc.
            unsafe {
                if let Err(error) = VirtualFree(self.base_ptr, 0, MEM_RELEASE) { warn!(%error, "native memory cleanup failed"); }
            }
        }
    }
}

impl Drop for NativeMemory {
    fn drop(&mut self) {
        // Close event handles
        if !self.copy_fence_event.is_invalid() {
            unsafe {
                if let Err(error) = CloseHandle(self.copy_fence_event) { warn!(%error, "native memory cleanup failed"); }
            }
        }
        // UploadRing and ReadbackRing events are dropped with their structs
    }
}

/// A GPU resource bound to a title virtual address range.
pub struct GpuResourceBinding {
    /// Title virtual address this resource is bound to.
    title_va: u64,
    /// Size in bytes.
    size: u64,
    /// The D3D12 device.
    device: Arc<ID3D12Device>,
    /// The D3D12 resource (DEFAULT heap for GPU access).
    resource: ID3D12Resource,
    /// GPU virtual address of the resource.
    pub gpu_va: u64,
    /// Whether this resource can be written by shaders (UAV).
    uav: bool,
    /// Resource dimension (buffer, texture1D, texture2D, texture3D).
    dimension: D3D12_RESOURCE_DIMENSION,
    /// Texture format (for textures).
    format: DXGI_FORMAT,
    /// Number of mip levels (for textures).
    mip_levels: u16,
    /// Array size (for texture arrays).
    array_size: u16,
    /// Sample count.
    sample_count: u32,
    /// Texture width (for textures).
    width: u64,
    /// Texture height (for textures).
    height: u32,
    /// Texture depth or array size (for textures).
    depth_or_array_size: u16,
    /// Optional upload heap staging resource for CPU->GPU copies.
    upload_staging: Option<ID3D12Resource>,
    /// Optional readback heap staging resource for GPU->CPU copies.
    readback_staging: Option<ID3D12Resource>,
    /// Fence value for tracking GPU completion on this resource.
    fence_value: AtomicU64,
    /// Aliased resources sharing the same heap memory (for XMemPhysicalPages aliasing).
    active_alias: Arc<Mutex<Option<ID3D12Resource>>>,
    aliased_resources: Arc<Mutex<Vec<ID3D12Resource>>>,
    /// Heap offset for placed resources (for aliasing).
    heap_offset: u64,
    /// Heap reference for placed resources.
    heap: Option<ID3D12Heap>,
}

impl std::fmt::Debug for GpuResourceBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuResourceBinding")
            .field("title_va", &self.title_va)
            .field("size", &self.size)
            .field("gpu_va", &self.gpu_va)
            .field("uav", &self.uav)
            .field("dimension", &self.dimension)
            .field("format", &self.format)
            .field("mip_levels", &self.mip_levels)
            .field("array_size", &self.array_size)
            .field("sample_count", &self.sample_count)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("depth_or_array_size", &self.depth_or_array_size)
            .field("heap_offset", &self.heap_offset)
            .field("heap", &self.heap.is_some())
            .finish_non_exhaustive()
    }
}

impl GpuResourceBinding {
    /// Create a new binding for a buffer resource.
    fn new_buffer(
        device: Arc<ID3D12Device>,
        title_va: u64,
        size: u64,
        resource: ID3D12Resource,
        gpu_va: u64,
        uav: bool,
    ) -> Self {
        Self {
            title_va,
            size,
            device,
            resource,
            gpu_va,
            uav,
            dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            format: DXGI_FORMAT_UNKNOWN,
            mip_levels: 1,
            array_size: 1,
            sample_count: 1,
            width: 0,
            height: 0,
            depth_or_array_size: 1,
            upload_staging: None,
            readback_staging: None,
            fence_value: AtomicU64::new(0),
            active_alias: Arc::default(),
            aliased_resources: Arc::new(Mutex::new(Vec::new())),
            heap_offset: 0,
            heap: None,
        }
    }

    /// Create a new binding for a texture resource.
    fn new_texture(
        device: Arc<ID3D12Device>,
        title_va: u64,
        size: u64,
        resource: ID3D12Resource,
        gpu_va: u64,
        uav: bool,
        desc: &D3D12_RESOURCE_DESC,
        heap_offset: u64,
        heap: Option<ID3D12Heap>,
    ) -> Self {
        Self {
            title_va,
            size,
            device,
            resource,
            gpu_va,
            uav,
            dimension: desc.Dimension,
            format: desc.Format,
            mip_levels: desc.MipLevels,
            array_size: desc.DepthOrArraySize,
            sample_count: desc.SampleDesc.Count,
            width: desc.Width,
            height: desc.Height,
            depth_or_array_size: desc.DepthOrArraySize,
            upload_staging: None,
            readback_staging: None,
            fence_value: AtomicU64::new(0),
            active_alias: Arc::default(),
            aliased_resources: Arc::new(Mutex::new(Vec::new())),
            heap_offset,
            heap,
        }
    }

    /// Add an aliased resource (for XMemPhysicalPages aliasing).
    fn activate(&self, list: &ID3D12GraphicsCommandList) {
        use std::mem::ManuallyDrop;
        let mut active = self.active_alias.lock().unwrap();
        if active.as_ref() == Some(&self.resource) { return; }
        let mut barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING, Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 { Aliasing: ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                pResourceBefore: ManuallyDrop::new(active.take()), pResourceAfter: ManuallyDrop::new(Some(self.resource.clone()))
            }) }
        };
        unsafe {
            list.ResourceBarrier(std::slice::from_ref(&barrier));
            ManuallyDrop::drop(&mut (*barrier.Anonymous.Aliasing).pResourceBefore);
            ManuallyDrop::drop(&mut (*barrier.Anonymous.Aliasing).pResourceAfter);
        }
        *active = Some(self.resource.clone());
    }
    fn add_alias(&self, resource: ID3D12Resource) {
        self.aliased_resources.lock().unwrap().push(resource);
    }

    /// Get all aliased resources.
    fn get_aliases(&self) -> Vec<ID3D12Resource> {
        self.aliased_resources.lock().unwrap().clone()
    }

    /// Check if this is a texture resource.
    pub fn resource(&self) -> &ID3D12Resource { &self.resource }

    fn is_texture(&self) -> bool {
        matches!(
            self.dimension,
            D3D12_RESOURCE_DIMENSION_TEXTURE1D
                | D3D12_RESOURCE_DIMENSION_TEXTURE2D
                | D3D12_RESOURCE_DIMENSION_TEXTURE3D
        )
    }

    /// Calculate upload footprint for a texture subresource.
    fn texture_footprint(&self, device: &ID3D12Device, subresource: u32) -> (D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u32, u32, u64) {
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut rows = 0u32;
        let mut row_pitch = 0u64;
        let mut total_bytes = 0u64;
        
        unsafe {
            device.GetCopyableFootprints(
                &D3D12_RESOURCE_DESC {
                    Dimension: self.dimension,
                    Alignment: 0,
                    Width: self.width,
                    Height: self.height,
                    DepthOrArraySize: self.depth_or_array_size,
                    MipLevels: self.mip_levels,
                    Format: self.format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: self.sample_count, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                    Flags: if self.uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAGS(0) },
                },
                subresource,
                1,
                0,
                Some(&mut footprint),
                Some(&mut rows),
                Some(&mut row_pitch),
                Some(&mut total_bytes),
            );
        }
        
        (footprint, rows, row_pitch as u32, total_bytes)
    }

    /// Get the full footprint for a subresource (for CopyTextureRegion).
    fn get_subresource_footprint(&self, subresource: u32) -> D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut rows = 0u32;
        let mut row_pitch = 0u64;
        let mut total_bytes = 0u64;
        
        unsafe {
            self.device.GetCopyableFootprints(
                &D3D12_RESOURCE_DESC {
                    Dimension: self.dimension,
                    Alignment: 0,
                    Width: self.width,
                    Height: self.height,
                    DepthOrArraySize: self.depth_or_array_size,
                    MipLevels: self.mip_levels,
                    Format: self.format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: self.sample_count, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                    Flags: if self.uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAGS(0) },
                },
                subresource,
                1,
                0,
                Some(&mut footprint),
                Some(&mut rows),
                Some(&mut row_pitch),
                Some(&mut total_bytes),
            );
        }
        
        footprint
    }
}

/// GPU heap arena for sub-allocating resources.
struct GpuArena {
    /// The D3D12 heap.
    heap: ID3D12Heap,
    /// Heap type.
    heap_type: D3D12_HEAP_TYPE,
    /// Heap flags.
    heap_flags: D3D12_HEAP_FLAGS,
    /// Base GPU virtual address.
    base_gpu_va: u64,
    /// Total size.
    size: u64,
    /// Free list: (offset, size) pairs.
    free_list: Mutex<Vec<(u64, u64)>>,
    /// Allocated resources in this arena (for tracking).
    allocations: Mutex<HashMap<u64, (u64, u64)>>, // gpu_va -> (offset, size)
}

impl std::fmt::Debug for GpuArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuArena")
            .field("heap_type", &self.heap_type)
            .field("heap_flags", &self.heap_flags)
            .field("base_gpu_va", &self.base_gpu_va)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl GpuArena {
    fn new(device: &ID3D12Device, size: u64, heap_type: D3D12_HEAP_TYPE, flags: D3D12_HEAP_FLAGS) -> anyhow::Result<Self> {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: size,
            Properties: D3D12_HEAP_PROPERTIES {
                Type: heap_type,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
                MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
                CreationNodeMask: 1,
                VisibleNodeMask: 1,
            },
            Alignment: D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT as u64,
            Flags: flags,
        };

        let mut heap: Option<ID3D12Heap> = None;
        unsafe { device.CreateHeap(&desc, &mut heap) }?;
        let heap = heap.unwrap();

        Ok(GpuArena {
            heap,
            heap_type,
            heap_flags: flags,
            base_gpu_va: 0,
            size,
            free_list: Mutex::new(vec![(0, size)]),
            allocations: Mutex::new(HashMap::new()),
        })
    }

    /// Get the heap reference.
    fn heap(&self) -> &ID3D12Heap {
        &self.heap
    }

    /// Get the heap type.
    fn heap_type(&self) -> D3D12_HEAP_TYPE {
        self.heap_type
    }

    /// Get the heap flags.
    fn heap_flags(&self) -> D3D12_HEAP_FLAGS {
        self.heap_flags
    }

    fn allocate(&self, size: u64, alignment: u64) -> Option<u64> {
        let mut free = self.free_list.lock().unwrap();
        let aligned_size = size.next_multiple_of(alignment);
        for (i, &(offset, free_size)) in free.iter().enumerate() {
            let aligned_offset = offset.next_multiple_of(alignment);
            if aligned_offset + aligned_size <= offset + free_size {
                // Found space
                let new_offset = aligned_offset;
                let remaining_before = aligned_offset - offset;
                let remaining_after = offset + free_size - (aligned_offset + aligned_size);

                free.remove(i);
                if remaining_before > 0 {
                    free.push((offset, remaining_before));
                }
                if remaining_after > 0 {
                    free.push((aligned_offset + aligned_size, remaining_after));
                }
                free.sort_by_key(|&(o, _)| o);
                return Some(new_offset);
            }
        }
        None
    }

    fn free(&self, offset: u64, size: u64) {
        let mut free = self.free_list.lock().unwrap();
        free.push((offset, size));
        free.sort_by_key(|&(o, _)| o);
        // Merge adjacent free blocks
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for &(o, s) in free.iter() {
            if let Some(last) = merged.last_mut() {
                if last.0 + last.1 == o {
                    last.1 += s;
                } else {
                    merged.push((o, s));
                }
            } else {
                merged.push((o, s));
            }
        }
        *free = merged;
    }
}

/// Persistent ring buffer for uploads (CPU -> GPU).
/// Uses fence-safe wraparound to prevent reuse of in-flight memory.
pub struct UploadRing {
    /// The upload heap resource.
    resource: ID3D12Resource,
    /// Mapped CPU pointer.
    mapped_ptr: *mut u8,
    /// Total size.
    size: u64,
    /// Current write offset.
    write_offset: AtomicU64,
    /// Fence for synchronization.
    fence: ID3D12Fence,
    /// Current fence value.
    fence_value: AtomicU64,
    /// Event for fence waiting.
    fence_event: HANDLE,
    /// Pending allocations: (offset, size, fence_value).
    /// Protected by a mutex for thread-safe access.
    pending: Mutex<Vec<(u64, u64, u64)>>,
}

impl std::fmt::Debug for UploadRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadRing")
            .field("size", &self.size)
            .field("write_offset", &self.write_offset.load(Ordering::Relaxed))
            .field("fence_value", &self.fence_value.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

// SAFETY: UploadRing contains a raw pointer (mapped_ptr) but all access is protected
// by the Mutex<pending> and AtomicU64 operations. The mapped pointer is a valid
// mapped upload buffer that is only accessed by the thread that allocated it.
unsafe impl Send for UploadRing {}
unsafe impl Sync for UploadRing {}

impl UploadRing {
    fn new(device: &ID3D12Device, size: u64) -> anyhow::Result<Self> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: UPLOAD_ALIGNMENT,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAGS(0),
        };

        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        };

        let mut resource: Option<ID3D12Resource> = None;
        unsafe { device.CreateCommittedResource(&heap_props, D3D12_HEAP_FLAGS(0), &desc, D3D12_RESOURCE_STATE_GENERIC_READ, None, &mut resource) }?;
        let resource = resource.unwrap();

        // Map the resource for CPU writes
        let mut mapped_ptr: *mut u8 = std::ptr::null_mut();
        unsafe {
            resource.Map(0, None, Some(&mut mapped_ptr as *mut _ as *mut _))?;
        }

        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAGS(0))? };
        let fence_event = unsafe { CreateEventW(None, false, false, None)? };

        Ok(UploadRing {
            resource,
            mapped_ptr,
            size,
            write_offset: AtomicU64::new(0),
            fence,
            fence_value: AtomicU64::new(1),
            fence_event,
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Allocate space in the ring buffer for an upload.
    /// Returns (offset, mapped_ptr) or None if full (caller should wait and retry).
    /// Implements fence-safe wraparound: waits for oldest pending allocation
    /// to complete before reusing its space.
    pub fn allocate(&self, size: u64, alignment: u64) -> Option<(u64, *mut u8)> {
        let size = size.next_multiple_of(alignment);
        
        // Try to allocate without wraparound first
        let current = self.write_offset.load(Ordering::Acquire);
        let next = current + size;
        if next <= self.size {
            if self.write_offset.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let ptr = unsafe { self.mapped_ptr.add(current as usize) };
                // Record this allocation with a placeholder fence value (0 = not yet signaled)
                self.pending.lock().unwrap().push((current, size, 0));
                return Some((current, ptr));
            }
        }
        
        // Need to wrap around - check if oldest pending allocation has completed
        let completed_fence = unsafe { self.fence.GetCompletedValue() };
        
        let mut pending = self.pending.lock().unwrap();
        
        // Remove completed allocations from the front
        while let Some(&(oldest_offset, oldest_size, oldest_fence)) = pending.first() {
            if oldest_fence == 0 || completed_fence >= oldest_fence {
                pending.remove(0);
            } else {
                break;
            }
        }
        
        // Calculate available space after completed allocations
        let used_space: u64 = pending.iter().map(|(_, s, _)| *s).sum();
        let available_space = self.size - used_space;
        
        if size > available_space {
            // Not enough space even after cleanup - caller should wait and retry
            return None;
        }
        
        // Find a free region
        if pending.is_empty() {
            // No pending allocations, can reset to beginning
            self.write_offset.store(0, Ordering::Release);
            if self.write_offset.compare_exchange(0, size, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let ptr = unsafe { self.mapped_ptr.add(0) };
                pending.push((0, size, 0));
                return Some((0, ptr));
            }
        }
        
        // Find gap after last pending allocation
        let last_end = pending.last().map(|(o, s, _)| o + s).unwrap_or(0);
        if last_end + size <= self.size {
            if self.write_offset.compare_exchange(last_end, last_end + size, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let ptr = unsafe { self.mapped_ptr.add(last_end as usize) };
                pending.push((last_end, size, 0));
                return Some((last_end, ptr));
            }
        }
        
        None
    }

    /// Signal the fence after recording copy commands.
    /// Updates the fence value for all pending allocations that don't have one yet.
    pub fn signal_fence(&self, queue: &ID3D12CommandQueue) -> u64 {
        let value = self.fence_value.fetch_add(1, Ordering::AcqRel);
        unsafe { queue.Signal(&self.fence, value) }.ok();
        
        // Update the fence value for all pending allocations that were made since the last signal
        let mut pending = self.pending.lock().unwrap();
        for alloc in pending.iter_mut() {
            if alloc.2 == 0 {
                alloc.2 = value;
            }
        }
        
        value
    }

    /// Wait for a fence value (blocking).
    pub fn wait_fence(&self, value: u64) {
        unsafe { self.fence.SetEventOnCompletion(value, self.fence_event) }.ok();
        unsafe { WaitForSingleObject(self.fence_event, u32::MAX) };
    }
    
    /// Get the current completed fence value.
    pub fn completed_fence(&self) -> u64 {
        unsafe { self.fence.GetCompletedValue() }
    }
}

impl Drop for UploadRing {
    fn drop(&mut self) {
        if !self.fence_event.is_invalid() {
            unsafe {
                if let Err(error) = CloseHandle(self.fence_event) { warn!(%error, "native memory cleanup failed"); }
            }
        }
        // Resource and fence are reference-counted, will be dropped automatically
    }
}

/// Persistent ring buffer for readbacks (GPU -> CPU).
/// Uses fence-safe wraparound to prevent reuse of in-flight memory.
pub struct ReadbackRing {
    resource: ID3D12Resource,
    mapped_ptr: *mut u8,
    size: u64,
    read_offset: AtomicU64,
    fence: ID3D12Fence,
    fence_value: AtomicU64,
    fence_event: HANDLE,
    /// Pending allocations: (offset, size, fence_value).
    pending: Mutex<Vec<(u64, u64, u64)>>,
}

impl std::fmt::Debug for ReadbackRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadbackRing")
            .field("size", &self.size)
            .field("read_offset", &self.read_offset.load(Ordering::Relaxed))
            .field("fence_value", &self.fence_value.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

// SAFETY: ReadbackRing contains a raw pointer (mapped_ptr) but all access is protected
// by the Mutex<pending> and AtomicU64 operations.
unsafe impl Send for ReadbackRing {}
unsafe impl Sync for ReadbackRing {}

impl ReadbackRing {
    fn new(device: &ID3D12Device, size: u64) -> anyhow::Result<Self> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: UPLOAD_ALIGNMENT,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAGS(0),
        };

        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        };

        let mut resource: Option<ID3D12Resource> = None;
        unsafe { device.CreateCommittedResource(&heap_props, D3D12_HEAP_FLAGS(0), &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut resource) }?;
        let resource = resource.unwrap();

        let mut mapped_ptr: *mut u8 = std::ptr::null_mut();
        unsafe {
            resource.Map(0, None, Some(&mut mapped_ptr as *mut _ as *mut _))?;
        }

        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAGS(0))? };
        let fence_event = unsafe { CreateEventW(None, false, false, None)? };

        Ok(ReadbackRing {
            resource,
            mapped_ptr,
            size,
            read_offset: AtomicU64::new(0),
            fence,
            fence_value: AtomicU64::new(1),
            fence_event,
            pending: Mutex::new(Vec::new()),
        })
    }

    pub fn allocate(&self, size: u64, alignment: u64) -> Option<(u64, *mut u8)> {
        let size = size.next_multiple_of(alignment);
        
        // Try to allocate without wraparound first
        let current = self.read_offset.load(Ordering::Acquire);
        let next = current + size;
        if next <= self.size {
            if self.read_offset.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let ptr = unsafe { self.mapped_ptr.add(current as usize) };
                self.pending.lock().unwrap().push((current, size, 0));
                return Some((current, ptr));
            }
        }
        
        // Need to wrap around - check if oldest pending allocation has completed
        let completed_fence = unsafe { self.fence.GetCompletedValue() };
        
        let mut pending = self.pending.lock().unwrap();
        
        // Remove completed allocations from the front
        while let Some(&(oldest_offset, oldest_size, oldest_fence)) = pending.first() {
            if oldest_fence == 0 || completed_fence >= oldest_fence {
                pending.remove(0);
            } else {
                break;
            }
        }
        
        // Calculate available space after completed allocations
        let used_space: u64 = pending.iter().map(|(_, s, _)| *s).sum();
        let available_space = self.size - used_space;
        
        if size > available_space {
            return None;
        }
        
        // Find a free region
        if pending.is_empty() {
            self.read_offset.store(0, Ordering::Release);
            if self.read_offset.compare_exchange(0, size, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let ptr = unsafe { self.mapped_ptr.add(0) };
                pending.push((0, size, 0));
                return Some((0, ptr));
            }
        }
        
        let last_end = pending.last().map(|(o, s, _)| o + s).unwrap_or(0);
        if last_end + size <= self.size {
            if self.read_offset.compare_exchange(last_end, last_end + size, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let ptr = unsafe { self.mapped_ptr.add(last_end as usize) };
                pending.push((last_end, size, 0));
                return Some((last_end, ptr));
            }
        }
        
        None
    }

    pub fn signal_fence(&self, queue: &ID3D12CommandQueue) -> u64 {
        let value = self.fence_value.fetch_add(1, Ordering::AcqRel);
        unsafe { queue.Signal(&self.fence, value) }.ok();
        
        let mut pending = self.pending.lock().unwrap();
        for alloc in pending.iter_mut() {
            if alloc.2 == 0 {
                alloc.2 = value;
            }
        }
        
        value
    }

    pub fn wait_fence(&self, value: u64) {
        unsafe { self.fence.SetEventOnCompletion(value, self.fence_event) }.ok();
        unsafe { WaitForSingleObject(self.fence_event, u32::MAX) };
    }
    
    pub fn completed_fence(&self) -> u64 {
        unsafe { self.fence.GetCompletedValue() }
    }
}

impl Drop for ReadbackRing {
    fn drop(&mut self) {
        if !self.fence_event.is_invalid() {
            unsafe {
                if let Err(error) = CloseHandle(self.fence_event) { warn!(%error, "native memory cleanup failed"); }
            }
        }
    }
}

/// The native memory manager.
/// Replaces Memory + Shadow + Watch + Upload with a single coherent system.
pub struct NativeMemory {
    device: Arc<ID3D12Device>,
    copy_queue: Arc<ID3D12CommandQueue>,

    /// Copy command allocator and list for texture operations.
    copy_allocator: Mutex<ID3D12CommandAllocator>,
    copy_list: Mutex<ID3D12GraphicsCommandList>,
    copy_fence: ID3D12Fence,
    copy_fence_value: AtomicU64,
    copy_fence_event: HANDLE,

    /// Sparse allocation registry: title VA -> HostAllocation.
    /// Uses DashMap for lock-free concurrent access.
    allocations: DashMap<u64, Arc<HostAllocation>>,

    /// GPU resource bindings: title VA -> GpuResourceBinding.
    gpu_bindings: DashMap<u64, Arc<GpuResourceBinding>>,

    /// GPU arenas for DEFAULT heap resources.
    default_arenas: Mutex<Vec<Arc<GpuArena>>>,

    /// GPU arenas for UPLOAD heap resources.
    upload_arenas: Mutex<Vec<Arc<GpuArena>>>,

    /// Persistent upload ring buffer.
    upload_ring: OnceLock<UploadRing>,

    /// Persistent readback ring buffer.
    readback_ring: OnceLock<ReadbackRing>,

    /// Deferred resource destruction queue (fence value -> resources to destroy).
    deferred_destruction: Mutex<BTreeMap<u64, Vec<DeferredResource>>>,

    /// Frame counter for statistics.
    frame: AtomicU64,

    /// Statistics.
    stats: NativeMemoryStats,
}

// SAFETY: NativeMemory contains raw pointers in UploadRing and ReadbackRing,
// but all access to those is protected by OnceLock and Mutex. The Arc<NativeMemory>
// in OnceLock is only accessed through thread-safe operations.
unsafe impl Send for NativeMemory {}
unsafe impl Sync for NativeMemory {}

/// A resource deferred for destruction after GPU fence completion.
#[derive(Clone)]
struct DeferredResource {
    resource: ID3D12Resource,
    heap: Option<ID3D12Heap>,
    heap_offset: u64,
}

struct NativeMemoryStats {
    host_allocations: AtomicUsize,
    gpu_resources: AtomicUsize,
    upload_bytes: AtomicU64,
    readback_bytes: AtomicU64,
    copy_operations: AtomicU64,
    fence_waits: AtomicU64,
    allocation_failures: AtomicUsize,
}

impl Default for NativeMemoryStats {
    fn default() -> Self {
        Self {
            host_allocations: AtomicUsize::new(0),
            gpu_resources: AtomicUsize::new(0),
            upload_bytes: AtomicU64::new(0),
            readback_bytes: AtomicU64::new(0),
            copy_operations: AtomicU64::new(0),
            fence_waits: AtomicU64::new(0),
            allocation_failures: AtomicUsize::new(0),
        }
    }
}

impl std::fmt::Debug for NativeMemoryStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeMemoryStats")
            .field("host_allocations", &self.host_allocations.load(Ordering::Relaxed))
            .field("gpu_resources", &self.gpu_resources.load(Ordering::Relaxed))
            .field("upload_bytes", &self.upload_bytes.load(Ordering::Relaxed))
            .field("readback_bytes", &self.readback_bytes.load(Ordering::Relaxed))
            .field("copy_operations", &self.copy_operations.load(Ordering::Relaxed))
            .field("fence_waits", &self.fence_waits.load(Ordering::Relaxed))
            .field("allocation_failures", &self.allocation_failures.load(Ordering::Relaxed))
            .finish()
    }
}

impl NativeMemory {
    /// Create a new native memory manager.
    pub fn new(device: Arc<ID3D12Device>, copy_queue: Arc<ID3D12CommandQueue>) -> anyhow::Result<Self> {
        let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory4 = unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory2(Default::default())? };
        let adapter = unsafe { factory.EnumAdapterByLuid(device.GetAdapterLuid())? };
        crate::host::memory_mgr::init_memory_manager(&device, Some(adapter));
        // Create default arenas
        let default_arena = GpuArena::new(
            &device,
            GPU_ARENA_SIZE,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES | D3D12_HEAP_FLAG_SHARED,
        )?;

        let upload_arena = GpuArena::new(
            &device,
            GPU_ARENA_SIZE,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
        )?;

        // Create copy command allocator and list for texture operations
        let copy_allocator: ID3D12CommandAllocator = unsafe {
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?
        };
        let copy_list: ID3D12GraphicsCommandList = unsafe {
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &copy_allocator, None)?
        };
        unsafe { copy_list.Close()? };

        let copy_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
        let copy_fence_event = unsafe { CreateEventW(None, false, false, None)? };

        Ok(NativeMemory {
            device,
            copy_queue,
            copy_allocator: Mutex::new(copy_allocator),
            copy_list: Mutex::new(copy_list),
            copy_fence,
            copy_fence_value: AtomicU64::new(1),
            copy_fence_event,
            allocations: DashMap::new(),
            gpu_bindings: DashMap::new(),
            default_arenas: Mutex::new(vec![Arc::new(default_arena)]),
            upload_arenas: Mutex::new(vec![Arc::new(upload_arena)]),
            upload_ring: OnceLock::new(),
            readback_ring: OnceLock::new(),
            deferred_destruction: Mutex::new(BTreeMap::new()),
            frame: AtomicU64::new(0),
            stats: NativeMemoryStats::default(),
        })
    }

    /// Get or create the upload ring buffer.
    pub fn upload_ring(&self) -> &UploadRing {
        self.upload_ring.get_or_init(|| {
            UploadRing::new(&self.device, UPLOAD_RING_SIZE).expect("Failed to create upload ring")
        })
    }

    /// Get or create the readback ring buffer.
    pub fn readback_ring(&self) -> &ReadbackRing {
        self.readback_ring.get_or_init(|| {
            ReadbackRing::new(&self.device, READBACK_RING_SIZE).expect("Failed to create readback ring")
        })
    }

    /// Allocate title virtual memory (replaces XMemVirtualAlloc).
    /// Returns the base title VA.
    pub fn allocate(&self, title_va: u64, size: u64, protect: u32, placeholder: bool) -> anyhow::Result<u64> {
        anyhow::ensure!(size != 0, "empty allocation");
        let aligned_va = title_va & !(PAGE - 1);
        let size = size.checked_add(title_va - aligned_va)
            .and_then(|n| n.checked_next_multiple_of(PAGE))
            .ok_or_else(|| anyhow::anyhow!("allocation size overflow"))?;
        anyhow::ensure!(aligned_va.checked_add(size).is_some(), "allocation address overflow");

        if placeholder {
            // Reserve address space without committing
            let ptr = unsafe {
                VirtualAlloc(
                    aligned_va as *mut _,
                    size as usize,
                    SYS_MEM_RESERVE,
                    SYS_PAGE_READWRITE,
                )
            };
            if ptr.is_null() {
                self.stats.allocation_failures.fetch_add(1, Ordering::Relaxed);
                return Err(anyhow::anyhow!("VirtualAlloc reserve failed"));
            }

            let allocated_va = ptr as u64;
            let alloc = Arc::new(HostAllocation {
                title_va: allocated_va,
                size,
                protect,
                placeholder: AtomicBool::new(true),
                base_ptr: ptr,
                committed_size: AtomicU64::new(0),
            });
            self.allocations.insert(allocated_va, alloc);
            self.stats.host_allocations.fetch_add(1, Ordering::Relaxed);
            debug!("Reserved title VA {:#x} + {:#x} (placeholder)", aligned_va, size);
            Ok(allocated_va)
        } else {
            // Commit memory
            let ptr = unsafe {
                VirtualAlloc(
                    aligned_va as *mut std::ffi::c_void,
                    size as usize,
                    SYS_MEM_RESERVE | SYS_MEM_COMMIT,
                    protect,
                )
            };
            if ptr.is_null() {
                self.stats.allocation_failures.fetch_add(1, Ordering::Relaxed);
                return Err(anyhow::anyhow!("VirtualAlloc commit failed"));
            }

            let allocated_va = ptr as u64;
            let alloc = Arc::new(HostAllocation {
                title_va: allocated_va,
                size,
                protect,
                placeholder: AtomicBool::new(false),
                base_ptr: ptr,
                committed_size: AtomicU64::new(size),
            });
            self.allocations.insert(allocated_va, alloc);
            self.stats.host_allocations.fetch_add(1, Ordering::Relaxed);
            debug!("Allocated title VA {:#x} + {:#x}", aligned_va, size);
            Ok(allocated_va)
        }
    }

    /// Free title virtual memory (replaces XMemVirtualFree).
    /// Note: GPU resource lifetime is managed by D3D12 reference counting.
    /// Free a title VA and its associated GPU resource.
    /// The resource is queued for deferred destruction to ensure fence-safe lifetime.
    pub fn free(&self, title_va: u64) -> bool {
        // Remove GPU binding and get the resource for deferred destruction
        let resource_info = self.gpu_bindings.remove(&title_va).map(|(_, binding)| {
            let fence_value = binding.fence_value.load(Ordering::Acquire);
            let resource = binding.resource.clone();
            let heap = binding.heap.clone();
            let heap_offset = binding.heap_offset;
            (fence_value, resource, heap, heap_offset)
        });

        if let Some((fence_value, resource, heap, heap_offset)) = resource_info {
            // Untrack from memory manager (Phase 3 Native Graphics).
            crate::host::memory_mgr::untrack_gpu_resource(&resource);

            // Queue for deferred destruction after the fence completes
            if fence_value > 0 {
                self.queue_deferred_destruction(fence_value, resource, heap, heap_offset);
            }
            // If fence_value is 0, the resource was never used for GPU operations
            // D3D12 refcount will handle cleanup when the last Arc is dropped
        }

        let result = self.allocations.remove(&title_va).is_some();
        if result {
            debug!("Freed title VA {:#x}", title_va);
        }
        result
    }

    /// Create a GPU buffer resource bound to title virtual memory.
    /// This is the key replacement for shadow copies - direct binding.
    pub fn create_gpu_resource(
        &self,
        title_va: u64,
        size: u64,
        uav: bool,
    ) -> anyhow::Result<Arc<GpuResourceBinding>> {
        let size = size.next_multiple_of(PAGE);

        // Try to allocate from default arena
        let (resource, gpu_va, offset, heap) = self.allocate_default_resource(size, uav)?;

        let mut binding = GpuResourceBinding::new_buffer(
            self.device.clone(), title_va, size, resource.clone(), gpu_va, uav
        );
        // Store heap info for potential aliasing
        binding.heap_offset = offset;
        binding.heap = heap;
        let binding = Arc::new(binding);

        // Track with memory manager (Phase 6 Hardware Optimization).
        crate::host::memory_mgr::track_gpu_resource(&resource, size, true, crate::host::memory_mgr::MemoryPoolType::Static, crate::host::memory_mgr::ResourcePriority::Normal);

        self.gpu_bindings.insert(title_va, binding.clone());
        self.stats.gpu_resources.fetch_add(1, Ordering::Relaxed);
        debug!("Created GPU buffer at title VA {:#x} + {:#x}, GPU VA {:#x}", title_va, size, gpu_va);
        Ok(binding)
    }

    /// Allocate a buffer resource from the DEFAULT heap arena.
    /// Returns (resource, gpu_va, heap_offset, heap).
    fn allocate_default_resource(&self, size: u64, uav: bool) -> anyhow::Result<(ID3D12Resource, u64, u64, Option<ID3D12Heap>)> {
        let alignment = DEFAULT_ALIGNMENT;
        let flags = if uav {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAGS(0)
        };

        // Try existing arenas
        {
            let arenas = self.default_arenas.lock().unwrap();
            for arena in arenas.iter() {
                if let Some(offset) = arena.allocate(size, alignment) {
                    let desc = D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Alignment: alignment,
                        Width: size,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: DXGI_FORMAT_UNKNOWN,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        Flags: flags,
                    };

                    let mut resource: Option<ID3D12Resource> = None;
                    unsafe { self.device.CreatePlacedResource(arena.heap(), offset, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut resource) }?;
                    let resource = resource.unwrap();

                    // Get GPU VA
                    let gpu_va = unsafe { resource.GetGPUVirtualAddress() };

                    // Track allocation
                    arena.allocations.lock().unwrap().insert(gpu_va, (offset, size));

                    return Ok((resource, gpu_va, offset, Some(arena.heap().clone())));
                }
            }
        }

        // Need new arena
        let new_arena = GpuArena::new(
            &self.device,
            GPU_ARENA_SIZE,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES | D3D12_HEAP_FLAG_SHARED,
        )?;

        let offset = new_arena.allocate(size, alignment).expect("New arena should have space");
        let heap_ref = new_arena.heap().clone();
        let new_arena = Arc::new(new_arena);
        self.default_arenas.lock().unwrap().push(new_arena.clone());

        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: alignment,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
        };

        let mut resource: Option<ID3D12Resource> = None;
        unsafe { self.device.CreatePlacedResource(new_arena.heap(), offset, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut resource) }?;
        let resource = resource.unwrap();

        let gpu_va = unsafe { resource.GetGPUVirtualAddress() };
        new_arena.allocations.lock().unwrap().insert(gpu_va, (offset, size));

        Ok((resource, gpu_va, offset, Some(heap_ref)))
    }

    /// Declare that the GPU will write to a range of a resource.
    /// This is used to track dirty ranges for readback.
    pub fn gpu_writes(&self, title_va: u64, offset: u64, len: u64) -> anyhow::Result<()> {
        let Some(binding) = self.gpu_bindings.get(&title_va) else {
            return Err(anyhow::anyhow!("No GPU resource bound to title VA {:#x}", title_va));
        };

        if offset.checked_add(len).is_none_or(|end| end > binding.size) {
            return Err(anyhow::anyhow!("GPU write exceeds resource bounds"));
        }

        // Mark the resource as having GPU writes
        // In native backend, we track this for readback synchronization
        // The actual data will be read back via readback_from_gpu when needed
        self.stats.copy_operations.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Upload data from CPU to GPU resource (CPU -> VRAM).
    /// Uses persistent upload ring buffer with batch fence synchronization.
    pub fn upload_to_gpu(&self, title_va: u64, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        let Some(binding) = self.gpu_bindings.get(&title_va) else {
            return Err(anyhow::anyhow!("No GPU resource bound to title VA {:#x}", title_va));
        };

        let size = data.len() as u64;
        if offset.checked_add(size).is_none_or(|end| end > binding.size) {
            return Err(anyhow::anyhow!("Upload exceeds resource bounds"));
        }

        // Allocate from upload ring, waiting if necessary
        let ring = self.upload_ring();
        let (ring_offset, ring_ptr) = loop {
            if let Some(alloc) = ring.allocate(size, UPLOAD_ALIGNMENT) {
                break alloc;
            }
            // Ring full, wait for oldest pending allocation to complete
            let fence_value = ring.completed_fence();
            // Wait for the fence to make progress
            let pending = ring.pending.lock().unwrap();
            if let Some(&(_, _, oldest_fence)) = pending.first() {
                if oldest_fence > 0 {
                    drop(pending);
                    ring.wait_fence(oldest_fence);
                    continue;
                }
            }
            drop(pending);
            // If no fence to wait on, yield and retry
            std::thread::yield_now();
        };

        // Copy data to ring buffer
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ring_ptr, size as usize);
        }

        // Record copy command on copy command list
        let copy_list = self.copy_list.lock().unwrap();
        let allocator = self.copy_allocator.lock().unwrap();
        unsafe {
            allocator.Reset()?;
            copy_list.Reset(&*allocator, None)?;
        }
        unsafe {
            binding.activate(&copy_list);
            copy_list.CopyBufferRegion(
                &binding.resource,
                offset,
                &ring.resource,
                ring_offset,
                size,
            );
        }
        unsafe { copy_list.Close()? };

        // Execute and signal fence
        let fence_value = self.execute_copy_list(&copy_list)?;
        binding.fence_value.fetch_max(fence_value, Ordering::Release);

        self.stats.upload_bytes.fetch_add(size, Ordering::Relaxed);
        self.stats.copy_operations.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Upload texture data from CPU to GPU texture resource.
    /// Uses texture footprints for proper mip-level and compressed format handling.
    pub fn upload_texture_to_gpu(&self, va: u64, sub: u32, data: &[u8], row_pitch: u32, slice_pitch: u64) -> anyhow::Result<()> {
        self.texture_transfer(va, sub, Some((data, row_pitch, slice_pitch)))?;
        Ok(())
    }

    pub fn readback_texture(&self, va: u64, sub: u32) -> anyhow::Result<Vec<u8>> {
        self.texture_transfer(va, sub, None)
    }

    fn texture_transfer(&self, va: u64, sub: u32, upload: Option<(&[u8], u32, u64)>) -> anyhow::Result<Vec<u8>> {
        use std::mem::ManuallyDrop;
        let binding = self.gpu_bindings.get(&va).ok_or_else(|| anyhow::anyhow!("missing texture binding"))?;
        let layers = if binding.dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { binding.array_size };
        anyhow::ensure!(binding.is_texture() && sub < u32::from(binding.mip_levels) * u32::from(layers), "invalid texture subresource");
        let (fp, rows, row_bytes, total) = binding.texture_footprint(&self.device, sub);
        let height = rows as usize;
        let depth = fp.Footprint.Depth as usize;
        let width = row_bytes as usize;
        let pitch = fp.Footprint.RowPitch as usize;
        anyhow::ensure!(width > 0 && height > 0 && depth > 0 && total != u64::MAX, "invalid footprint");
        let desc = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: total, Height: 1, DepthOrArraySize: 1, MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ..Default::default() };
        let (heap, state) = if upload.is_some() { (D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ) } else { (D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST) };
        let props = D3D12_HEAP_PROPERTIES { Type: heap, CreationNodeMask: 1, VisibleNodeMask: 1, ..Default::default() };
        let mut staging = None;
        unsafe { self.device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut staging)? };
        let staging: ID3D12Resource = staging.unwrap();
        if let Some((data, row, slice)) = upload {
            let row = if row == 0 { width } else { row as usize };
            let slice = if slice == 0 { row.checked_mul(height) } else { usize::try_from(slice).ok() }.ok_or_else(|| anyhow::anyhow!("slice stride overflow"))?;
            let last = (depth-1).checked_mul(slice).and_then(|v| v.checked_add((height-1).checked_mul(row)?)).and_then(|v|v.checked_add(width));
            anyhow::ensure!(row >= width && last.is_some_and(|n| n <= data.len()), "invalid source pitches");
            let mut ptr = std::ptr::null_mut();
            unsafe { staging.Map(0, Some(&D3D12_RANGE::default()), Some(&mut ptr))?; }
            for z in 0..depth { for y in 0..height {
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(z*slice+y*row), ptr.cast::<u8>().add((z*height+y)*pitch), width); }
            }}
            unsafe { staging.Unmap(0, None); }
        }
        let list = self.copy_list.lock().unwrap();
        let allocator = self.copy_allocator.lock().unwrap();
        unsafe { allocator.Reset()?; list.Reset(&*allocator, None)?; }
        binding.activate(&list);
        let mut tex = D3D12_TEXTURE_COPY_LOCATION { pResource: ManuallyDrop::new(Some(binding.resource.clone())), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub } };
        let mut buf = D3D12_TEXTURE_COPY_LOCATION { pResource: ManuallyDrop::new(Some(staging.clone())), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
        unsafe {
            if upload.is_some() { list.CopyTextureRegion(&tex,0,0,0,&buf,None); } else { list.CopyTextureRegion(&buf,0,0,0,&tex,None); }
            ManuallyDrop::drop(&mut tex.pResource); ManuallyDrop::drop(&mut buf.pResource);
            list.Close()?;
        }
        let fence = self.execute_copy_list(&list)?;
        binding.fence_value.fetch_max(fence, Ordering::Release);
        let mut result = Vec::new();
        if upload.is_none() {
            let mut ptr = std::ptr::null_mut();
            unsafe { staging.Map(0,None,Some(&mut ptr))?; }
            for i in 0..height*depth {
                result.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr.cast::<u8>().add(i*pitch), width) });
            }
            unsafe { staging.Unmap(0,Some(&D3D12_RANGE::default())); }
        }
        Ok(result)
    }

    /// Execute the copy command list and signal fence.
    /// Returns the fence value.
    fn execute_copy_list(&self, list: &ID3D12GraphicsCommandList) -> anyhow::Result<u64> {
        let copy_queue = &self.copy_queue;
        
        unsafe {
            copy_queue.ExecuteCommandLists(&[Some(list.cast()?)]);
        }

        let fence_value = self.copy_fence_value.fetch_add(1, Ordering::AcqRel);
        unsafe { copy_queue.Signal(&self.copy_fence, fence_value) }?;

        // Wait for completion (synchronous for correctness)
        unsafe {
            self.copy_fence.SetEventOnCompletion(fence_value, self.copy_fence_event)?;
            anyhow::ensure!(WaitForSingleObject(self.copy_fence_event, INFINITE) == WAIT_OBJECT_0, "copy fence wait failed");
            anyhow::ensure!(self.copy_fence.GetCompletedValue() != u64::MAX, "device removed during copy");
        }

        Ok(fence_value)
    }

    /// Process deferred resource destruction for completed fences.
    pub fn process_deferred_destruction(&self, completed_fence: u64) {
        let mut deferred = self.deferred_destruction.lock().unwrap();
        let to_destroy: Vec<_> = deferred.range(..=completed_fence).flat_map(|(_, v)| v.iter().cloned()).collect();
        
        for res in to_destroy {
            // Resource will be dropped here, D3D12 will handle cleanup
            // Heap memory becomes available for reuse
            drop(res.resource);
            if let Some(heap) = res.heap {
                drop(heap);
            }
        }
        
        deferred.retain(|&k, _| k > completed_fence);
    }

    /// Queue a resource for deferred destruction after the given fence value.
    pub fn queue_deferred_destruction(&self, fence_value: u64, resource: ID3D12Resource, heap: Option<ID3D12Heap>, heap_offset: u64) {
        let mut deferred = self.deferred_destruction.lock().unwrap();
        deferred.entry(fence_value).or_default().push(DeferredResource {
            resource,
            heap,
            heap_offset,
        });
    }

    /// Upload full texture (all mip levels and array slices) from CPU to GPU.
    /// Data layout: consecutive subresources in D3D12 subresource order.
    pub fn upload_full_texture_to_gpu(
        &self,
        title_va: u64,
        data: &[u8],
        subresource_data: &[D3D12_SUBRESOURCE_DATA],
    ) -> anyhow::Result<()> {
        let Some(binding) = self.gpu_bindings.get(&title_va) else {
            return Err(anyhow::anyhow!("No GPU resource bound to title VA {:#x}", title_va));
        };

        if !binding.is_texture() {
            return Err(anyhow::anyhow!("Resource at {:#x} is not a texture", title_va));
        }

        let subresource_count = binding.mip_levels as u32 * binding.array_size as u32;
        if subresource_data.len() != subresource_count as usize {
            return Err(anyhow::anyhow!(
                "Subresource data count mismatch: expected {}, got {}",
                subresource_count, subresource_data.len()
            ));
        }

        let mut offset = 0;
        for (sub, input) in subresource_data.iter().enumerate() {
            let (fp, rows, width, _) = binding.texture_footprint(&self.device, sub as u32);
            let row = u32::try_from(input.RowPitch)?;
            let slice = u64::try_from(input.SlicePitch)?;
            anyhow::ensure!(row >= width && slice >= u64::from(rows - 1) * u64::from(row) + u64::from(width), "invalid texture strides");
            let size = usize::try_from(slice.checked_mul(u64::from(fp.Footprint.Depth)).ok_or_else(|| anyhow::anyhow!("texture size overflow"))?)?;
            let end = size.checked_add(offset).ok_or_else(|| anyhow::anyhow!("texture offset overflow"))?;
            self.upload_texture_to_gpu(title_va, sub as u32, data.get(offset..end).ok_or_else(|| anyhow::anyhow!("truncated texture"))?, row, slice)?;
            offset = end;
        }
        Ok(())
    }

    /// Read back data from GPU resource to CPU (VRAM -> CPU).
    /// Uses persistent readback ring buffer.
    pub fn readback_from_gpu(&self, title_va: u64, offset: u64, size: u64) -> anyhow::Result<Vec<u8>> {
        let Some(binding) = self.gpu_bindings.get(&title_va) else {
            return Err(anyhow::anyhow!("No GPU resource bound to title VA {:#x}", title_va));
        };

        if offset.checked_add(size).is_none_or(|end| end > binding.size) {
            return Err(anyhow::anyhow!("Readback exceeds resource bounds"));
        }

        // Allocate from readback ring, waiting if necessary
        let ring = self.readback_ring();
        let (ring_offset, ring_ptr) = loop {
            if let Some(alloc) = ring.allocate(size, UPLOAD_ALIGNMENT) {
                break alloc;
            }
            let pending = ring.pending.lock().unwrap();
            if let Some(&(_, _, oldest_fence)) = pending.first() {
                if oldest_fence > 0 {
                    drop(pending);
                    ring.wait_fence(oldest_fence);
                    continue;
                }
            }
            drop(pending);
            std::thread::yield_now();
        };

        // Record copy command on copy command list
        let copy_list = self.copy_list.lock().unwrap();
        let allocator = self.copy_allocator.lock().unwrap();
        unsafe {
            allocator.Reset()?;
            copy_list.Reset(&*allocator, None)?;
        }
        unsafe {
            binding.activate(&copy_list);
            copy_list.CopyBufferRegion(
                &ring.resource,
                ring_offset,
                &binding.resource,
                offset,
                size,
            );
        }
        unsafe { copy_list.Close()? };

        // Execute and signal fence
        let fence_value = self.execute_copy_list(&copy_list)?;
        binding.fence_value.fetch_max(fence_value, Ordering::Release);

        // Copy data from ring buffer
        let mut data = vec![0u8; size as usize];
        unsafe {
            std::ptr::copy_nonoverlapping(ring_ptr, data.as_mut_ptr(), size as usize);
        }

        self.stats.readback_bytes.fetch_add(size, Ordering::Relaxed);
        self.stats.copy_operations.fetch_add(1, Ordering::Relaxed);
        self.stats.fence_waits.fetch_add(1, Ordering::Relaxed);

        Ok(data)
    }

    /// Map a GPU resource for CPU access (direct map, no copy).
    /// Only works for UPLOAD heap resources or placed resources in upload heaps.
    pub fn map_gpu_resource(&self, title_va: u64) -> anyhow::Result<*mut u8> {
        let Some(binding) = self.gpu_bindings.get(&title_va) else {
            return Err(anyhow::anyhow!("No GPU resource bound to title VA {:#x}", title_va));
        };

        let mut ptr: *mut u8 = std::ptr::null_mut();
        unsafe {
            binding.resource.Map(0, None, Some(&mut ptr as *mut _ as *mut _))?;
        }

        Ok(ptr)
    }

    /// Unmap a GPU resource.
    pub fn unmap_gpu_resource(&self, title_va: u64) -> anyhow::Result<()> {
        let Some(binding) = self.gpu_bindings.get(&title_va) else {
            return Err(anyhow::anyhow!("No GPU resource bound to title VA {:#x}", title_va));
        };

        unsafe {
            binding.resource.Unmap(0, None);
        }

        Ok(())
    }

    /// Get the GPU virtual address for a title VA.
    /// This replaces the directory lookup + shadow copy indirection.
    pub fn get_gpu_va(&self, title_va: u64) -> Option<u64> {
        let binding = self.binding_containing(title_va)?;
        (binding.dimension == D3D12_RESOURCE_DIMENSION_BUFFER)
            .then(|| binding.gpu_va.checked_add(title_va - binding.title_va)).flatten()
    }

    fn binding_containing(&self, title_va: u64) -> Option<Arc<GpuResourceBinding>> {
        if let Some(binding) = self.gpu_bindings.get(&title_va) {
            return Some(binding.clone());
        }
        self.gpu_bindings.iter()
            .filter(|b| title_va.checked_sub(b.title_va).is_some_and(|offset| offset < b.size))
            .max_by_key(|b| b.title_va)
            .map(|b| b.value().clone())
    }

    /// Check if a title VA has a GPU resource bound.
    pub fn has_gpu_binding(&self, title_va: u64) -> bool {
        self.binding_containing(title_va).is_some()
    }

    /// Get the host allocation for a title VA.
    pub fn get_allocation(&self, title_va: u64) -> Option<Arc<HostAllocation>> {
        self.allocations.get(&title_va).map(|a| a.clone())
    }

    /// Find a committed allocation covering the given address range.
    fn find_committed_allocation(&self, address: usize, len: usize) -> Option<Arc<HostAllocation>> {
        self.allocations.iter().find(|a| {
            let a = a.value();
            (address as u64).checked_sub(a.title_va).is_some_and(|off| {
                off.checked_add(len as u64).is_some_and(|end| end <= a.committed_size())
            })
        }).map(|a| a.value().clone())
    }

    /// Ensure a committed allocation exists for the given address range.
    /// Commits an existing placeholder allocation or creates a new one.
    fn ensure_committed_allocation(&self, address: usize, len: usize) -> anyhow::Result<Arc<HostAllocation>> {
        let address_u64 = address as u64;
        let len_u64 = len as u64;

        // Align to page boundaries
        let aligned_va = address_u64 & !(PAGE - 1);
        let end_va = address_u64.checked_add(len_u64).ok_or_else(|| anyhow::anyhow!("address range overflow"))?;
        let aligned_end = end_va.next_multiple_of(PAGE);
        let size = aligned_end.checked_sub(aligned_va).ok_or_else(|| anyhow::anyhow!("size overflow"))?;

        // First, check if there's a placeholder allocation covering this range
        if let Some(placeholder) = self.allocations.iter().find(|a| {
            let a = a.value();
            a.is_placeholder()
                && aligned_va >= a.title_va
                && aligned_end <= a.title_va + a.size
        }) {
            let placeholder = placeholder.value().clone();
            // Commit the placeholder allocation
            return self.commit_placeholder(placeholder, aligned_va, size);
        }

        // No existing allocation (placeholder or committed) covers this range.
        // Create a new committed allocation.
        self.allocate_committed(aligned_va, size, PAGE_READWRITE)
    }

    /// Commit a placeholder allocation for the given range.
    fn commit_placeholder(
        &self,
        placeholder: Arc<HostAllocation>,
        aligned_va: u64,
        size: u64,
    ) -> anyhow::Result<Arc<HostAllocation>> {
        // The placeholder covers a larger range. We need to commit the requested sub-range.
        // For simplicity, commit the entire placeholder range.
        let ptr = unsafe {
            windows_sys::Win32::System::Memory::VirtualAlloc(
                placeholder.base_ptr,
                placeholder.size as usize,
                SYS_MEM_COMMIT,
                placeholder.protect,
            )
        };
        if ptr.is_null() {
            return Err(anyhow::anyhow!("VirtualAlloc commit failed for placeholder"));
        }

        // Update the existing allocation in place (atomic fields)
        placeholder.set_committed(placeholder.size);
        debug!("Committed placeholder title VA {:#x} + {:#x}", placeholder.title_va, placeholder.size);
        Ok(placeholder)
    }

    /// Allocate a new committed host allocation.
    fn allocate_committed(&self, aligned_va: u64, size: u64, protect: u32) -> anyhow::Result<Arc<HostAllocation>> {
        // Try to allocate
        let ptr = unsafe {
            windows_sys::Win32::System::Memory::VirtualAlloc(
                aligned_va as *mut std::ffi::c_void,
                size as usize,
                SYS_MEM_RESERVE | SYS_MEM_COMMIT,
                protect,
            )
        };
        if ptr.is_null() {
            // Allocation failed. Check if another thread already allocated this range.
            let error = std::io::Error::last_os_error();
            // If the address is already allocated, try to get the existing allocation.
            if error.raw_os_error() == Some(487) || error.raw_os_error() == Some(183) {
                // ERROR_INVALID_ADDRESS (487) or ERROR_ALREADY_EXISTS (183)
                if let Some(existing) = self.allocations.get(&aligned_va) {
                    if !existing.is_placeholder() && existing.committed_size() >= size {
                        return Ok(existing.clone());
                    }
                }
            }
            self.stats.allocation_failures.fetch_add(1, Ordering::Relaxed);
            return Err(anyhow::anyhow!("VirtualAlloc commit failed: {}", error));
        }

        let alloc = Arc::new(HostAllocation {
            title_va: aligned_va,
            size,
            protect,
            placeholder: AtomicBool::new(false),
            base_ptr: ptr,
            committed_size: AtomicU64::new(size),
        });
        self.allocations.insert(aligned_va, alloc.clone());
        self.stats.host_allocations.fetch_add(1, Ordering::Relaxed);
        debug!("Allocated committed title VA {:#x} + {:#x}", aligned_va, size);
        Ok(alloc)
    }

    /// Advance frame counter and report stats.
    /// Also processes deferred resource destruction for completed fences.
    pub fn next_frame(&self) {
        let frame = self.frame.fetch_add(1, Ordering::Relaxed);
        
        // Process deferred resource destruction
        let completed_fence = unsafe { self.copy_fence.GetCompletedValue() };
        self.process_deferred_destruction(completed_fence);
        
        // Also process upload/readback ring fence completion for buffer uploads
        let upload_completed = self.upload_ring().completed_fence();
        let readback_completed = self.readback_ring().completed_fence();
        
        if frame % 60 == 0 {
            self.report_stats(frame);
        }
    }

    /// Create a GPU texture resource bound to title virtual memory.
    /// Supports swizzle, mipmaps, compressed formats, and texture arrays.
    pub fn create_gpu_texture(
        &self,
        title_va: u64,
        desc: &D3D12_RESOURCE_DESC,
        uav: bool,
    ) -> anyhow::Result<Arc<GpuResourceBinding>> {
        // Calculate total size for all mip levels
        let size = self.calculate_texture_size(desc);
        
        // Allocate from default arena
        let (resource, gpu_va, offset, heap) = self.allocate_default_texture_resource(desc, uav)?;

        let binding = Arc::new(GpuResourceBinding::new_texture(
            self.device.clone(), title_va, size, resource.clone(), gpu_va, uav, desc, offset, heap
        ));

        // Track with memory manager (Phase 6 Hardware Optimization).
        crate::host::memory_mgr::track_gpu_resource(&resource, size, true, crate::host::memory_mgr::MemoryPoolType::Static, crate::host::memory_mgr::ResourcePriority::Normal);

        self.gpu_bindings.insert(title_va, binding.clone());
        self.stats.gpu_resources.fetch_add(1, Ordering::Relaxed);
        debug!(
            "Created GPU texture at title VA {:#x} + {:#x}, GPU VA {:#x}, format={:?}, mips={}, array={}, dim={:?}",
            title_va, size, gpu_va, desc.Format, desc.MipLevels, desc.DepthOrArraySize, desc.Dimension
        );
        Ok(binding)
    }

    /// Calculate total texture size including all mip levels.
    fn calculate_texture_size(&self, desc: &D3D12_RESOURCE_DESC) -> u64 {
        let mut total = 0u64;
        let mut width = desc.Width;
        let mut height = desc.Height.max(1);
        let mut depth = desc.DepthOrArraySize.max(1);
        
        for _ in 0..desc.MipLevels {
            // Use GetCopyableFootprints for accurate size calculation
            // For now, estimate based on format
            let block_size = self.format_block_size(desc.Format);
            let blocks_x = ((width + 3) / 4) as u64;
            let blocks_y = ((height + 3) / 4) as u64;
            let depth = depth as u64;
            total += blocks_x * blocks_y * depth * block_size;
            
            width = (width / 2).max(1);
            height = (height / 2).max(1);
        }
        
        total.max(PAGE).next_multiple_of(PAGE)
    }

    /// Get block size in bytes for a compressed format.
    fn format_block_size(&self, format: DXGI_FORMAT) -> u64 {
        match format {
            // BC1 (DXT1): 8 bytes per 4x4 block
            DXGI_FORMAT_BC1_UNORM | DXGI_FORMAT_BC1_UNORM_SRGB | DXGI_FORMAT_BC1_TYPELESS => 8,
            // BC2 (DXT3): 16 bytes per 4x4 block
            DXGI_FORMAT_BC2_UNORM | DXGI_FORMAT_BC2_UNORM_SRGB | DXGI_FORMAT_BC2_TYPELESS => 16,
            // BC3 (DXT5): 16 bytes per 4x4 block
            DXGI_FORMAT_BC3_UNORM | DXGI_FORMAT_BC3_UNORM_SRGB | DXGI_FORMAT_BC3_TYPELESS => 16,
            // BC4: 8 bytes per 4x4 block
            DXGI_FORMAT_BC4_UNORM | DXGI_FORMAT_BC4_SNORM | DXGI_FORMAT_BC4_TYPELESS => 8,
            // BC5: 16 bytes per 4x4 block
            DXGI_FORMAT_BC5_UNORM | DXGI_FORMAT_BC5_SNORM | DXGI_FORMAT_BC5_TYPELESS => 16,
            // BC6H: 16 bytes per 4x4 block
            DXGI_FORMAT_BC6H_UF16 | DXGI_FORMAT_BC6H_SF16 | DXGI_FORMAT_BC6H_TYPELESS => 16,
            // BC7: 16 bytes per 4x4 block
            DXGI_FORMAT_BC7_UNORM | DXGI_FORMAT_BC7_UNORM_SRGB | DXGI_FORMAT_BC7_TYPELESS => 16,
            // Standard formats: estimate 4 bytes per pixel (RGBA8)
            _ => 4,
        }
    }

    /// Allocate a texture resource from the DEFAULT heap arena.
    /// Returns (resource, gpu_va, heap_offset, heap).
    fn allocate_default_texture_resource(
        &self,
        desc: &D3D12_RESOURCE_DESC,
        uav: bool,
    ) -> anyhow::Result<(ID3D12Resource, u64, u64, Option<ID3D12Heap>)> {
        let mut tex_desc = *desc;
        if uav { tex_desc.Flags |= D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS; }
        let info = unsafe { self.device.GetResourceAllocationInfo(0, &[tex_desc]) };
        anyhow::ensure!(info.SizeInBytes != u64::MAX && info.Alignment != 0, "invalid texture description");
        let size = info.SizeInBytes;
        let alignment = info.Alignment;
        // Try existing arenas
        {
            let arenas = self.default_arenas.lock().unwrap();
            for arena in arenas.iter() {
                if let Some(offset) = arena.allocate(size, alignment) {

                    let mut resource: Option<ID3D12Resource> = None;
                    unsafe { self.device.CreatePlacedResource(arena.heap(), offset, &tex_desc, D3D12_RESOURCE_STATE_COMMON, None, &mut resource) }?;
                    let resource = resource.unwrap();

                    // Get GPU VA
                    let gpu_va = unsafe { resource.GetGPUVirtualAddress() };

                    // Track allocation
                    arena.allocations.lock().unwrap().insert(resource.as_raw() as u64, (offset, size));

                    return Ok((resource, gpu_va, offset, Some(arena.heap().clone())));
                }
            }
        }

        // Need new arena
        let new_arena = GpuArena::new(
            &self.device,
            GPU_ARENA_SIZE.max(size),
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES | D3D12_HEAP_FLAG_SHARED,
        )?;

        let offset = new_arena.allocate(size, alignment).expect("New arena should have space");
        let heap_ref = new_arena.heap().clone();
        let new_arena = Arc::new(new_arena);
        self.default_arenas.lock().unwrap().push(new_arena.clone());

        let mut resource: Option<ID3D12Resource> = None;
        unsafe { self.device.CreatePlacedResource(new_arena.heap(), offset, &tex_desc, D3D12_RESOURCE_STATE_COMMON, None, &mut resource) }?;
        let resource = resource.unwrap();

        let gpu_va = unsafe { resource.GetGPUVirtualAddress() };
        new_arena.allocations.lock().unwrap().insert(resource.as_raw() as u64, (offset, size));

        Ok((resource, gpu_va, offset, Some(heap_ref)))
    }

    /// Check if a format is a compressed (block-based) format.
    fn is_compressed_format(&self, format: DXGI_FORMAT) -> bool {
        matches!(
            format,
            DXGI_FORMAT_BC1_UNORM | DXGI_FORMAT_BC1_UNORM_SRGB | DXGI_FORMAT_BC1_TYPELESS
                | DXGI_FORMAT_BC2_UNORM | DXGI_FORMAT_BC2_UNORM_SRGB | DXGI_FORMAT_BC2_TYPELESS
                | DXGI_FORMAT_BC3_UNORM | DXGI_FORMAT_BC3_UNORM_SRGB | DXGI_FORMAT_BC3_TYPELESS
                | DXGI_FORMAT_BC4_UNORM | DXGI_FORMAT_BC4_SNORM | DXGI_FORMAT_BC4_TYPELESS
                | DXGI_FORMAT_BC5_UNORM | DXGI_FORMAT_BC5_SNORM | DXGI_FORMAT_BC5_TYPELESS
                | DXGI_FORMAT_BC6H_UF16 | DXGI_FORMAT_BC6H_SF16 | DXGI_FORMAT_BC6H_TYPELESS
                | DXGI_FORMAT_BC7_UNORM | DXGI_FORMAT_BC7_UNORM_SRGB | DXGI_FORMAT_BC7_TYPELESS
        )
    }

    /// Create an aliased GPU resource at the same heap location as an existing resource.
    /// This implements XMemPhysicalPages aliasing - multiple title VAs pointing to the same physical memory.
    /// Returns a new GpuResourceBinding that shares the same heap memory.
    pub fn create_aliased_resource(
        &self,
        title_va: u64,
        source_title_va: u64,
        size: u64,
        uav: bool,
    ) -> anyhow::Result<Arc<GpuResourceBinding>> {
        let size = size.next_multiple_of(PAGE);

        // Get the source binding to find its heap location
        let source_binding = self.gpu_bindings.get(&source_title_va).map(|entry| entry.value().clone())
            .ok_or_else(|| anyhow::anyhow!("No source GPU resource at title VA {:#x}", source_title_va))?;

        let heap = source_binding.heap.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Source resource has no heap reference for aliasing"))?;
        let heap_offset = source_binding.heap_offset;

        // Create a new placed resource at the same heap offset
        let alignment = DEFAULT_ALIGNMENT;
        let flags = if uav {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAGS(0)
        };

        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: alignment,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
        };

        let mut resource: Option<ID3D12Resource> = None;
        unsafe { self.device.CreatePlacedResource(heap, heap_offset, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut resource) }?;
        let resource = resource.unwrap();

        let gpu_va = unsafe { resource.GetGPUVirtualAddress() };

        let mut binding = GpuResourceBinding::new_buffer(
            self.device.clone(), title_va, size, resource.clone(), gpu_va, uav
        );
        binding.heap_offset = heap_offset;
        binding.heap = Some(heap.clone());
        binding.active_alias = source_binding.active_alias.clone();
        let binding = Arc::new(binding);

        // Register as alias on the source binding
        source_binding.add_alias(resource);

        self.gpu_bindings.insert(title_va, binding.clone());
        self.stats.gpu_resources.fetch_add(1, Ordering::Relaxed);
        debug!("Created aliased GPU resource at title VA {:#x} -> {:#x} (same heap offset {:#x})", title_va, source_title_va, heap_offset);
        Ok(binding)
    }

    /// Create an aliased texture resource at the same heap location.
    pub fn create_aliased_texture(
        &self,
        title_va: u64,
        source_title_va: u64,
        desc: &D3D12_RESOURCE_DESC,
        uav: bool,
    ) -> anyhow::Result<Arc<GpuResourceBinding>> {
        let source_binding = self.gpu_bindings.get(&source_title_va).map(|entry| entry.value().clone())
            .ok_or_else(|| anyhow::anyhow!("No source GPU texture at title VA {:#x}", source_title_va))?;

        let heap = source_binding.heap.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Source texture has no heap reference for aliasing"))?;
        let heap_offset = source_binding.heap_offset;

        let mut tex_desc = *desc;
        if uav { tex_desc.Flags |= D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS; }
        let info = unsafe { self.device.GetResourceAllocationInfo(0, &[tex_desc]) };
        let source = unsafe { self.device.GetResourceAllocationInfo(0, &[source_binding.resource.GetDesc()]) };
        anyhow::ensure!(info.SizeInBytes <= source.SizeInBytes && info.Alignment != 0 && heap_offset.is_multiple_of(info.Alignment), "alias exceeds source placement");
        let size = info.SizeInBytes;
        let mut resource: Option<ID3D12Resource> = None;
        unsafe { self.device.CreatePlacedResource(heap, heap_offset, &tex_desc, D3D12_RESOURCE_STATE_COMMON, None, &mut resource) }?;
        let resource = resource.unwrap();

        let gpu_va = unsafe { resource.GetGPUVirtualAddress() };

        let mut binding = GpuResourceBinding::new_texture(
            self.device.clone(), title_va, size, resource.clone(), gpu_va, uav, desc, heap_offset, Some(heap.clone())
        );
        binding.active_alias = source_binding.active_alias.clone();
        let binding = Arc::new(binding);

        // Register as alias on the source binding
        source_binding.add_alias(resource);

        self.gpu_bindings.insert(title_va, binding.clone());
        self.stats.gpu_resources.fetch_add(1, Ordering::Relaxed);
        debug!("Created aliased GPU texture at title VA {:#x} -> {:#x} (same heap offset {:#x})", title_va, source_title_va, heap_offset);
        Ok(binding)
    }

    /// Create a bottom-level acceleration structure (BLAS) for ray tracing.
    /// The title provides the build inputs at title_va.
    pub fn create_blas(
        &self,
        title_va: u64,
        size: u64,
        build_inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    ) -> anyhow::Result<Arc<GpuResourceBinding>> {
        let size = size.next_multiple_of(PAGE);

        // Allocate buffer for the acceleration structure
        let (resource, gpu_va, offset, heap) = self.allocate_default_resource(size, false)?;

        let mut binding = GpuResourceBinding::new_buffer(
            self.device.clone(), title_va, size, resource.clone(), gpu_va, false
        );
        binding.heap_offset = offset;
        binding.heap = heap;
        let binding = Arc::new(binding);

        // The actual BLAS build is done via command list, not here
        // This just allocates the backing buffer

        self.gpu_bindings.insert(title_va, binding.clone());
        self.stats.gpu_resources.fetch_add(1, Ordering::Relaxed);
        debug!("Created BLAS buffer at title VA {:#x} + {:#x}, GPU VA {:#x}", title_va, size, gpu_va);
        Ok(binding)
    }

    /// Create a top-level acceleration structure (TLAS) for ray tracing.
    pub fn create_tlas(
        &self,
        title_va: u64,
        size: u64,
    ) -> anyhow::Result<Arc<GpuResourceBinding>> {
        let size = size.next_multiple_of(PAGE);

        // Allocate buffer for the acceleration structure
        let (resource, gpu_va, offset, heap) = self.allocate_default_resource(size, false)?;

        let mut binding = GpuResourceBinding::new_buffer(
            self.device.clone(), title_va, size, resource.clone(), gpu_va, false
        );
        binding.heap_offset = offset;
        binding.heap = heap;
        let binding = Arc::new(binding);

        self.gpu_bindings.insert(title_va, binding.clone());
        self.stats.gpu_resources.fetch_add(1, Ordering::Relaxed);
        debug!("Created TLAS buffer at title VA {:#x} + {:#x}, GPU VA {:#x}", title_va, size, gpu_va);
        Ok(binding)
    }

    /// Get the acceleration structure prebuild info (required sizes).
    pub fn get_acceleration_structure_prebuild_info(
        &self,
        build_inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    ) -> D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO {
        let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        unsafe {
            if let Ok(device5) = self.device.cast::<ID3D12Device5>() {
                device5.GetRaytracingAccelerationStructurePrebuildInfo(build_inputs, &mut info);
            }
        }
        info
    }

    fn report_stats(&self, frame: u64) {
        info!(
            frame,
            host_allocations = self.stats.host_allocations.load(Ordering::Relaxed),
            gpu_resources = self.stats.gpu_resources.load(Ordering::Relaxed),
            upload_mb = self.stats.upload_bytes.load(Ordering::Relaxed) as f64 / 1024.0 / 1024.0,
            readback_mb = self.stats.readback_bytes.load(Ordering::Relaxed) as f64 / 1024.0 / 1024.0,
            copy_ops = self.stats.copy_operations.load(Ordering::Relaxed),
            fence_waits = self.stats.fence_waits.load(Ordering::Relaxed),
            "native memory stats"
        );
    }
}

/// Global native memory instance (optional, for incremental adoption).
static NATIVE_MEMORY: OnceLock<Arc<NativeMemory>> = OnceLock::new();

/// Initialize the native memory backend.
pub fn init_native_memory(device: Arc<ID3D12Device>, copy_queue: Arc<ID3D12CommandQueue>) -> anyhow::Result<Arc<NativeMemory>> {
    let mem = Arc::new(NativeMemory::new(device, copy_queue)?);
    NATIVE_MEMORY.set(mem.clone()).map_err(|_| anyhow::anyhow!("Native memory already initialized"))?;
    Ok(mem)
}

/// Get the native memory instance.
pub fn native_memory() -> Option<Arc<NativeMemory>> {
    NATIVE_MEMORY.get().cloned()
}

/// Check if native memory backend is active.
pub fn is_native_memory_active() -> bool {
    NATIVE_MEMORY.get().is_some()
}

/// A native backend shadow handle that mimics the Shadow API for the native path.
/// Used as a drop-in replacement for Arc<Shadow> when native backend is active.
pub struct NativeShadow {
    /// The native memory backend.
    backend: Arc<NativeMemory>,
    /// Title virtual address range.
    title_va: u64,
    /// Size in bytes.
    len: u64,
    /// GPU virtual address of the resource.
    gpu_va: u64,
    /// The D3D12 resource (for descriptor creation).
    resource: ID3D12Resource,
    /// Whether this is a UAV resource.
    uav: bool,
}

impl NativeShadow {
    /// Create a new NativeShadow for a title VA range.
    pub fn new(backend: Arc<NativeMemory>, title_va: u64, len: u64, uav: bool) -> anyhow::Result<Arc<Self>> {
        // Ensure GPU resource exists
        let binding = backend.create_gpu_resource(title_va, len, uav)?;
        Ok(Arc::new(NativeShadow {
            backend,
            title_va,
            len,
            gpu_va: binding.gpu_va,
            resource: binding.resource.clone(),
            uav,
        }))
    }

    /// Get the underlying D3D12 resource for descriptor creation.
    pub fn resource(&self) -> &ID3D12Resource {
        &self.resource
    }

    /// The host GPU address of a title address inside the buffer.
    pub fn gpu(&self, address: u64) -> u64 {
        self.gpu_va + (address - self.title_va)
    }

    /// The title memory the copy is of: its start and length.
    pub fn span(&self) -> (u64, u64) {
        (self.title_va, self.len)
    }

    /// The offset of a title address in the copy, if it lies in it.
    pub fn offset_in(&self, address: u64) -> Option<u64> {
        address.checked_sub(self.title_va).filter(|&at| at < self.len)
    }

    /// Bytes from a title address to the buffer's end.
    pub fn left(&self, address: u64) -> u64 {
        (self.title_va + self.len).saturating_sub(address)
    }

    fn host_range(&self, address: usize, len: usize) -> (Arc<HostAllocation>, usize, usize) {
        // First try to find an existing committed allocation covering the range.
        if let Some(allocation) = self.backend.find_committed_allocation(address, len) {
            let offset = address - allocation.title_va as usize;
            let resource_offset = address.checked_sub(self.title_va as usize).expect("address precedes binding");
            assert!(resource_offset.checked_add(len).is_some_and(|end| end <= self.len as usize));
            return (allocation, offset, resource_offset);
        }

        // No committed allocation found. Ensure one exists (commits placeholders or creates new).
        let allocation = self.backend.ensure_committed_allocation(address, len)
            .expect("failed to ensure committed host allocation for native shadow range");
        let offset = address - allocation.title_va as usize;
        let resource_offset = address.checked_sub(self.title_va as usize).expect("address precedes binding");
        assert!(resource_offset.checked_add(len).is_some_and(|end| end <= self.len as usize));
        (allocation, offset, resource_offset)
    }

    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        let (allocation, offset, resource_offset) = self.host_range(address, len);
        let data = unsafe { std::slice::from_raw_parts(allocation.base_ptr.cast::<u8>().add(offset), len) };
        self.backend.upload_to_gpu(self.title_va, resource_offset as u64, data).expect("native shadow upload");
        len
    }

    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        let (allocation, offset, resource_offset) = self.host_range(address, len);
        let bytes = self.backend.readback_from_gpu(self.title_va, resource_offset as u64, len as u64).expect("native shadow readback");
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), allocation.base_ptr.cast::<u8>().add(offset), len); }
        len
    }

    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        list.defer_refresh(&crate::shadow::ShadowHandle::Native(self.clone()), address as usize, len.min(self.left(address)) as usize);
    }

    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        list.defer_gpu_write(&crate::shadow::ShadowHandle::Native(self.clone()), address as usize, len.min(self.left(address)) as usize);
    }

    /// Whether the memory behind the copy turned out not to be write-watched.
    pub fn unwatched(&self) -> bool {
        false // Native backend doesn't use write watching
    }

    /// The bytes of the host buffer (resource width).
    pub fn width(&self) -> u64 {
        unsafe { self.resource.GetDesc() }.Width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_alignment() {
        assert_eq!(PAGE, 65536);
        assert!(PAGE.is_power_of_two());
    }

    #[test]
    fn test_max_title_pages() {
        assert_eq!(MAX_TITLE_PAGES, 1 << 26); // 4 GiB / 64 KiB
    }
}