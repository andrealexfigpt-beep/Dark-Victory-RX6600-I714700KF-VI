//! Command allocator pool for reuse (Phase 3 Native Graphics).
//!
//! Reuses command allocators after GPU completion to avoid
//! repeated allocation overhead. Integrates with existing Host infrastructure.
//! Lists are created from pooled allocators and reset on reuse.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use windows::core::Interface;

use crate::host::host;

/// A pooled command allocator with its associated fence value.
#[derive(Clone)]
struct PooledAllocator {
    allocator: ID3D12CommandAllocator,
    /// The fence value at which this allocator was last submitted.
    /// Can be reused once the GPU fence reaches this value.
    fence_value: u64,
}

/// Pool for reusing command allocators by type.
pub(crate) struct CommandPool {
    /// Pool for DIRECT (graphics) command allocators.
    direct: Mutex<CommandTypePool>,
    /// Pool for COPY command allocators.
    copy: Mutex<CommandTypePool>,
    /// Pool for COMPUTE command allocators (separate for async compute).
    compute: Mutex<CommandTypePool>,
    /// Global fence for tracking GPU completion.
    fence: Arc<ID3D12Fence>,
    /// Current fence value (incremented on each submit).
    fence_value: AtomicU64,
    /// Statistics.
    stats: PoolStats,
}

struct CommandTypePool {
    /// Available (reset) allocators ready for reuse.
    available_allocators: VecDeque<PooledAllocator>,
    /// In-flight allocators waiting for GPU completion.
    in_flight_allocators: VecDeque<PooledAllocator>,
    /// Total allocators created (for stats).
    total_created: u64,
    /// Total allocators reused.
    allocators_reused: u64,
    /// Command list type.
    list_type: D3D12_COMMAND_LIST_TYPE,
}

#[derive(Default)]
struct PoolStats {
    pub direct_allocators_created: std::sync::atomic::AtomicU64,
    pub direct_allocators_reused: std::sync::atomic::AtomicU64,
    pub copy_allocators_created: std::sync::atomic::AtomicU64,
    pub copy_allocators_reused: std::sync::atomic::AtomicU64,
    pub compute_allocators_created: std::sync::atomic::AtomicU64,
    pub compute_allocators_reused: std::sync::atomic::AtomicU64,
}

impl CommandPool {
    /// Create a new command pool.
    pub(crate) fn new(device: &ID3D12Device) -> windows::core::Result<Self> {
        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }?;

        Ok(CommandPool {
            direct: Mutex::new(CommandTypePool::new(D3D12_COMMAND_LIST_TYPE_DIRECT)),
            copy: Mutex::new(CommandTypePool::new(D3D12_COMMAND_LIST_TYPE_COPY)),
            compute: Mutex::new(CommandTypePool::new(D3D12_COMMAND_LIST_TYPE_COMPUTE)),
            fence: Arc::new(fence),
            fence_value: AtomicU64::new(1),
            stats: PoolStats::default(),
        })
    }

    /// Get a command allocator for recording (DIRECT queue).
    /// The caller creates a command list from this allocator.
    pub(crate) fn get_direct_allocator(&self) -> windows::core::Result<PooledAllocatorHandle> {
        self.get_allocator(&self.direct, &self.stats.direct_allocators_created, &self.stats.direct_allocators_reused)
    }

    /// Get a command allocator for recording (COPY queue).
    pub(crate) fn get_copy_allocator(&self) -> windows::core::Result<PooledAllocatorHandle> {
        self.get_allocator(&self.copy, &self.stats.copy_allocators_created, &self.stats.copy_allocators_reused)
    }

    /// Get a command allocator for recording (COMPUTE queue).
    pub(crate) fn get_compute_allocator(&self) -> windows::core::Result<PooledAllocatorHandle> {
        self.get_allocator(&self.compute, &self.stats.compute_allocators_created, &self.stats.compute_allocators_reused)
    }

    fn get_allocator(
        &self,
        pool: &Mutex<CommandTypePool>,
        created: &std::sync::atomic::AtomicU64,
        reused: &std::sync::atomic::AtomicU64,
    ) -> windows::core::Result<PooledAllocatorHandle> {
        // First, try to reclaim any completed in-flight resources.
        self.reclaim_completed(pool);

        let mut pool = pool.lock().unwrap();

        // Try to get a pooled allocator.
        if let Some(pooled) = pool.available_allocators.pop_front() {
            unsafe { pooled.allocator.Reset()? };
            reused.fetch_add(1, Ordering::Relaxed);
            pool.allocators_reused += 1;

            return Ok(PooledAllocatorHandle {
                allocator: pooled.allocator,
                pool_type: pool.list_type,
                fence_value: 0,
                pool_fence: self.fence.clone(),
                in_flight_queue: std::sync::Arc::new(Mutex::new(pool.in_flight_allocators.clone())),
                // We'll push to the actual pool's in_flight on submit
                _pool_ref: &*pool as *const CommandTypePool,
            });
        }

        // Create new allocator.
        let allocator: ID3D12CommandAllocator = unsafe {
            crate::host::host().unwrap().device.CreateCommandAllocator(pool.list_type)?
        };

        created.fetch_add(1, Ordering::Relaxed);
        pool.total_created += 1;

        Ok(PooledAllocatorHandle {
            allocator,
            pool_type: pool.list_type,
            fence_value: 0,
            pool_fence: self.fence.clone(),
            in_flight_queue: std::sync::Arc::new(Mutex::new(pool.in_flight_allocators.clone())),
            _pool_ref: &*pool as *const CommandTypePool,
        })
    }

    /// Reclaim completed allocators from in-flight queues.
    fn reclaim_completed(&self, pool: &Mutex<CommandTypePool>) {
        let completed_value = unsafe { self.fence.GetCompletedValue() };
        if completed_value == 0 {
            return;
        }

        let mut pool = pool.lock().unwrap();

        while let Some(front) = pool.in_flight_allocators.front() {
            if front.fence_value <= completed_value {
                let reclaimed = pool.in_flight_allocators.pop_front().unwrap();
                pool.available_allocators.push_back(reclaimed);
            } else {
                break;
            }
        }
    }

    /// Mark an allocator as submitted with a fence value for later reuse.
    pub(crate) fn submit_allocator(&self, handle: PooledAllocatorHandle, queue: &ID3D12CommandQueue) -> windows::core::Result<u64> {
        let fence_value = self.fence_value.fetch_add(1, Ordering::AcqRel);
        
        // Signal the fence.
        unsafe { queue.Signal(self.fence.as_ref(), fence_value)? };

        // Return the allocator to the appropriate pool's in-flight queue.
        let pool = match handle.pool_type {
            D3D12_COMMAND_LIST_TYPE_DIRECT => &self.direct,
            D3D12_COMMAND_LIST_TYPE_COPY => &self.copy,
            D3D12_COMMAND_LIST_TYPE_COMPUTE => &self.compute,
            _ => &self.direct,
        };

        let mut pool = pool.lock().unwrap();
        pool.in_flight_allocators.push_back(PooledAllocator {
            allocator: handle.allocator,
            fence_value,
        });

        Ok(fence_value)
    }

    /// Wait for a specific fence value.
    pub(crate) fn wait_for_fence(&self, value: u64) -> windows::core::Result<()> {
        if value == 0 {
            return Ok(());
        }
        let completed = unsafe { self.fence.GetCompletedValue() };
        if completed >= value {
            return Ok(());
        }
        let event = unsafe { CreateEventW(None, false, false, None) }?;
        unsafe { self.fence.SetEventOnCompletion(value, event)? };
        unsafe { WaitForSingleObject(event, INFINITE) };
        Ok(())
    }

    /// Get the global fence for cross-queue synchronization.
    pub(crate) fn fence(&self) -> &ID3D12Fence {
        &self.fence
    }

    /// Get the current fence value.
    pub(crate) fn current_fence_value(&self) -> u64 {
        self.fence_value.load(Ordering::Acquire)
    }

    /// Get pool statistics.
    pub(crate) fn stats(&self) -> PoolStats {
        self.stats
    }
}

impl CommandTypePool {
    fn new(list_type: D3D12_COMMAND_LIST_TYPE) -> Self {
        CommandTypePool {
            available_allocators: VecDeque::new(),
            in_flight_allocators: VecDeque::new(),
            total_created: 0,
            allocators_reused: 0,
            list_type,
        }
    }
}

/// A handle to a pooled command allocator.
/// When dropped without submitting, the allocator is not returned to the pool (safety).
/// Must call submit_allocator() or explicitly return it.
pub(crate) struct PooledAllocatorHandle {
    allocator: ID3D12CommandAllocator,
    pool_type: D3D12_COMMAND_LIST_TYPE,
    fence_value: u64,
    pool_fence: Arc<ID3D12Fence>,
    in_flight_queue: std::sync::Arc<Mutex<VecDeque<PooledAllocator>>>,
    _pool_ref: *const CommandTypePool,
}

impl PooledAllocatorHandle {
    /// Get the underlying command allocator.
    pub(crate) fn allocator(&self) -> &ID3D12CommandAllocator {
        &self.allocator
    }

    /// Create a command list from this allocator.
    pub(crate) fn create_list(&self) -> windows::core::Result<ID3D12GraphicsCommandList> {
        let list: ID3D12GraphicsCommandList = unsafe {
            crate::host::host().unwrap().device.CreateCommandList(
                0,
                self.pool_type,
                &self.allocator,
                None,
            )?
        };
        #[cfg(feature = "diagnostics")]
        crate::host::pass_begin(&list);
        Ok(list)
    }

    /// Submit this allocator (after its list has been executed) and return fence value.
    pub(crate) fn submit(self, queue: &ID3D12CommandQueue) -> windows::core::Result<u64> {
        let host = host().expect("Host not initialized");
        host.command_pool().submit_allocator(self, queue)
    }
}

impl Drop for PooledAllocatorHandle {
    fn drop(&mut self) {
        // Safety: if not submitted, we don't return to pool (would be unsafe without fence).
        // The allocator will be cleaned up by Windows when dropped.
    }
}

/// Global command pool instance.
static COMMAND_POOL: std::sync::OnceLock<CommandPool> = std::sync::OnceLock::new();

/// Initialize the command pool.
pub(crate) fn init_command_pool(device: &ID3D12Device) -> windows::core::Result<()> {
    let pool = CommandPool::new(device)?;
    COMMAND_POOL.set(pool).map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
}

/// Get the global command pool.
pub(crate) fn command_pool() -> &'static CommandPool {
    COMMAND_POOL.get().expect("Command pool not initialized")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(all(windows, feature = "native-memory"))]
    fn test_command_pool_creation() {
        let host = host().expect("Failed to get host");
        let pool = CommandPool::new(&host.device).expect("Failed to create command pool");
        
        // Test getting a direct allocator
        let handle = pool.get_direct_allocator().expect("Failed to get direct allocator");
        assert!(!handle.allocator().is_null());
        
        // Test creating a list from it
        let list = handle.create_list().expect("Failed to create list");
        assert!(!list.is_null());
        
        // Test getting a copy allocator
        let handle = pool.get_copy_allocator().expect("Failed to get copy allocator");
        assert!(!handle.allocator().is_null());
    }
}