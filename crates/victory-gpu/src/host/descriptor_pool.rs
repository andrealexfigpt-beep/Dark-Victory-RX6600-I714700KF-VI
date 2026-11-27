//! Persistent descriptor heap management (Phase 3 Native Graphics).
//!
//! Manages descriptor heaps (CBV/SRV/UAV, Sampler, RTV, DSV) with
//! persistent allocations and reuse. Reduces heap creation overhead
//! and fragmentation.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::host::host;

/// A block of descriptors within a heap.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DescriptorBlock {
    /// The heap this block belongs to.
    pub(crate) heap_index: usize,
    /// Start descriptor index in the heap.
    pub(crate) start: u32,
    /// Number of descriptors in this block.
    pub(crate) count: u32,
    /// CPU descriptor handle for the start.
    pub(crate) cpu_handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    /// GPU descriptor handle for the start (if shader visible).
    pub(crate) gpu_handle: D3D12_GPU_DESCRIPTOR_HANDLE,
}

/// A descriptor heap with free block tracking.
struct DescriptorHeap {
    heap: ID3D12DescriptorHeap,
    /// Size of each descriptor in this heap.
    descriptor_size: u32,
    /// Total descriptors in this heap.
    capacity: u32,
    /// Free blocks (start, count), kept sorted by start.
    free_blocks: Vec<(u32, u32)>,
    /// Whether this heap is shader visible (has GPU handles).
    shader_visible: bool,
    /// CPU base handle.
    cpu_base: D3D12_CPU_DESCRIPTOR_HANDLE,
    /// GPU base handle (if shader visible).
    gpu_base: D3D12_GPU_DESCRIPTOR_HANDLE,
}

/// Pool for managing descriptor heaps of a specific type.
pub(crate) struct DescriptorHeapPool {
    heaps: Mutex<Vec<DescriptorHeap>>,
    heap_type: D3D12_DESCRIPTOR_HEAP_TYPE,
    /// Descriptor size for this heap type (cached).
    descriptor_size: u32,
    /// Maximum descriptors per heap.
    max_per_heap: u32,
    /// Statistics.
    stats: HeapPoolStats,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct HeapPoolStats {
    pub heaps_created: u64,
    pub allocations: u64,
    pub deallocations: u64,
    pub current_allocated: u64,
    pub peak_allocated: u64,
}

impl DescriptorHeapPool {
    /// Create a new descriptor heap pool.
    pub(crate) fn new(
        device: &ID3D12Device,
        heap_type: D3D12_DESCRIPTOR_HEAP_TYPE,
        max_per_heap: u32,
    ) -> windows::core::Result<Self> {
        // Get descriptor size.
        let descriptor_size = unsafe { device.GetDescriptorHandleIncrementSize(heap_type) };

        // Validate heap type flags.
        let flags = match heap_type {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV => D3D12_DESCRIPTOR_HEAP_FLAGS_SHADER_VISIBLE,
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER => D3D12_DESCRIPTOR_HEAP_FLAGS_SHADER_VISIBLE,
            _ => D3D12_DESCRIPTOR_HEAP_FLAGS_NONE,
        };

        Ok(DescriptorHeapPool {
            heaps: Mutex::new(Vec::new()),
            heap_type,
            descriptor_size,
            max_per_heap,
            stats: HeapPoolStats::default(),
        })
    }

    /// Allocate a block of descriptors.
    pub(crate) fn allocate(&self, count: u32) -> windows::core::Result<DescriptorBlock> {
        if count == 0 {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
        }

        let mut heaps = self.heaps.lock().unwrap();

        // Try to find space in existing heaps.
        for (heap_index, heap) in heaps.iter_mut().enumerate() {
            if let Some((start, _)) = heap.find_free_block(count) {
                let block = heap.allocate_block(heap_index as u32, start, count);
                self.stats.allocations += 1;
                self.stats.current_allocated += count as u64;
                self.stats.peak_allocated = self.stats.peak_allocated.max(self.stats.current_allocated);
                return Ok(block);
            }
        }

        // Need a new heap.
        let device = host().expect("Host not initialized").device.clone();
        let flags = match self.heap_type {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV => D3D12_DESCRIPTOR_HEAP_FLAGS_SHADER_VISIBLE,
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER => D3D12_DESCRIPTOR_HEAP_FLAGS_SHADER_VISIBLE,
            _ => D3D12_DESCRIPTOR_HEAP_FLAGS_NONE,
        };

        let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: self.heap_type,
            NumDescriptors: self.max_per_heap,
            Flags: flags,
            NodeMask: 0,
        };

        let heap: ID3D12DescriptorHeap = unsafe { device.CreateDescriptorHeap(&heap_desc)? };

        let cpu_base = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
        let gpu_base = if flags & D3D12_DESCRIPTOR_HEAP_FLAGS_SHADER_VISIBLE.0 != 0 {
            unsafe { heap.GetGPUDescriptorHandleForHeapStart() }
        } else {
            D3D12_GPU_DESCRIPTOR_HANDLE { ptr: 0 }
        };

        let mut new_heap = DescriptorHeap {
            heap,
            descriptor_size: self.descriptor_size,
            capacity: self.max_per_heap,
            free_blocks: vec![(0, self.max_per_heap)],
            shader_visible: flags & D3D12_DESCRIPTOR_HEAP_FLAGS_SHADER_VISIBLE.0 != 0,
            cpu_base,
            gpu_base,
        };

        let block = new_heap.allocate_block(heaps.len() as u32, 0, count);
        heaps.push(new_heap);

        self.stats.heaps_created += 1;
        self.stats.allocations += 1;
        self.stats.current_allocated += count as u64;
        self.stats.peak_allocated = self.stats.peak_allocated.max(self.stats.current_allocated);

        Ok(block)
    }

    /// Free a previously allocated block.
    pub(crate) fn free(&self, block: DescriptorBlock) {
        let mut heaps = self.heaps.lock().unwrap();
        if (block.heap_index as usize) < heaps.len() {
            heaps[block.heap_index as usize].free_block(block.start, block.count);
            self.stats.deallocations += 1;
            self.stats.current_allocated = self.stats.current_allocated.saturating_sub(block.count as u64);
        }
    }

    /// Get the heap for a block (for binding).
    pub(crate) fn get_heap(&self, heap_index: usize) -> Option<ID3D12DescriptorHeap> {
        let heaps = self.heaps.lock().unwrap();
        heaps.get(heap_index).map(|h| h.heap.clone())
    }

    /// Get pool statistics.
    pub(crate) fn stats(&self) -> HeapPoolStats {
        self.stats
    }
}

impl DescriptorHeap {
    /// Find a free block of at least `count` descriptors.
    fn find_free_block(&self, count: u32) -> Option<(u32, u32)> {
        for (start, free_count) in &self.free_blocks {
            if *free_count >= count {
                return Some((*start, *free_count));
            }
        }
        None
    }

    /// Allocate a block at the given start position.
    fn allocate_block(&mut self, heap_index: u32, start: u32, count: u32) -> DescriptorBlock {
        // Find and update the free block.
        for i in 0..self.free_blocks.len() {
            let (block_start, block_count) = self.free_blocks[i];
            if block_start == start && block_count >= count {
                // Remove or shrink this free block.
                if block_count == count {
                    self.free_blocks.remove(i);
                } else {
                    self.free_blocks[i] = (block_start + count, block_count - count);
                }
                break;
            }
        }

        let cpu_handle = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu_base.ptr + (start as usize * self.descriptor_size as usize),
        };

        let gpu_handle = if self.shader_visible {
            D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: self.gpu_base.ptr + (start as u64 * self.descriptor_size as u64),
            }
        } else {
            D3D12_GPU_DESCRIPTOR_HANDLE { ptr: 0 }
        };

        DescriptorBlock {
            heap_index: heap_index as usize,
            start,
            count,
            cpu_handle,
            gpu_handle,
        }
    }

    /// Free a block and merge with adjacent free blocks.
    fn free_block(&mut self, start: u32, count: u32) {
        let end = start + count;
        let mut new_blocks = Vec::new();
        let mut merged = false;

        for (block_start, block_count) in &self.free_blocks {
            let block_end = block_start + block_count;

            if block_end == start {
                // Merge with previous block.
                new_blocks.push((*block_start, block_count + count));
                merged = true;
            } else if end == *block_start {
                // Merge with next block.
                new_blocks.push((start, count + block_count));
                merged = true;
            } else {
                new_blocks.push((*block_start, *block_count));
            }
        }

        if !merged {
            // Insert in sorted order.
            let mut inserted = false;
            for (block_start, block_count) in self.free_blocks.drain(..) {
                if !inserted && block_start > start {
                    new_blocks.push((start, count));
                    inserted = true;
                }
                new_blocks.push((block_start, block_count));
            }
            if !inserted {
                new_blocks.push((start, count));
            }
        }

        // Merge adjacent blocks in new_blocks.
        self.free_blocks = Vec::new();
        for (block_start, block_count) in new_blocks {
            if let Some(last) = self.free_blocks.last_mut() {
                if last.0 + last.1 == block_start {
                    last.1 += block_count;
                } else {
                    self.free_blocks.push((block_start, block_count));
                }
            } else {
                self.free_blocks.push((block_start, block_count));
            }
        }
    }
}

/// Global descriptor heap pools.
static CBV_SRV_UAV_POOL: std::sync::OnceLock<DescriptorHeapPool> = std::sync::OnceLock::new();
static SAMPLER_POOL: std::sync::OnceLock<DescriptorHeapPool> = std::sync::OnceLock::new();
static RTV_POOL: std::sync::OnceLock<DescriptorHeapPool> = std::sync::OnceLock::new();
static DSV_POOL: std::sync::OnceLock<DescriptorHeapPool> = std::sync::OnceLock::new();

/// Initialize all descriptor heap pools.
pub(crate) fn init_descriptor_pools(device: &ID3D12Device) -> windows::core::Result<()> {
    // CBV/SRV/UAV pool: 1M descriptors per heap (shader visible)
    CBV_SRV_UAV_POOL.set(DescriptorHeapPool::new(device, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1_000_000)?)
        .map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;

    // Sampler pool: 64K descriptors per heap (shader visible)
    SAMPLER_POOL.set(DescriptorHeapPool::new(device, D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 65_536)?)
        .map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;

    // RTV pool: 4K descriptors per heap (not shader visible)
    RTV_POOL.set(DescriptorHeapPool::new(device, D3D12_DESCRIPTOR_HEAP_TYPE_RTV, 4_096)?)
        .map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;

    // DSV pool: 4K descriptors per heap (not shader visible)
    DSV_POOL.set(DescriptorHeapPool::new(device, D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 4_096)?)
        .map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;

    Ok(())
}

/// Get the CBV/SRV/UAV descriptor pool.
pub(crate) fn cbv_srv_uav_pool() -> &'static DescriptorHeapPool {
    CBV_SRV_UAV_POOL.get().expect("CBV/SRV/UAV pool not initialized")
}

/// Get the Sampler descriptor pool.
pub(crate) fn sampler_pool() -> &'static DescriptorHeapPool {
    SAMPLER_POOL.get().expect("Sampler pool not initialized")
}

/// Get the RTV descriptor pool.
pub(crate) fn rtv_pool() -> &'static DescriptorHeapPool {
    RTV_POOL.get().expect("RTV pool not initialized")
}

/// Get the DSV descriptor pool.
pub(crate) fn dsv_pool() -> &'static DescriptorHeapPool {
    DSV_POOL.get().expect("DSV pool not initialized")
}

/// Allocate CBV/SRV/UAV descriptors.
pub(crate) fn allocate_cbv_srv_uav(count: u32) -> windows::core::Result<DescriptorBlock> {
    cbv_srv_uav_pool().allocate(count)
}

/// Allocate Sampler descriptors.
pub(crate) fn allocate_samplers(count: u32) -> windows::core::Result<DescriptorBlock> {
    sampler_pool().allocate(count)
}

/// Allocate RTV descriptors.
pub(crate) fn allocate_rtv(count: u32) -> windows::core::Result<DescriptorBlock> {
    rtv_pool().allocate(count)
}

/// Allocate DSV descriptors.
pub(crate) fn allocate_dsv(count: u32) -> windows::core::Result<DescriptorBlock> {
    dsv_pool().allocate(count)
}

/// Free CBV/SRV/UAV descriptors.
pub(crate) fn free_cbv_srv_uav(block: DescriptorBlock) {
    cbv_srv_uav_pool().free(block);
}

/// Free Sampler descriptors.
pub(crate) fn free_samplers(block: DescriptorBlock) {
    sampler_pool().free(block);
}

/// Free RTV descriptors.
pub(crate) fn free_rtv(block: DescriptorBlock) {
    rtv_pool().free(block);
}

/// Free DSV descriptors.
pub(crate) fn free_dsv(block: DescriptorBlock) {
    dsv_pool().free(block);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(all(windows, feature = "native-memory"))]
    fn test_descriptor_pool_basic() {
        let host = host().expect("Failed to get host");
        
        init_descriptor_pools(&host.device).expect("Failed to init pools");
        
        // Allocate some CBV/SRV/UAV descriptors
        let block = allocate_cbv_srv_uav(10).expect("Failed to allocate CBV/SRV/UAV");
        assert_eq!(block.count, 10);
        assert!(block.cpu_handle.ptr != 0);
        assert!(block.gpu_handle.ptr != 0);
        
        // Allocate more
        let block2 = allocate_cbv_srv_uav(5).expect("Failed to allocate more");
        assert_eq!(block2.count, 5);
        
        // Free first block
        free_cbv_srv_uav(block);
        
        // Allocate again - should reuse freed space
        let block3 = allocate_cbv_srv_uav(10).expect("Failed to reallocate");
        assert_eq!(block3.count, 10);
    }
}