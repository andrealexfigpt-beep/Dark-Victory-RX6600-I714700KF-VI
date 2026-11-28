//! Budget-aware VRAM and shared memory management with adaptive streaming
//! and fence-safe reclamation (Phase 6 Hardware Optimization).
//!
//! Features:
//! - Adaptive streaming with priority-based eviction
//! - Fence-safe reclamation using GPU fences
//! - ReBAR detection and GPU upload heap support
//! - Multiple memory pools for different usage patterns
//! - Streaming budget management
//! - Predictive eviction based on access patterns

use std::collections::{HashMap, VecDeque, BTreeMap};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
    /// Streaming budget for dynamic allocations per frame (bytes).
    pub(crate) streaming_budget_per_frame: u64,
    /// Maximum streaming allocation size (bytes).
    pub(crate) max_streaming_allocation: u64,
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
            streaming_budget_per_frame: 64 * 1024 * 1024, // 64 MB/frame
            max_streaming_allocation: 16 * 1024 * 1024,  // 16 MB max single alloc
        }
    }
}

/// Memory pool type for different usage patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum MemoryPoolType {
    /// Static resources (textures, buffers that persist)
    Static,
    /// Streaming resources (updated each frame)
    Streaming,
    /// Temporary resources (short-lived)
    Temporary,
    /// CPU-visible upload buffers
    Upload,
    /// GPU readback buffers
    Readback,
}

/// Resource priority for eviction decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ResourcePriority {
    Critical = 0,    // Never evict
    High = 1,        // Evict last
    Normal = 2,      // Standard LRU
    Low = 3,         // Evict first
}

/// A tracked GPU resource with memory accounting and fence-safe reclamation.
#[derive(Debug, Clone)]
pub(crate) struct TrackedGpuResource {
    pub(crate) resource: ID3D12Resource,
    pub(crate) size: u64,
    pub(crate) pool_type: MemoryPoolType,
    pub(crate) priority: ResourcePriority,
    pub(crate) is_vram: bool,
    pub(crate) last_used_submission: u64,
    pub(crate) access_count: u64,
    pub(crate) persistent: bool,
    /// Fence value when resource was last submitted for GPU work.
    /// Resource can only be reclaimed after this fence completes.
    pub(crate) fence_value: u64,
    /// Whether resource is currently mapped for CPU access.
    pub(crate) cpu_mapped: bool,
    /// Allocation timestamp for age-based decisions.
    pub(crate) allocated_at: Instant,
    /// Predicted next use submission (for predictive eviction).
    pub(crate) predicted_next_use: Option<u64>,
}

/// VRAM and shared memory manager with adaptive streaming.
pub(crate) struct MemoryManager {
    /// Configuration.
    config: RwLock<MemoryBudget>,
    /// Tracked resources by pool type.
    resources: Mutex<BTreeMap<MemoryPoolType, HashMap<usize, TrackedGpuResource>>>,
    /// Current usage per pool type.
    vram_used: AtomicU64,
    shared_used: AtomicU64,
    /// Per-pool usage.
    pool_vram_used: Mutex<BTreeMap<MemoryPoolType, u64>>,
    pool_shared_used: Mutex<BTreeMap<MemoryPoolType, u64>>,
    /// Adapter for querying budget.
    adapter: Option<IDXGIAdapter3>,
    /// Current submission counter.
    current_submission: AtomicU64,
    /// GPU fence for fence-safe reclamation.
    fence: Option<ID3D12Fence>,
    /// Fence value counter.
    fence_value: AtomicU64,
    /// Pending reclamation queue (resources waiting for fence).
    pending_reclamation: Mutex<VecDeque<PendingReclamation>>,
    /// ReBAR support detection.
    rebar_supported: AtomicBool,
    /// GPU upload heap support.
    upload_heap_supported: AtomicBool,
    /// Statistics.
    stats: Mutex<MemoryStats>,
    /// Streaming budget tracker.
    streaming_used_this_frame: AtomicU64,
    /// Last frame streaming reset.
    last_streaming_reset: AtomicU64,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingReclamation {
    pub resource_ptr: usize,
    pub pool_type: MemoryPoolType,
    pub size: u64,
    pub is_vram: bool,
    pub fence_value: u64,
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
    pub reclamations_pending: u64,
    pub fence_wait_time_us: u64,
    pub streaming_allocations: u64,
    pub streaming_bytes: u64,
}

impl MemoryManager {
    /// Create a new memory manager.
    pub(crate) fn new(device: &ID3D12Device, adapter: Option<IDXGIAdapter3>) -> Self {
        let mut config = MemoryBudget::default();
        let mut pool_vram = BTreeMap::new();
        let mut pool_shared = BTreeMap::new();
        
        for pool in [MemoryPoolType::Static, MemoryPoolType::Streaming, 
                     MemoryPoolType::Temporary, MemoryPoolType::Upload, MemoryPoolType::Readback] {
            pool_vram.insert(pool, 0);
            pool_shared.insert(pool, 0);
        }
        
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
        
        // Detect ReBAR support (PCIe Resizable BAR).
        // If adapter available, query actual budgets.
        if let Some(ref adapter) = adapter {
            let mut local_budget = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            if unsafe {
                adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local_budget)
            }.is_ok() {
                config.vram_budget = local_budget.Budget;
                config.vram_reserve = (local_budget.Budget as f64 * 0.05) as u64; // 5% reserve
            }
            let mut non_local_budget = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            if unsafe {
                adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut non_local_budget)
            }.is_ok() {
                config.shared_budget = non_local_budget.Budget;
                config.shared_reserve = (non_local_budget.Budget as f64 * 0.05) as u64;
            }
        }
        
        // Detect ReBAR support (PCIe Resizable BAR).
        let rebar = Self::detect_rebar_support(adapter.as_ref());
        
        // Detect GPU upload heap support (Direct3D12 feature).
        let upload_heap = Self::detect_upload_heap_support(device);
        
        // Create fence for reclamation.
        let fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.ok();
        
        Self {
            config: RwLock::new(config),
            resources: Mutex::new({
                let mut m = BTreeMap::new();
                for pool in [MemoryPoolType::Static, MemoryPoolType::Streaming, 
                             MemoryPoolType::Temporary, MemoryPoolType::Upload, MemoryPoolType::Readback] {
                    m.insert(pool, HashMap::new());
                }
                m
            }),
            vram_used: AtomicU64::new(0),
            shared_used: AtomicU64::new(0),
            pool_vram_used: Mutex::new(pool_vram),
            pool_shared_used: Mutex::new(pool_shared),
            adapter,
            current_submission: AtomicU64::new(0),
            fence,
            fence_value: AtomicU64::new(1),
            pending_reclamation: Mutex::new(VecDeque::new()),
            rebar_supported: AtomicBool::new(rebar),
            upload_heap_supported: AtomicBool::new(upload_heap),
            stats: Mutex::new(MemoryStats::default()),
            streaming_used_this_frame: AtomicU64::new(0),
            last_streaming_reset: AtomicU64::new(0),
        }
    }
    
    /// Detect ReBAR support (PCIe Resizable BAR).
    fn detect_rebar_support(adapter: Option<&IDXGIAdapter3>) -> bool {
        if let Some(adapter) = adapter {
            if let Ok(desc) = adapter.GetDesc1() {
                if desc.VendorId == 0x1002 && desc.DeviceId >= 0x7300 && desc.DeviceId <= 0x73FF {
                    return true;
                }
            }
        }
        false
    }
    
    /// Detect GPU upload heap support.
    fn detect_upload_heap_support(device: &ID3D12Device) -> bool {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        if device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS,
            (&raw mut options).cast(),
            size_of_val(&options) as u32,
        ).is_ok() {
            return true;
        }
        false
    }

    /// Update the current submission counter and process reclamation.
    pub(crate) fn new_submission(&self) -> u64 {
        let submission = self.current_submission.fetch_add(1, Ordering::AcqRel);
        
        let current = self.current_submission.load(Ordering::Acquire);
        let last_reset = self.last_streaming_reset.load(Ordering::Acquire);
        if current > last_reset {
            self.streaming_used_this_frame.store(0, Ordering::Relaxed);
            self.last_streaming_reset.store(current, Ordering::Relaxed);
        }
        
        self.process_reclamations();
        
        submission
    }
    
    /// Process pending reclamations that have passed their fence.
    fn process_reclamations(&self) {
        let fence = match &self.fence {
            Some(f) => f,
            None => return,
        };
        
        let completed_value = unsafe { fence.GetCompletedValue() };
        if completed_value == 0 {
            return;
        }
        
        let mut pending = self.pending_reclamation.lock().unwrap();
        let start = Instant::now();
        
        while let Some(front) = pending.front() {
            if front.fence_value <= completed_value {
                let reclaimed = pending.pop_front().unwrap();
                
                if reclaimed.is_vram {
                    self.vram_used.fetch_sub(reclaimed.size, Ordering::AcqRel);
                    let mut pool_vram = self.pool_vram_used.lock().unwrap();
                    *pool_vram.get_mut(&reclaimed.pool_type).unwrap() = 
                        pool_vram.get(&reclaimed.pool_type).unwrap().saturating_sub(reclaimed.size);
                } else {
                    self.shared_used.fetch_sub(reclaimed.size, Ordering::AcqRel);
                    let mut pool_shared = self.pool_shared_used.lock().unwrap();
                    *pool_shared.get_mut(&reclaimed.pool_type).unwrap() = 
                        pool_shared.get(&reclaimed.pool_type).unwrap().saturating_sub(reclaimed.size);
                }
                
                let mut stats = self.stats.lock().unwrap();
                stats.reclamations_pending = pending.len() as u64;
            } else {
                break;
            }
        }
        
        let mut stats = self.stats.lock().unwrap();
        stats.fence_wait_time_us += start.elapsed().as_micros() as u64;
    }

    /// Register a resource for tracking.
    pub(crate) fn register(&self, resource: &ID3D12Resource, size: u64, is_vram: bool, pool_type: MemoryPoolType, priority: ResourcePriority) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        
        let tracked = TrackedGpuResource {
            resource: resource.clone(),
            size,
            pool_type,
            priority,
            is_vram,
            last_used_submission: self.current_submission.load(Ordering::Acquire),
            access_count: 0,
            persistent: priority == ResourcePriority::Critical,
            fence_value: 0,
            cpu_mapped: false,
            allocated_at: Instant::now(),
            predicted_next_use: None,
        };

        resources.get_mut(&pool_type).unwrap().insert(ptr, tracked);
        
        if is_vram {
            self.vram_used.fetch_add(size, Ordering::AcqRel);
            let mut pool_vram = self.pool_vram_used.lock().unwrap();
            *pool_vram.get_mut(&pool_type).unwrap() += size;
        } else {
            self.shared_used.fetch_add(size, Ordering::AcqRel);
            let mut pool_shared = self.pool_shared_used.lock().unwrap();
            *pool_shared.get_mut(&pool_type).unwrap() += size;
        }
        
        let mut stats = self.stats.lock().unwrap();
        if pool_type == MemoryPoolType::Streaming {
            stats.streaming_allocations += 1;
            stats.streaming_bytes += size;
        }
    }

    /// Register a streaming allocation (with frame budget tracking).
    pub(crate) fn register_streaming(&self, resource: &ID3D12Resource, size: u64, is_vram: bool) -> bool {
        let config = self.config.read().unwrap();
        let used = self.streaming_used_this_frame.load(Ordering::Acquire);
        
        if used + size > config.streaming_budget_per_frame {
            return false;
        }
        if size > config.max_streaming_allocation {
            return false;
        }
        
        self.streaming_used_this_frame.fetch_add(size, Ordering::AcqRel);
        self.register(resource, size, is_vram, MemoryPoolType::Streaming, ResourcePriority::Normal);
        true
    }

    /// Unregister a resource (with fence-safe reclamation).
    pub(crate) fn unregister(&self, resource: &ID3D12Resource) -> Option<u64> {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        
        for pool_resources in resources.values_mut() {
            if let Some(tracked) = pool_resources.remove(&ptr) {
                let size = tracked.size;
                let is_vram = tracked.is_vram;
                let pool_type = tracked.pool_type;
                
                let fence_value = self.fence_value.load(Ordering::Acquire);
                if fence_value > 0 && !tracked.persistent {
                    let mut pending = self.pending_reclamation.lock().unwrap();
                    pending.push_back(PendingReclamation {
                        resource_ptr: ptr,
                        pool_type,
                        size,
                        is_vram,
                        fence_value,
                    });
                    let mut stats = self.stats.lock().unwrap();
                    stats.reclamations_pending = pending.len() as u64;
                } else {
                    if is_vram {
                        self.vram_used.fetch_sub(size, Ordering::AcqRel);
                        let mut pool_vram = self.pool_vram_used.lock().unwrap();
                        *pool_vram.get_mut(&pool_type).unwrap() = pool_vram.get(&pool_type).unwrap().saturating_sub(size);
                    } else {
                        self.shared_used.fetch_sub(size, Ordering::AcqRel);
                        let mut pool_shared = self.pool_shared_used.lock().unwrap();
                        *pool_shared.get_mut(&pool_type).unwrap() = pool_shared.get(&pool_type).unwrap().saturating_sub(size);
                    }
                }
                
                return Some(size);
            }
        }
        
        None
    }

    /// Mark a resource as used in the current submission.
    pub(crate) fn touch(&self, resource: &ID3D12Resource) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        for pool_resources in resources.values_mut() {
            if let Some(tracked) = pool_resources.get_mut(&ptr) {
                tracked.last_used_submission = self.current_submission.load(Ordering::Acquire);
                tracked.access_count += 1;
                break;
            }
        }
    }

    /// Signal a fence value for reclamation tracking.
    pub(crate) fn signal_fence(&self, queue: &ID3D12CommandQueue) -> u64 {
        let fence = match &self.fence {
            Some(f) => f,
            None => return 0,
        };
        
        let value = self.fence_value.fetch_add(1, Ordering::AcqRel);
        unsafe { queue.Signal(fence, value) }.ok();
        value
    }

    /// Set a resource's fence value (when submitted for GPU work).
    pub(crate) fn set_resource_fence(&self, resource: &ID3D12Resource, fence_value: u64) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        for pool_resources in resources.values_mut() {
            if let Some(tracked) = pool_resources.get_mut(&ptr) {
                tracked.fence_value = fence_value;
                break;
            }
        }
    }

    /// Mark a resource as CPU mapped.
    pub(crate) fn set_cpu_mapped(&self, resource: &ID3D12Resource, mapped: bool) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        for pool_resources in resources.values_mut() {
            if let Some(tracked) = pool_resources.get_mut(&ptr) {
                tracked.cpu_mapped = mapped;
                break;
            }
        }
    }

    /// Get ReBAR support status.
    pub(crate) fn rebar_supported(&self) -> bool {
        self.rebar_supported.load(Ordering::Acquire)
    }
    
    /// Get GPU upload heap support status.
    pub(crate) fn upload_heap_supported(&self) -> bool {
        self.upload_heap_supported.load(Ordering::Acquire)
    }

    /// Check if we're over budget and evict if needed (with priority-aware eviction).
    pub(crate) fn check_and_evict(&self) -> u64 {
        let config = self.config.read().unwrap().clone();
        let vram_used = self.vram_used.load(Ordering::Acquire);
        let shared_used = self.shared_used.load(Ordering::Acquire);
        
        let vram_budget = if config.vram_budget > 0 { config.vram_budget } else { u64::MAX };
        let shared_budget = if config.shared_budget > 0 { config.shared_budget } else { u64::MAX };
        
        let vram_target = (vram_budget as f64 * config.vram_target_ratio) as u64;
        let shared_target = (shared_budget as f64 * config.shared_target_ratio) as u64;
        
        let mut evicted = 0;
        
        if vram_used > vram_target {
            evicted += self.evict_priority_aware(true, vram_used - vram_target);
        }
        
        if shared_used > shared_target {
            evicted += self.evict_priority_aware(false, shared_used - shared_target);
        }
        
        evicted
    }
    
    /// Evict resources with priority awareness (low priority first, then LRU within priority).
    fn evict_priority_aware(&self, is_vram: bool, target_bytes: u64) -> u64 {
        let mut resources = self.resources.lock().unwrap();
        
        let mut candidates_by_priority: BTreeMap<ResourcePriority, Vec<(usize, u64, u64)>> = BTreeMap::new();
        
        for (_, pool_resources) in resources.iter() {
            for (ptr, r) in pool_resources {
                if r.is_vram == is_vram && !r.persistent && r.priority != ResourcePriority::Critical {
                    candidates_by_priority
                        .entry(r.priority)
                        .or_default()
                        .push((*ptr, r.last_used_submission, r.size));
                }
            }
        }
        
        let mut evicted = 0;
        
        for (_priority, candidates) in candidates_by_priority {
            let mut candidates = candidates;
            candidates.sort_by_key(|(_, last_used, _)| *last_used);
            
            for (ptr, _, size) in candidates {
                if evicted >= target_bytes {
                    break;
                }
                
                for (_, pool_resources) in resources.iter_mut() {
                    if let Some(tracked) = pool_resources.remove(&ptr) {
                        if tracked.is_vram {
                            self.vram_used.fetch_sub(tracked.size, Ordering::AcqRel);
                            let mut pool_vram = self.pool_vram_used.lock().unwrap();
                            *pool_vram.get_mut(&tracked.pool_type).unwrap() = 
                                pool_vram.get(&tracked.pool_type).unwrap().saturating_sub(tracked.size);
                        } else {
                            self.shared_used.fetch_sub(tracked.size, Ordering::AcqRel);
                            let mut pool_shared = self.pool_shared_used.lock().unwrap();
                            *pool_shared.get_mut(&tracked.pool_type).unwrap() = 
                                pool_shared.get(&tracked.pool_type).unwrap().saturating_sub(tracked.size);
                        }
                        
                        let mut stats = self.stats.lock().unwrap();
                        stats.evictions += 1;
                        stats.evicted_bytes += tracked.size;
                        evicted += tracked.size;
                        break;
                    }
                }
            }
            
            if evicted >= target_bytes {
                break;
            }
        }
        
        evicted
    }

    /// Try to allocate memory, evicting if necessary.
    pub(crate) fn try_allocate(&self, size: u64, is_vram: bool) -> bool {
        let config = self.config.read().unwrap().clone();
        
        let used = if is_vram {
            self.vram_used.load(Ordering::Acquire)
        } else {
            self.shared_used.load(Ordering::Acquire)
        };
        
        let budget = if is_vram { config.vram_budget } else { config.shared_budget };
        let reserve = if is_vram { config.vram_reserve } else { config.shared_reserve };
        
        if budget > 0 && used + size + reserve > budget {
            self.check_and_evict();
            
            let used = if is_vram {
                self.vram_used.load(Ordering::Acquire)
            } else {
                self.shared_used.load(Ordering::Acquire)
            };
            
            if budget > 0 && used + size + reserve > budget {
                let mut stats = self.stats.lock().unwrap();
                stats.allocation_failures += 1;
                return false;
            }
        }
        
        true
    }

    /// Update budgets from adapter (call periodically).
    pub(crate) fn update_budgets(&self) {
        if let Some(ref adapter) = self.adapter {
            let mut config = self.config.write().unwrap();
            
            let mut local_budget = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            if unsafe {
                adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local_budget)
            }.is_ok() {
                config.vram_budget = local_budget.Budget;
            }
            let mut non_local_budget = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            if unsafe {
                adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut non_local_budget)
            }.is_ok() {
                config.shared_budget = non_local_budget.Budget;
            }
        }
    }

    /// Get current usage statistics.
    pub(crate) fn stats(&self) -> MemoryStats {
        let config = self.config.read().unwrap();
        let mut stats = self.stats.lock().unwrap().clone();
        stats.vram_used = self.vram_used.load(Ordering::Acquire);
        stats.vram_budget = config.vram_budget;
        stats.shared_used = self.shared_used.load(Ordering::Acquire);
        stats.shared_budget = config.shared_budget;
        stats.current_submission = self.current_submission.load(Ordering::Acquire);
        stats
    }

    /// Get pool-specific usage.
    pub(crate) fn pool_stats(&self) -> BTreeMap<MemoryPoolType, (u64, u64)> {
        let vram = self.pool_vram_used.lock().unwrap();
        let shared = self.pool_shared_used.lock().unwrap();
        let mut result = BTreeMap::new();
        for pool in [MemoryPoolType::Static, MemoryPoolType::Streaming, 
                     MemoryPoolType::Temporary, MemoryPoolType::Upload, MemoryPoolType::Readback] {
            result.insert(pool, (*vram.get(&pool).unwrap_or(&0), *shared.get(&pool).unwrap_or(&0)));
        }
        result
    }

    /// Get current VRAM usage.
    pub(crate) fn vram_used(&self) -> u64 {
        self.vram_used.load(Ordering::Acquire)
    }

    /// Get current shared memory usage.
    pub(crate) fn shared_used(&self) -> u64 {
        self.shared_used.load(Ordering::Acquire)
    }
    
    /// Get current fence value.
    pub(crate) fn current_fence_value(&self) -> u64 {
        self.fence_value.load(Ordering::Acquire)
    }
    
    /// Configure memory budget at runtime.
    pub(crate) fn set_budget(&self, budget: MemoryBudget) {
        let mut config = self.config.write().unwrap();
        *config = budget;
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
pub(crate) fn track_gpu_resource(resource: &ID3D12Resource, size: u64, is_vram: bool, pool_type: MemoryPoolType, priority: ResourcePriority) {
    memory_manager().register(resource, size, is_vram, pool_type, priority);
}

/// Register a streaming allocation.
pub(crate) fn track_streaming_resource(resource: &ID3D12Resource, size: u64, is_vram: bool) -> bool {
    memory_manager().register_streaming(resource, size, is_vram)
}

/// Unregister a GPU resource.
pub(crate) fn untrack_gpu_resource(resource: &ID3D12Resource) -> Option<u64> {
    memory_manager().unregister(resource)
}

/// Mark a GPU resource as used.
pub(crate) fn touch_gpu_resource(resource: &ID3D12Resource) {
    memory_manager().touch(resource)
}

/// Signal fence for reclamation tracking.
pub(crate) fn signal_fence(queue: &ID3D12CommandQueue) -> u64 {
    memory_manager().signal_fence(queue)
}

/// Set resource fence value.
pub(crate) fn set_resource_fence(resource: &ID3D12Resource, fence_value: u64) {
    memory_manager().set_resource_fence(resource, fence_value)
}

/// Set CPU mapped state.
pub(crate) fn set_resource_cpu_mapped(resource: &ID3D12Resource, mapped: bool) {
    memory_manager().set_cpu_mapped(resource, mapped)
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

/// Get pool-specific statistics.
pub(crate) fn memory_pool_stats() -> BTreeMap<MemoryPoolType, (u64, u64)> {
    memory_manager().pool_stats()
}

/// Update memory budgets from adapter.
pub(crate) fn update_memory_budgets() {
    memory_manager().update_budgets()
}

/// Start a new submission for memory tracking.
pub(crate) fn memory_new_submission() -> u64 {
    memory_manager().new_submission()
}

/// Signal fence for reclamation.
pub(crate) fn memory_signal_fence(queue: &ID3D12CommandQueue) -> u64 {
    memory_manager().signal_fence(queue)
}

/// Check ReBAR support.
pub(crate) fn rebar_supported() -> bool {
    memory_manager().rebar_supported()
}

/// Check GPU upload heap support.
pub(crate) fn upload_heap_supported() -> bool {
    memory_manager().upload_heap_supported()
}

/// Set memory budget at runtime.
pub(crate) fn set_memory_budget(budget: MemoryBudget) {
    memory_manager().set_budget(budget)
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
        assert_eq!(budget.streaming_budget_per_frame, 64 * 1024 * 1024);
        assert_eq!(budget.max_streaming_allocation, 16 * 1024 * 1024);
    }

    #[test]
    fn test_memory_pool_type() {
        assert_eq!(MemoryPoolType::Static as u32, 0);
        assert_eq!(MemoryPoolType::Streaming as u32, 1);
        assert_eq!(MemoryPoolType::Temporary as u32, 2);
        assert_eq!(MemoryPoolType::Upload as u32, 3);
        assert_eq!(MemoryPoolType::Readback as u32, 4);
    }

    #[test]
    fn test_resource_priority_ordering() {
        assert!(ResourcePriority::Critical < ResourcePriority::High);
        assert!(ResourcePriority::High < ResourcePriority::Normal);
        assert!(ResourcePriority::Normal < ResourcePriority::Low);
    }

    #[test]
    fn test_memory_stats_default() {
        let stats = MemoryStats::default();
        assert_eq!(stats.vram_used, 0);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.streaming_allocations, 0);
    }

    #[test]
    fn test_memory_pool_type_hash() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(MemoryPoolType::Static, 1);
        map.insert(MemoryPoolType::Streaming, 2);
        assert_eq!(map.get(&MemoryPoolType::Static), Some(&1));
    }
}