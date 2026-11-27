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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use dashmap::DashMap;
use tracing::{debug, info, warn};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
use windows::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RESERVE, MEM_RELEASE, PAGE_READWRITE};

use crate::host::{host, Host};
use crate::resource::{footprint, AllocationInfo};

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
const UPLOAD_ALIGNMENT: u64 = 64 * 1024;

/// Default heap alignment (64 KiB).
const DEFAULT_ALIGNMENT: u64 = 64 * 1024;

/// A host allocation backing title virtual memory.
#[derive(Debug)]
struct HostAllocation {
    /// Base address in title virtual address space.
    title_va: u64,
    /// Size in bytes.
    size: u64,
    /// Protection flags (PAGE_READWRITE, etc.).
    protect: u32,
    /// Whether this is a placeholder (reserved but not committed).
    placeholder: bool,
    /// Underlying Windows allocation (for VirtualFree on drop).
    base_ptr: *mut std::ffi::c_void,
    /// Actual committed size.
    committed_size: u64,
}

impl Drop for HostAllocation {
    fn drop(&mut self) {
        if !self.base_ptr.is_null() {
            // SAFETY: We allocated this with VirtualAlloc.
            unsafe {
                VirtualFree(self.base_ptr, 0, MEM_RELEASE);
            }
        }
    }
}

/// A GPU resource bound to a title virtual address range.
#[derive(Debug, Clone)]
struct GpuResourceBinding {
    /// Title virtual address this resource is bound to.
    title_va: u64,
    /// Size in bytes.
    size: u64,
    /// The D3D12 resource (DEFAULT heap for GPU access).
    resource: ID3D12Resource,
    /// GPU virtual address of the resource.
    gpu_va: u64,
    /// Whether this resource can be written by shaders (UAV).
    uav: bool,
    /// Optional upload heap staging resource for CPU->GPU copies.
    upload_staging: Option<ID3D12Resource>,
    /// Optional readback heap staging resource for GPU->CPU copies.
    readback_staging: Option<ID3D12Resource>,
    /// Fence value for tracking GPU completion on this resource.
    fence_value: u64,
}

/// GPU heap arena for sub-allocating resources.
#[derive(Debug)]
struct GpuArena {
    /// The D3D12 heap.
    heap: ID3D12Heap,
    /// Base GPU virtual address.
    base_gpu_va: u64,
    /// Total size.
    size: u64,
    /// Free list: (offset, size) pairs.
    free_list: Mutex<Vec<(u64, u64)>>,
    /// Allocated resources in this arena (for tracking).
    allocations: Mutex<HashMap<u64, (u64, u64)>>, // gpu_va -> (offset, size)
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
            Alignment: D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT,
            Flags: flags,
        };

        let heap: ID3D12Device = unsafe { std::mem::transmute(device.clone()) };
        let heap = unsafe { device.CreateHeap(&desc)? };

        // Get the base GPU VA (heaps don't have a direct GPU VA, but placed resources do)
        // We'll assign GPUVAs when placing resources.

        Ok(GpuArena {
            heap,
            base_gpu_va: 0, // Will be set when resources are placed
            size,
            free_list: Mutex::new(vec![(0, size)]),
            allocations: Mutex::new(HashMap::new()),
        })
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
        let mut merged = Vec::new();
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
#[derive(Debug)]
struct UploadRing {
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
    fence_event: std::ptr::NullMut,
}

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

        let resource: ID3D12Resource = unsafe { device.CreateCommittedResource(&heap_props, D3D12_HEAP_FLAGS(0), &desc, D3D12_RESOURCE_STATE_GENERIC_READ, None)? };

        // Map the resource for CPU writes
        let mut mapped_ptr: *mut u8 = std::ptr::null_mut();
        unsafe {
            resource.Map(0, None, Some(&mut mapped_ptr as *mut _ as *mut _))?;
        }

        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAGS(0))? };
        let fence_event = unsafe { windows::Win32::Foundation::CreateEventW(None, false, false, None) };

        Ok(UploadRing {
            resource,
            mapped_ptr,
            size,
            write_offset: AtomicU64::new(0),
            fence,
            fence_value: AtomicU64::new(1),
            fence_event,
        })
    }

    /// Allocate space in the ring buffer for an upload.
    /// Returns (offset, mapped_ptr) or None if full.
    fn allocate(&self, size: u64, alignment: u64) -> Option<(u64, *mut u8)> {
        let size = size.next_multiple_of(alignment);
        let current = self.write_offset.load(Ordering::Acquire);
        let next = current + size;
        if next > self.size {
            // Ring buffer full - would need to wait for fence
            return None;
        }
        if self.write_offset.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let ptr = unsafe { self.mapped_ptr.add(current as usize) };
            Some((current, ptr))
        } else {
            None
        }
    }

    /// Signal the fence after recording copy commands.
    fn signal_fence(&self, queue: &ID3D12CommandQueue) -> u64 {
        let value = self.fence_value.fetch_add(1, Ordering::AcqRel);
        unsafe { queue.Signal(&self.fence, value) }.ok();
        value
    }

    /// Wait for a fence value (blocking).
    fn wait_fence(&self, value: u64) {
        unsafe { self.fence.SetEventOnCompletion(value, self.fence_event) }.ok();
        unsafe { windows::Win32::Foundation::WaitForSingleObject(self.fence_event, u32::MAX) };
    }
}

/// Persistent ring buffer for readbacks (GPU -> CPU).
#[derive(Debug)]
struct ReadbackRing {
    resource: ID3D12Resource,
    mapped_ptr: *mut u8,
    size: u64,
    read_offset: AtomicU64,
    fence: ID3D12Fence,
    fence_value: AtomicU64,
    fence_event: std::ptr::NullMut,
}

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

        let resource: ID3D12Resource = unsafe { device.CreateCommittedResource(&heap_props, D3D12_HEAP_FLAGS(0), &desc, D3D12_RESOURCE_STATE_COPY_DEST, None)? };

        let mut mapped_ptr: *mut u8 = std::ptr::null_mut();
        unsafe {
            resource.Map(0, None, Some(&mut mapped_ptr as *mut _ as *mut _))?;
        }

        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAGS(0))? };
        let fence_event = unsafe { windows::Win32::Foundation::CreateEventW(None, false, false, None) };

        Ok(ReadbackRing {
            resource,
            mapped_ptr,
            size,
            read_offset: AtomicU64::new(0),
            fence,
            fence_value: AtomicU64::new(1),
            fence_event,
        })
    }

    fn allocate(&self, size: u64, alignment: u64) -> Option<(u64, *mut u8)> {
        let size = size.next_multiple_of(alignment);
        let current = self.read_offset.load(Ordering::Acquire);
        let next = current + size;
        if next > self.size {
            return None;
        }
        if self.read_offset.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let ptr = unsafe { self.mapped_ptr.add(current as usize) };
            Some((current, ptr))
        } else {
            None
        }
    }

    fn signal_fence(&self, queue: &ID3D12CommandQueue) -> u64 {
        let value = self.fence_value.fetch_add(1, Ordering::AcqRel);
        unsafe { queue.Signal(&self.fence, value) }.ok();
        value
    }

    fn wait_fence(&self, value: u64) {
        unsafe { self.fence.SetEventOnCompletion(value, self.fence_event) }.ok();
        unsafe { windows::Win32::Foundation::WaitForSingleObject(self.fence_event, u32::MAX) };
    }
}

/// The native memory manager.
/// Replaces Memory + Shadow + Watch + Upload with a single coherent system.
pub struct NativeMemory {
    device: Arc<ID3D12Device>,
    copy_queue: Arc<ID3D12CommandQueue>,

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

    /// Frame counter for statistics.
    frame: AtomicU64,

    /// Statistics.
    stats: NativeMemoryStats,
}

#[derive(Default, Debug)]
struct NativeMemoryStats {
    host_allocations: AtomicUsize,
    gpu_resources: AtomicUsize,
    upload_bytes: AtomicU64,
    readback_bytes: AtomicU64,
    copy_operations: AtomicU64,
    fence_waits: AtomicU64,
    allocation_failures: AtomicUsize,
}

impl NativeMemory {
    /// Create a new native memory manager.
    pub fn new(device: Arc<ID3D12Device>, copy_queue: Arc<ID3D12CommandQueue>) -> anyhow::Result<Self> {
        // Create default arenas
        let default_arena = GpuArena::new(
            &device,
            GPU_ARENA_SIZE,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES,
        )?;

        let upload_arena = GpuArena::new(
            &device,
            GPU_ARENA_SIZE,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
        )?;

        Ok(NativeMemory {
            device,
            copy_queue,
            allocations: DashMap::new(),
            gpu_bindings: DashMap::new(),
            default_arenas: Mutex::new(vec![Arc::new(default_arena)]),
            upload_arenas: Mutex::new(vec![Arc::new(upload_arena)]),
            upload_ring: OnceLock::new(),
            readback_ring: OnceLock::new(),
            frame: AtomicU64::new(0),
            stats: NativeMemoryStats::default(),
        })
    }

    /// Get or create the upload ring buffer.
    fn upload_ring(&self) -> &UploadRing {
        self.upload_ring.get_or_init(|| {
            UploadRing::new(&self.device, UPLOAD_RING_SIZE).expect("Failed to create upload ring")
        })
    }

    /// Get or create the readback ring buffer.
    fn readback_ring(&self) -> &ReadbackRing {
        self.readback_ring.get_or_init(|| {
            ReadbackRing::new(&self.device, READBACK_RING_SIZE).expect("Failed to create readback ring")
        })
    }

    /// Allocate title virtual memory (replaces XMemVirtualAlloc).
    /// Returns the base title VA.
    pub fn allocate(&self, title_va: u64, size: u64, protect: u32, placeholder: bool) -> anyhow::Result<u64> {
        // Align to page boundary
        let size = size.next_multiple_of(PAGE);
        let aligned_va = title_va & !(PAGE - 1);

        if placeholder {
            // Reserve address space without committing
            let ptr = unsafe {
                VirtualAlloc(
                    aligned_va as *mut _,
                    size as usize,
                    MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            if ptr.is_null() {
                self.stats.allocation_failures.fetch_add(1, Ordering::Relaxed);
                return Err(anyhow::anyhow!("VirtualAlloc reserve failed"));
            }

            let alloc = Arc::new(HostAllocation {
                title_va: aligned_va,
                size,
                protect,
                placeholder: true,
                base_ptr: ptr,
                committed_size: 0,
            });
            self.allocations.insert(aligned_va, alloc);
            self.stats.host_allocations.fetch_add(1, Ordering::Relaxed);
            debug!("Reserved title VA {:#x} + {:#x} (placeholder)", aligned_va, size);
            Ok(aligned_va)
        } else {
            // Commit memory
            let ptr = unsafe {
                VirtualAlloc(
                    aligned_va as *mut _,
                    size as usize,
                    MEM_RESERVE | MEM_COMMIT,
                    protect,
                )
            };
            if ptr.is_null() {
                self.stats.allocation_failures.fetch_add(1, Ordering::Relaxed);
                return Err(anyhow::anyhow!("VirtualAlloc commit failed"));
            }

            let alloc = Arc::new(HostAllocation {
                title_va: aligned_va,
                size,
                protect,
                placeholder: false,
                base_ptr: ptr,
                committed_size: size,
            });
            self.allocations.insert(aligned_va, alloc);
            self.stats.host_allocations.fetch_add(1, Ordering::Relaxed);
            debug!("Allocated title VA {:#x} + {:#x}", aligned_va, size);
            Ok(aligned_va)
        }
    }

    /// Free title virtual memory (replaces XMemVirtualFree).
    pub fn free(&self, title_va: u64) -> bool {
        if let Some((_, alloc)) = self.allocations.remove(&title_va) {
            // Also remove any GPU binding
            self.gpu_bindings.remove(&title_va);
            debug!("Freed title VA {:#x} + {:#x}", alloc.title_va, alloc.size);
            true
        } else {
            false
        }
    }

    /// Create a GPU resource bound to title virtual memory.
    /// This is the key replacement for shadow copies - direct binding.
    pub fn create_gpu_resource(
        &self,
        title_va: u64,
        size: u64,
        uav: bool,
    ) -> anyhow::Result<Arc<GpuResourceBinding>> {
        let size = size.next_multiple_of(PAGE);

        // Try to allocate from default arena
        let (resource, gpu_va) = self.allocate_default_resource(size, uav)?;

        let binding = Arc::new(GpuResourceBinding {
            title_va,
            size,
            resource,
            gpu_va,
            uav,
            upload_staging: None,
            readback_staging: None,
            fence_value: 0,
        });

        self.gpu_bindings.insert(title_va, binding.clone());
        self.stats.gpu_resources.fetch_add(1, Ordering::Relaxed);
        debug!("Created GPU resource at title VA {:#x} + {:#x}, GPU VA {:#x}", title_va, size, gpu_va);
        Ok(binding)
    }

    /// Allocate a resource from the DEFAULT heap arena.
    fn allocate_default_resource(&self, size: u64, uav: bool) -> anyhow::Result<(ID3D12Resource, u64)> {
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

                    let resource: ID3D12Resource = unsafe {
                        self.device.CreatePlacedResource(&arena.heap, offset, &desc, D3D12_RESOURCE_STATE_COMMON, None)?
                    };

                    // Get GPU VA
                    let gpu_va = unsafe { resource.GetGPUVirtualAddress() };

                    // Track allocation
                    arena.allocations.lock().unwrap().insert(gpu_va, (offset, size));

                    return Ok((resource, gpu_va));
                }
            }
        }

        // Need new arena
        let new_arena = GpuArena::new(
            &self.device,
            GPU_ARENA_SIZE,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_HEAP_FLAG_ALLOW_ALL_BUFFERS_AND_TEXTURES,
        )?;

        let offset = new_arena.allocate(size, alignment).expect("New arena should have space");
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

        let resource: ID3D12Resource = unsafe {
            new_arena.heap.CreatePlacedResource(offset, &desc, D3D12_RESOURCE_STATE_COMMON, None)?
        };

        let gpu_va = unsafe { resource.GetGPUVirtualAddress() };
        new_arena.allocations.lock().unwrap().insert(gpu_va, (offset, size));

        Ok((resource, gpu_va))
    }

    /// Upload data from CPU to GPU resource (CPU -> VRAM).
    /// Uses persistent upload ring buffer with batch fence synchronization.
    pub fn upload_to_gpu(&self, title_va: u64, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        let Some(binding) = self.gpu_bindings.get(&title_va) else {
            return Err(anyhow::anyhow!("No GPU resource bound to title VA {:#x}", title_va));
        };

        let size = data.len() as u64;
        if offset + size > binding.size {
            return Err(anyhow::anyhow!("Upload exceeds resource bounds"));
        }

        // Allocate from upload ring
        let ring = self.upload_ring();
        let (ring_offset, ring_ptr) = ring.allocate(size, UPLOAD_ALIGNMENT)
            .ok_or_else(|| anyhow::anyhow!("Upload ring full, need to wait for fence"))?;

        // Copy data to ring buffer
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ring_ptr, size as usize);
        }

        // Record copy command
        let copy_queue = &self.copy_queue;
        unsafe {
            copy_queue.CopyBufferRegion(
                &binding.resource,
                offset,
                &ring.resource,
                ring_offset,
                size,
            );
        }

        // Signal fence
        let fence_value = ring.signal_fence(copy_queue);
        binding.fence_value = fence_value;

        self.stats.upload_bytes.fetch_add(size, Ordering::Relaxed);
        self.stats.copy_operations.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Read back data from GPU resource to CPU (VRAM -> CPU).
    /// Uses persistent readback ring buffer.
    pub fn readback_from_gpu(&self, title_va: u64, offset: u64, size: u64) -> anyhow::Result<Vec<u8>> {
        let Some(binding) = self.gpu_bindings.get(&title_va) else {
            return Err(anyhow::anyhow!("No GPU resource bound to title VA {:#x}", title_va));
        };

        if offset + size > binding.size {
            return Err(anyhow::anyhow!("Readback exceeds resource bounds"));
        }

        // Allocate from readback ring
        let ring = self.readback_ring();
        let (ring_offset, ring_ptr) = ring.allocate(size, UPLOAD_ALIGNMENT)
            .ok_or_else(|| anyhow::anyhow!("Readback ring full"))?;

        // Record copy command
        let copy_queue = &self.copy_queue;
        unsafe {
            copy_queue.CopyBufferRegion(
                &ring.resource,
                ring_offset,
                &binding.resource,
                offset,
                size,
            );
        }

        // Signal and wait for fence (readback must be synchronous for correctness)
        let fence_value = ring.signal_fence(copy_queue);
        ring.wait_fence(fence_value);

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
        self.gpu_bindings.get(&title_va).map(|b| b.gpu_va + (title_va - b.title_va))
    }

    /// Check if a title VA has a GPU resource bound.
    pub fn has_gpu_binding(&self, title_va: u64) -> bool {
        self.gpu_bindings.contains_key(&title_va)
    }

    /// Get the host allocation for a title VA.
    pub fn get_allocation(&self, title_va: u64) -> Option<Arc<HostAllocation>> {
        self.allocations.get(&title_va).map(|a| a.clone())
    }

    /// Advance frame counter and report stats.
    pub fn next_frame(&self) {
        let frame = self.frame.fetch_add(1, Ordering::Relaxed);
        if frame % 60 == 0 {
            self.report_stats(frame);
        }
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
static NATIVE_MEMORY: OnceLock<NativeMemory> = OnceLock::new();

/// Initialize the native memory backend.
pub fn init_native_memory(device: Arc<ID3D12Device>, copy_queue: Arc<ID3D12CommandQueue>) -> anyhow::Result<&'static NativeMemory> {
    let mem = NativeMemory::new(device, copy_queue)?;
    NATIVE_MEMORY.set(mem).map_err(|_| anyhow::anyhow!("Native memory already initialized"))?;
    Ok(NATIVE_MEMORY.get().unwrap())
}

/// Get the native memory instance.
pub fn native_memory() -> Option<&'static NativeMemory> {
    NATIVE_MEMORY.get()
}

/// Check if native memory backend is active.
pub fn is_native_memory_active() -> bool {
    NATIVE_MEMORY.get().is_some()
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