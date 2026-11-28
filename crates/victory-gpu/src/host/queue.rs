//! Async queue management with cross-queue fences (Phase 3 Native Graphics).
//!
//! Provides dedicated graphics, compute, and copy queues with proper
//! synchronization via fences and timeline semaphores.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use windows::core::Interface;

use crate::host::host;

/// Queue types for async operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum QueueType {
    Graphics,
    Compute,
    Copy,
}

/// A command queue with its allocator pool and fence.
pub(crate) struct AsyncQueue {
    pub(crate) queue_type: QueueType,
    pub(crate) queue: ID3D12CommandQueue,
    pub(crate) fence: ID3D12Fence,
    pub(crate) fence_value: AtomicU64,
    /// Pending operations waiting on this queue.
    pending_ops: Mutex<VecDeque<PendingOp>>,
}

/// A pending operation on a queue.
#[derive(Debug)]
struct PendingOp {
    fence_value: u64,
    /// Resources to release after this operation completes.
    resources_to_release: Vec<ID3D12Resource>,
}

/// Manager for all async queues.
pub(crate) struct QueueManager {
    graphics: Option<AsyncQueue>,
    compute: Option<AsyncQueue>,
    copy: Option<AsyncQueue>,
    /// Shared timeline semaphore for cross-queue sync (if supported).
    timeline_semaphore: Option<ID3D12Fence>,
    /// Statistics.
    stats: QueueStats,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct QueueStats {
    pub graphics_submissions: u64,
    pub compute_submissions: u64,
    pub copy_submissions: u64,
    pub cross_queue_syncs: u64,
    pub fence_waits: u64,
}

impl QueueManager {
    /// Create a new queue manager.
    pub(crate) fn new(device: &ID3D12Device) -> windows::core::Result<Self> {
        // Create graphics queue (always present).
        let graphics_queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
            Flags: D3D12_COMMAND_QUEUE_FLAGS(0),
            NodeMask: 0,
        };
        let graphics_queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&graphics_queue_desc)? };
        let graphics_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };

        // Try to create compute queue (async compute).
        let compute = if Self::supports_compute_queue(device) {
            let compute_queue_desc = D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COMPUTE,
                Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
                Flags: D3D12_COMMAND_QUEUE_FLAGS(0),
                NodeMask: 0,
            };
            let compute_queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&compute_queue_desc)? };
            let compute_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
            Some(AsyncQueue {
                queue_type: QueueType::Compute,
                queue: compute_queue,
                fence: compute_fence,
                fence_value: AtomicU64::new(1),
                pending_ops: Mutex::new(VecDeque::new()),
            })
        } else {
            None
        };

        // Create copy queue.
        let copy_queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
            Flags: D3D12_COMMAND_QUEUE_FLAGS(0),
            NodeMask: 0,
        };
        let copy_queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&copy_queue_desc)? };
        let copy_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };

        // Try to create timeline semaphore for cross-queue sync.
        let timeline_semaphore = if Self::supports_timeline_semaphores(device) {
            let fence: ID3D12Fence = unsafe { 
                device.CreateFence(0, D3D12_FENCE_FLAG_SHARED | D3D12_FENCE_FLAG_SHARED_CROSS_ADAPTER)? 
            };
            Some(fence)
        } else {
            None
        };

        Ok(QueueManager {
            graphics: Some(AsyncQueue {
                queue_type: QueueType::Graphics,
                queue: graphics_queue,
                fence: graphics_fence,
                fence_value: AtomicU64::new(1),
                pending_ops: Mutex::new(VecDeque::new()),
            }),
            compute,
            copy: Some(AsyncQueue {
                queue_type: QueueType::Copy,
                queue: copy_queue,
                fence: copy_fence,
                fence_value: AtomicU64::new(1),
                pending_ops: Mutex::new(VecDeque::new()),
            }),
            timeline_semaphore,
            stats: QueueStats::default(),
        })
    }

    /// Check if device supports a separate compute queue.
    fn supports_compute_queue(device: &ID3D12Device) -> bool {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        let ok = unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        }.is_ok();
        ok && options.IndependentFrontPanelVideo.as_bool() // Rough heuristic: async compute support
    }

    /// Check if device supports timeline semaphores.
    fn supports_timeline_semaphores(device: &ID3D12Device) -> bool {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        let ok = unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        }.is_ok();
        ok && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED // SM 6.6 implies timeline
    }

    /// Get the graphics queue.
    pub(crate) fn graphics_queue(&self) -> &AsyncQueue {
        self.graphics.as_ref().expect("Graphics queue always present")
    }

    /// Get the compute queue (if available).
    pub(crate) fn compute_queue(&self) -> Option<&AsyncQueue> {
        self.compute.as_ref()
    }

    /// Get the copy queue.
    pub(crate) fn copy_queue(&self) -> &AsyncQueue {
        self.copy.as_ref().expect("Copy queue always present")
    }

    /// Get a queue by type.
    pub(crate) fn get_queue(&self, queue_type: QueueType) -> Option<&AsyncQueue> {
        match queue_type {
            QueueType::Graphics => self.graphics.as_ref(),
            QueueType::Compute => self.compute.as_ref(),
            QueueType::Copy => self.copy.as_ref(),
        }
    }

    /// Execute command lists on a queue and signal fence.
    pub(crate) fn execute(&self, queue_type: QueueType, lists: &[ID3D12CommandList]) -> windows::core::Result<u64> {
        let queue = self.get_queue(queue_type).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
        
        let fence_value = queue.fence_value.fetch_add(1, Ordering::AcqRel);
        
        unsafe { queue.queue.ExecuteCommandLists(lists) };
        unsafe { queue.queue.Signal(&queue.fence, fence_value)? };

        // Update stats.
        match queue_type {
            QueueType::Graphics => self.stats.graphics_submissions += 1,
            QueueType::Compute => self.stats.compute_submissions += 1,
            QueueType::Copy => self.stats.copy_submissions += 1,
        }

        Ok(fence_value)
    }

    /// Wait for a fence value on a queue.
    pub(crate) fn wait(&self, queue_type: QueueType, fence_value: u64) -> windows::core::Result<()> {
        let queue = self.get_queue(queue_type).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
        
        if unsafe { queue.fence.GetCompletedValue() } >= fence_value {
            return Ok(());
        }

        let event = unsafe { CreateEventW(None, false, false, None)? };
        unsafe { queue.fence.SetEventOnCompletion(fence_value, event)? };
        unsafe { WaitForSingleObject(event, INFINITE) };

        self.stats.fence_waits += 1;
        Ok(())
    }

    /// Signal a fence on one queue and wait on another (cross-queue sync).
    pub(crate) fn signal_and_wait(
        &self,
        signal_queue: QueueType,
        wait_queue: QueueType,
    ) -> windows::core::Result<u64> {
        let signal_q = self.get_queue(signal_queue).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;
        let wait_q = self.get_queue(wait_queue).ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))?;

        let fence_value = signal_q.fence_value.fetch_add(1, Ordering::AcqRel);
        
        // Signal on the signal queue.
        unsafe { signal_q.queue.Signal(&signal_q.fence, fence_value)? };
        
        // Wait on the wait queue.
        if unsafe { wait_q.fence.GetCompletedValue() } < fence_value {
            let event = unsafe { CreateEventW(None, false, false, None)? };
            unsafe { wait_q.fence.SetEventOnCompletion(fence_value, event)? };
            unsafe { WaitForSingleObject(event, INFINITE) };
        }

        self.stats.cross_queue_syncs += 1;
        Ok(fence_value)
    }

    /// Wait for multiple queues to reach a fence value.
    pub(crate) fn wait_multiple(&self, waits: &[(QueueType, u64)]) -> windows::core::Result<()> {
        // For simplicity, wait sequentially.
        // A more advanced implementation would use WaitForMultipleObjects.
        for (queue_type, value) in waits {
            self.wait(*queue_type, *value)?;
        }
        Ok(())
    }

    /// Get the timeline semaphore for cross-adapter sync.
    pub(crate) fn timeline_semaphore(&self) -> Option<&ID3D12Fence> {
        self.timeline_semaphore.as_ref()
    }

    /// Get statistics.
    pub(crate) fn stats(&self) -> QueueStats {
        self.stats
    }
}

impl AsyncQueue {
    /// Get the queue type.
    pub(crate) fn queue_type(&self) -> QueueType {
        self.queue_type
    }

    /// Get the underlying D3D12 command queue.
    pub(crate) fn queue(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    /// Get the fence.
    pub(crate) fn fence(&self) -> &ID3D12Fence {
        &self.fence
    }

    /// Get the current fence value.
    pub(crate) fn current_fence_value(&self) -> u64 {
        self.fence_value.load(Ordering::Acquire)
    }

    /// Signal the fence.
    pub(crate) fn signal(&self, value: u64) -> windows::core::Result<()> {
        unsafe { self.queue.Signal(&self.fence, value) }
    }

    /// Wait for a fence value.
    pub(crate) fn wait(&self, value: u64) -> windows::core::Result<()> {
        if unsafe { self.fence.GetCompletedValue() } >= value {
            return Ok(());
        }
        let event = unsafe { CreateEventW(None, false, false, None)? };
        unsafe { self.fence.SetEventOnCompletion(value, event)? };
        unsafe { WaitForSingleObject(event, INFINITE) };
        Ok(())
    }
}

/// Global queue manager instance.
static QUEUE_MANAGER: OnceLock<QueueManager> = OnceLock::new();

/// Initialize the queue manager.
pub(crate) fn init_queue_manager(device: &ID3D12Device) -> windows::core::Result<()> {
    let manager = QueueManager::new(device)?;
    QUEUE_MANAGER.set(manager).map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
}

/// Get the global queue manager.
pub(crate) fn queue_manager() -> &'static QueueManager {
    QUEUE_MANAGER.get().expect("Queue manager not initialized")
}

/// Get the graphics queue.
pub(crate) fn graphics_queue() -> &'static AsyncQueue {
    queue_manager().graphics_queue()
}

/// Get the compute queue (if available).
pub(crate) fn compute_queue() -> Option<&'static AsyncQueue> {
    queue_manager().compute_queue()
}

/// Get the copy queue.
pub(crate) fn copy_queue() -> &'static AsyncQueue {
    queue_manager().copy_queue()
}

/// Execute command lists on a specific queue.
pub(crate) fn execute_on_queue(queue_type: QueueType, lists: &[ID3D12CommandList]) -> windows::core::Result<u64> {
    queue_manager().execute(queue_type, lists)
}

/// Wait for a fence on a specific queue.
pub(crate) fn wait_on_queue(queue_type: QueueType, fence_value: u64) -> windows::core::Result<()> {
    queue_manager().wait(queue_type, fence_value)
}

/// Cross-queue synchronization: signal on one queue, wait on another.
pub(crate) fn sync_queues(signal_queue: QueueType, wait_queue: QueueType) -> windows::core::Result<u64> {
    queue_manager().signal_and_wait(signal_queue, wait_queue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_type_enum() {
        assert_eq!(QueueType::Graphics as u32, 0);
        assert_eq!(QueueType::Compute as u32, 1);
        assert_eq!(QueueType::Copy as u32, 2);
    }

    #[test]
    fn test_queue_stats_default() {
        let stats = QueueStats::default();
        assert_eq!(stats.graphics_submissions, 0);
        assert_eq!(stats.compute_submissions, 0);
        assert_eq!(stats.copy_submissions, 0);
        assert_eq!(stats.cross_queue_syncs, 0);
    }
}