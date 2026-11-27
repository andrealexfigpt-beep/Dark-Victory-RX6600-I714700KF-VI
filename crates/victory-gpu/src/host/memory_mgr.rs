//! Budget-aware VRAM and shared memory management (Phase 3 Native Graphics).
//!
//! Monitors GPU memory usage, enforces budgets, and manages eviction
//! of least-recently-used resources when budgets are exceeded.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use crate::host::host;

/// Memory budget configuration.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MemoryBudget {
    /// Total VRAM budget in bytes (0 = auto from adapter).
    pub(crate) vram_budget: u64,
    /// Total shared system memory budget in bytes (0 = auto).
    pub(crate) shared_budget: u64,
    /// Target VRAM usage ratio (0.0-1.0) before eviction starts.
    pub(crate) vram_target_ratio: f64,
    /// Target shared memory usage ratio before eviction starts.
    pub(crate) shared_target_ratio: f64,
    /// Minimum free VRAM to keep (bytes).
    pub(crate) vram_reserve: u64,
    /// Minimum free shared memory to keep (bytes).
    pub(crate) shared_reserve: u64,
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self {
            vram_budget: 0,
            shared_budget: 0,
            vram_target_ratio: 0.85,    // Start eviction at 85%
            shared_target_ratio: 0.90,  // Start eviction at 90%
            vram_reserve: 256 * 1024 * 1024,   // 256 MB reserve
            shared_reserve: 512 * 1024 * 1024, // 512 MB reserve
        }
    }
}

/// A tracked GPU resource with memory accounting.
#[derive(Debug, Clone)]
pub(crate) struct TrackedGpuResource {
    pub(crate) resource: ID3D12Resource,
    pub(crate) size: u64,
    pub(crate) is_vram: bool,  // true = VRAM (DEFAULT heap), false = shared (UPLOAD/READBACK)
    pub(crate) last_used_submission: u64,
    pub(crate) access_count: u64,
    pub(crate) persistent: bool, // If true, never evict
}

/// VRAM and shared memory manager.
pub(crate) struct MemoryManager {
    /// Configuration.
    config: Mutex<MemoryBudget>,
    /// Tracked resources.
    resources: Mutex<HashMap<usize, TrackedGpuResource>>,
    /// Current usage.
    vram_used: AtomicU64,
    shared_used: AtomicU64,
    /// Adapter for querying budget (if available).
    adapter: Option<IDXGIAdapter3>,
    /// Current submission counter.
    current_submission: AtomicU64,
    /// Statistics.
    stats: MemoryStats,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct MemoryStats {
    pub vram_used: u64,
    pub vram_budget: u64,
    pub shared_used: u64,
    pub shared_budget: u64,
    pub evictions: u64,
    pub evicted_bytes: u64,
    pub allocation_failures: u64,
    pub current_submission: u64,
}

impl MemoryManager {
    /// Create a new memory manager.
    pub(crate) fn new(device: &ID3D12Device, adapter: Option<IDXGIAdapter3>) -> Self {
        let mut config = MemoryBudget::default();
        
        // If adapter available, query actual budgets.
        if let Some(ref adapter) = adapter {
            if let Ok(mut budget) = unsafe {
                let mut budget: DXGI_QUERY_VIDEO_MEMORY_INFO = std::mem::zeroed();
                adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut budget)
            } {
                config.vram_budget = budget.Budget;
                config.vram_reserve = (budget.Budget as f64 * 0.05) as u64; // 5% reserve
            }
            if let Ok(mut budget) = unsafe {
                let mut budget: DXGI_QUERY_VIDEO_MEMORY_INFO = std::mem::zeroed();
                adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut budget)
            } {
                config.shared_budget = budget.Budget;
                config.shared_reserve = (budget.Budget as f64 * 0.05) as u64;
            }
        }

        Self {
            config: Mutex::new(config),
            resources: Mutex::new(HashMap::new()),
            vram_used: AtomicU64::new(0),
            shared_used: AtomicU64::new(0),
            adapter,
            current_submission: AtomicU64::new(0),
            stats: MemoryStats::default(),
        }
    }

    /// Update the current submission counter.
    pub(crate) fn new_submission(&self) -> u64 {
        self.current_submission.fetch_add(1, Ordering::AcqRel)
    }

    /// Register a resource for tracking.
    pub(crate) fn register(&self, resource: &ID3D12Resource, size: u64, is_vram: bool) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        
        let tracked = TrackedGpuResource {
            resource: resource.clone(),
            size,
            is_vram,
            last_used_submission: self.current_submission.load(Ordering::Acquire),
            access_count: 0,
            persistent: false,
        };

        resources.insert(ptr, tracked);
        
        if is_vram {
            self.vram_used.fetch_add(size, Ordering::AcqRel);
        } else {
            self.shared_used.fetch_add(size, Ordering::AcqRel);
        }
    }

    /// Unregister a resource.
    pub(crate) fn unregister(&self, resource: &ID3D12Resource) -> Option<u64> {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        
        if let Some(tracked) = resources.remove(&ptr) {
            if tracked.is_vram {
                self.vram_used.fetch_sub(tracked.size, Ordering::AcqRel);
            } else {
                self.shared_used.fetch_sub(tracked.size, Ordering::AcqRel);
            }
            Some(tracked.size)
        } else {
            None
        }
    }

    /// Mark a resource as used in the current submission.
    pub(crate) fn touch(&self, resource: &ID3D12Resource) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        if let Some(tracked) = resources.get_mut(&ptr) {
            tracked.last_used_submission = self.current_submission.load(Ordering::Acquire);
            tracked.access_count += 1;
        }
    }

    /// Mark a resource as persistent (never evict).
    pub(crate) fn set_persistent(&self, resource: &ID3D12Resource, persistent: bool) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        if let Some(tracked) = resources.get_mut(&ptr) {
            tracked.persistent = persistent;
        }
    }

    /// Check if we're over budget and evict if needed.
    pub(crate) fn check_and_evict(&self) -> u64 {
        let config = self.config.lock().unwrap().clone();
        let vram_used = self.vram_used.load(Ordering::Acquire);
        let shared_used = self.shared_used.load(Ordering::Acquire);
        
        let vram_budget = if config.vram_budget > 0 { config.vram_budget } else { u64::MAX };
        let shared_budget = if config.shared_budget > 0 { config.shared_budget } else { u64::MAX };
        
        let vram_target = (vram_budget as f64 * config.vram_target_ratio) as u64;
        let shared_target = (shared_budget as f64 * config.shared_target_ratio) as u64;
        
        let mut evicted = 0;
        
        // Evict VRAM if over target.
        if vram_used > vram_target {
            evicted += self.evict_lru(true, vram_used - vram_target);
        }
        
        // Evict shared if over target.
        if shared_used > shared_target {
            evicted += self.evict_lru(false, shared_used - shared_target);
        }
        
        evicted
    }

    /// Evict least recently used resources.
    fn evict_lru(&self, is_vram: bool, target_bytes: u64) -> u64 {
        let mut resources = self.resources.lock().unwrap();
        
        // Collect candidates (non-persistent, correct memory type).
        let mut candidates: Vec<_> = resources
            .iter()
            .filter(|(_, r)| r.is_vram == is_vram && !r.persistent)
            .map(|(ptr, r)| (*ptr, r.last_used_submission, r.size))
            .collect();
        
        // Sort by last used (oldest first).
        candidates.sort_by_key(|(_, last_used, _)| *last_used);
        
        let mut evicted = 0;
        for (ptr, _, size) in candidates {
            if evicted >= target_bytes {
                break;
            }
            
            if let Some(tracked) = resources.remove(&ptr) {
                if tracked.is_vram {
                    self.vram_used.fetch_sub(tracked.size, Ordering::AcqRel);
                } else {
                    self.shared_used.fetch_sub(tracked.size, Ordering::AcqRel);
                }
                evicted += tracked.size;
                self.stats.evictions += 1;
                self.stats.evicted_bytes += tracked.size;
            }
        }
        
        evicted
    }

    /// Try to allocate memory, evicting if necessary.
    pub(crate) fn try_allocate(&self, size: u64, is_vram: bool) -> bool {
        let config = self.config.lock().unwrap().clone();
        
        let used = if is_vram {
            self.vram_used.load(Ordering::Acquire)
        } else {
            self.shared_used.load(Ordering::Acquire)
        };
        
        let budget = if is_vram { config.vram_budget } else { config.shared_budget };
        let reserve = if is_vram { config.vram_reserve } else { config.shared_reserve };
        
        if budget > 0 && used + size + reserve > budget {
            // Try to evict.
            self.check_and_evict();
            
            // Recheck.
            let used = if is_vram {
                self.vram_used.load(Ordering::Acquire)
            } else {
                self.shared_used.load(Ordering::Acquire)
            };
            
            if budget > 0 && used + size + reserve > budget {
                self.stats.allocation_failures += 1;
                return false;
            }
        }
        
        true
    }

    /// Update budgets from adapter (call periodically).
    pub(crate) fn update_budgets(&self) {
        if let Some(ref adapter) = self.adapter {
            let mut config = self.config.lock().unwrap();
            
            if let Ok(mut budget) = unsafe {
                let mut budget: DXGI_QUERY_VIDEO_MEMORY_INFO = std::mem::zeroed();
                adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut budget)
            } {
                config.vram_budget = budget.Budget;
            }
            if let Ok(mut budget) = unsafe {
                let mut budget: DXGI_QUERY_VIDEO_MEMORY_INFO = std::mem::zeroed();
                adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut budget)
            } {
                config.shared_budget = budget.Budget;
            }
        }
    }

    /// Get current usage statistics.
    pub(crate) fn stats(&self) -> MemoryStats {
        let config = self.config.lock().unwrap();
        MemoryStats {
            vram_used: self.vram_used.load(Ordering::Acquire),
            vram_budget: config.vram_budget,
            shared_used: self.shared_used.load(Ordering::Acquire),
            shared_budget: config.shared_budget,
            evictions: self.stats.evictions,
            evicted_bytes: self.stats.evicted_bytes,
            allocation_failures: self.stats.allocation_failures,
            current_submission: self.current_submission.load(Ordering::Acquire),
        }
    }

    /// Get current VRAM usage.
    pub(crate) fn vram_used(&self) -> u64 {
        self.vram_used.load(Ordering::Acquire)
    }

    /// Get current shared memory usage.
    pub(crate) fn shared_used(&self) -> u64 {
        self.shared_used.load(Ordering::Acquire)
    }
}

/// Global memory manager instance.
static MEMORY_MANAGER: OnceLock<MemoryManager> = OnceLock::new();

/// Initialize the memory manager.
pub(crate) fn init_memory_manager(device: &ID3D12Device, adapter: Option<IDXGIAdapter3>) {
    let manager = MemoryManager::new(device, adapter);
    MEMORY_MANAGER.set(manager).ok();
}

/// Get the global memory manager.
pub(crate) fn memory_manager() -> &'static MemoryManager {
    MEMORY_MANAGER.get().expect("Memory manager not initialized")
}

/// Register a GPU resource for memory tracking.
pub(crate) fn track_gpu_resource(resource: &ID3D12Resource, size: u64, is_vram: bool) {
    memory_manager().register(resource, size, is_vram);
}

/// Unregister a GPU resource.
pub(crate) fn untrack_gpu_resource(resource: &ID3D12Resource) -> Option<u64> {
    memory_manager().unregister(resource)
}

/// Mark a GPU resource as used.
pub(crate) fn touch_gpu_resource(resource: &ID3D12Resource) {
    memory_manager().touch(resource);
}

/// Set a resource as persistent (never evict).
pub(crate) fn set_persistent_resource(resource: &ID3D12Resource, persistent: bool) {
    memory_manager().set_persistent(resource, persistent);
}

/// Check memory budgets and evict if needed.
pub(crate) fn check_memory_budget() -> u64 {
    memory_manager().check_and_evict()
}

/// Try to allocate memory.
pub(crate) fn try_allocate_memory(size: u64, is_vram: bool) -> bool {
    memory_manager().try_allocate(size, is_vram)
}

/// Get memory statistics.
pub(crate) fn memory_stats() -> MemoryStats {
    memory_manager().stats()
}

/// Update memory budgets from adapter.
pub(crate) fn update_memory_budgets() {
    memory_manager().update_budgets();
}

/// Start a new submission for memory tracking.
pub(crate) fn memory_new_submission() -> u64 {
    memory_manager().new_submission()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_budget_default() {
        let budget = MemoryBudget::default();
        assert_eq!(budget.vram_target_ratio, 0.85);
        assert_eq!(budget.shared_target_ratio, 0.90);
        assert_eq!(budget.vram_reserve, 256 * 1024 * 1024);
    }

    #[test]
    fn test_memory_stats_default() {
        let stats = MemoryStats::default();
        assert_eq!(stats.vram_used, 0);
        assert_eq!(stats.evictions, 0);
    }
}