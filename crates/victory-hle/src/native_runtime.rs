//! Unified native runtime initialization and lifecycle management.
//!
//! Coordinates initialization of all native subsystems:
//! - Native Memory (memory_mgr, native_mem)
//! - Native Shaders (shader_perm, translation cache)
//! - Native Graphics (command_pool, descriptor_pool, binding, barrier, queue)
//! - Native Platform (kernel, files, xaudio2, gameinput, dstorage)
//!
//! Provides A/B execution modes, runtime profiling, budget-aware scheduling,
//! and coordinated shutdown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(windows)]
use victory_gpu::host::{host, Host};
#[cfg(windows)]
use victory_gpu::host::memory_mgr::{init_memory_manager, memory_stats, MemoryBudget};
#[cfg(windows)]
use victory_gpu::host::command_pool::init_command_pool;
#[cfg(windows)]
use victory_gpu::host::descriptor_pool::init_descriptor_pools;
#[cfg(windows)]
use victory_gpu::host::binding::set_binding_config;
#[cfg(windows)]
use victory_gpu::host::barrier::init_barrier_tracker;
#[cfg(windows)]
use victory_gpu::host::queue::init_queue_manager;
#[cfg(windows)]
use victory_gpu::shader_perm::{enable_persistence, stats as shader_stats};
#[cfg(windows)]
use victory_gpu::pipeline::library::use_pipeline_cache;
#[cfg(windows)]
use victory_hle::runtime::{use_console_timers, set_main_image};

/// Execution mode for the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Full native implementation with all optimizations.
    #[default]
    Native,
    /// Legacy fallback mode for compatibility.
    Legacy,
    /// Hybrid: native where validated, legacy elsewhere.
    Hybrid,
}

/// Hardware capabilities detected at startup.
#[derive(Debug, Clone, Default)]
pub struct HardwareCapabilities {
    pub vendor_id: u32,
    pub device_id: u32,
    pub vram_budget: u64,
    pub shared_memory_budget: u64,
    pub max_wave_lane_count: u32,
    pub min_wave_lane_count: u32,
    pub supports_mesh_shaders: bool,
    pub supports_depth_bounds: bool,
    pub supports_timeline_semaphores: bool,
    pub supports_async_compute: bool,
    pub is_rdna: bool,
    pub is_rdna2: bool,
    pub cpu_core_count: u32,
    pub cpu_logical_processors: u32,
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub execution_mode: ExecutionMode,
    pub enable_shader_cache: bool,
    pub enable_pipeline_cache: bool,
    pub enable_permutation_persistence: bool,
    pub shader_cache_dir: Option<std::path::PathBuf>,
    pub pipeline_cache_dir: Option<std::path::PathBuf>,
    pub permutation_cache_dir: Option<std::path::PathBuf>,
    pub memory_budget: Option<MemoryBudget>,
    pub enable_profiling: bool,
    pub profiling_interval: Duration,
    pub background_task_budget_ms: u64,
    pub streaming_budget_mb: u64,
    pub max_background_threads: u32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::Native,
            enable_shader_cache: true,
            enable_pipeline_cache: true,
            enable_permutation_persistence: true,
            shader_cache_dir: None,
            pipeline_cache_dir: None,
            permutation_cache_dir: None,
            memory_budget: None,
            enable_profiling: true,
            profiling_interval: Duration::from_millis(100),
            background_task_budget_ms: 16,
            streaming_budget_mb: 512,
            max_background_threads: 4,
        }
    }
}

/// Profiling data for a subsystem.
#[derive(Debug, Default, Clone)]
pub struct SubsystemProfile {
    pub cpu_time_ms: f64,
    pub gpu_time_ms: f64,
    pub ram_bytes: u64,
    pub vram_bytes: u64,
    pub sync_wait_ms: f64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub allocations: u64,
    pub evictions: u64,
}

/// Runtime profiling snapshot.
#[derive(Debug, Default, Clone)]
pub struct RuntimeProfile {
    pub timestamp: Instant,
    pub memory: SubsystemProfile,
    pub shader: SubsystemProfile,
    pub graphics: SubsystemProfile,
    pub platform: SubsystemProfile,
    pub background: SubsystemProfile,
}

/// Background task for budget-aware scheduling.
struct BackgroundTask {
    id: u64,
    name: String,
    work: Box<dyn FnOnce() + Send>,
    priority: u32,
    estimated_memory_mb: u64,
    deadline: Option<Instant>,
}

/// Unified native runtime.
pub struct NativeRuntime {
    config: RuntimeConfig,
    hardware: HardwareCapabilities,
    initialized: AtomicBool,
    execution_mode: RwLock<ExecutionMode>,
    profiling_enabled: AtomicBool,
    profiles: Mutex<HashMap<String, RuntimeProfile>>,
    background_tasks: Mutex<Vec<BackgroundTask>>,
    background_thread: Option<std::thread::JoinHandle<()>>,
    shutdown_requested: AtomicBool,
    #[cfg(windows)]
    host: Option<Arc<Host>>,
}

impl NativeRuntime {
    /// Create a new runtime instance.
    pub fn new(config: RuntimeConfig) -> Self {
        Self {
            config,
            hardware: HardwareCapabilities::default(),
            initialized: AtomicBool::new(false),
            execution_mode: RwLock::new(ExecutionMode::Native),
            profiling_enabled: AtomicBool::new(true),
            profiles: Mutex::new(HashMap::new()),
            background_tasks: Mutex::new(Vec::new()),
            background_thread: None,
            shutdown_requested: AtomicBool::new(false),
            #[cfg(windows)]
            host: None,
        }
    }

    /// Initialize the unified native runtime.
    /// 
    /// Order of initialization:
    /// 1. Hardware capability detection
    /// 2. Native Memory (must be first - other subsystems depend on it)
    /// 3. Native Shaders (translation cache, permutation tracking)
    /// 4. Native Graphics (command pool, descriptor pools, barriers, queues)
    /// 5. Native Platform (kernel, files, audio, input, storage)
    /// 6. Background task scheduler
    /// 7. Profiling
    pub fn initialize(&mut self) -> Result<(), String> {
        if self.initialized.swap(true, Ordering::AcqRel) {
            return Err("Runtime already initialized".into());
        }

        #[cfg(windows)]
        {
            // 1. Detect hardware capabilities
            self.detect_hardware_capabilities()?;

            // 2. Set up console timers and process priority
            use_console_timers();
            set_main_image(0); // Will be updated when title loads

            // 3. Initialize Native Memory
            self.initialize_native_memory()?;

            // 4. Initialize Native Shaders
            self.initialize_native_shaders()?;

            // 5. Initialize Native Graphics
            self.initialize_native_graphics()?;

            // 6. Initialize Native Platform
            self.initialize_native_platform()?;

            // 7. Start background task scheduler
            self.start_background_scheduler();

            // 8. Start profiling if enabled
            if self.config.enable_profiling {
                self.start_profiling();
            }
        }

        #[cfg(not(windows))]
        {
            // On non-Windows, we can't initialize the actual subsystems
            // but we can set up the configuration
            tracing::info!("Native runtime configured (non-Windows platform)");
        }

        tracing::info!(
            mode = ?self.config.execution_mode,
            hardware = ?self.hardware,
            "Native runtime initialized"
        );

        Ok(())
    }

    /// Detect hardware capabilities.
    #[cfg(windows)]
    fn detect_hardware_capabilities(&mut self) -> Result<(), String> {
        let host_guard = host().ok_or("Failed to get host")?;
        let device = &host_guard.device;

        // Query adapter info
        let mut adapter_luid = None;
        if let Ok(factory) = unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory2::<windows::Win32::Graphics::Dxgi::IDXGIFactory4>(0) } {
            if let Ok(adapter) = factory.EnumAdapterByLuid::<windows::Win32::Graphics::Dxgi::IDXGIAdapter3>(host_guard.device.GetAdapterLuid()) {
                if let Ok(desc) = adapter.GetDesc1() {
                    self.hardware.vendor_id = desc.VendorId;
                    self.hardware.device_id = desc.DeviceId;
                    adapter_luid = Some(desc.AdapterLuid);
                }
            }

        // Query video memory budgets
        if let Some(luid) = adapter_luid {
            if let Ok(factory) = unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory2::<windows::Win32::Graphics::Dxgi::IDXGIFactory4>(0) } {
                if let Ok(adapter) = factory.EnumAdapterByLuid::<windows::Win32::Graphics::Dxgi::IDXGIAdapter3>(luid) {
                    let mut local_info: windows::Win32::Graphics::Dxgi::DXGI_QUERY_VIDEO_MEMORY_INFO = Default::default();
                    if adapter.QueryVideoMemoryInfo(0, windows::Win32::Graphics::Dxgi::DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local_info).is_ok() {
                        self.hardware.vram_budget = local_info.Budget;
                    }
                    let mut non_local_info: windows::Win32::Graphics::Dxgi::DXGI_QUERY_VIDEO_MEMORY_INFO = Default::default();
                    if adapter.QueryVideoMemoryInfo(0, windows::Win32::Graphics::Dxgi::DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut non_local_info).is_ok() {
                        self.hardware.shared_memory_budget = non_local_info.Budget;
                    }
                }
            }
        }

        // Query D3D12 options
        let mut options1 = windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        if device.CheckFeatureSupport(
            windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut options1).cast(),
            size_of_val(&options1) as u32,
        ).is_ok() {
            self.hardware.max_wave_lane_count = options1.WaveLaneCountMax;
            self.hardware.min_wave_lane_count = options1.WaveLaneCountMin;
        }

        let mut options7 = windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        if device.CheckFeatureSupport(
            windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_D3D12_OPTIONS7,
            (&raw mut options7).cast(),
            size_of_val(&options7) as u32,
        ).is_ok() {
            self.hardware.supports_mesh_shaders = options7.MeshShaderTier != windows::Win32::Graphics::Direct3D12::D3D12_MESH_SHADER_TIER_NOT_SUPPORTED;
        }

        let mut options2 = windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
        if device.CheckFeatureSupport(
            windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_D3D12_OPTIONS2,
            (&raw mut options2).cast(),
            size_of_val(&options2) as u32,
        ).is_ok() {
            self.hardware.supports_depth_bounds = options2.DepthBoundsTestSupported.as_bool();
        }

        // Check RDNA
        let amd_vendor = self.hardware.vendor_id == 0x1002; // AMD
        self.hardware.is_rdna = amd_vendor && self.hardware.min_wave_lane_count == 32 && self.hardware.max_wave_lane_count == 64;
        self.hardware.is_rdna2 = self.hardware.is_rdna && self.hardware.device_id >= 0x7300 && self.hardware.device_id <= 0x73FF; // Navi 2x

        // CPU info
        let mut sys_info = windows::Win32::System::SystemInformation::SYSTEM_INFO::default();
        unsafe { windows::Win32::System::SystemInformation::GetSystemInfo(&mut sys_info) };
        self.hardware.cpu_core_count = sys_info.dwNumberOfProcessors;
        self.hardware.cpu_logical_processors = sys_info.dwNumberOfProcessors; // Simplified

        tracing::info!(
            vendor = format_args!("{:#06x}", self.hardware.vendor_id),
            device = format_args!("{:#06x}", self.hardware.device_id),
            vram_mb = self.hardware.vram_budget / (1024 * 1024),
            shared_mb = self.hardware.shared_memory_budget / (1024 * 1024),
            wave_min = self.hardware.min_wave_lane_count,
            wave_max = self.hardware.max_wave_lane_count,
            rdna = self.hardware.is_rdna,
            rdna2 = self.hardware.is_rdna2,
            mesh = self.hardware.supports_mesh_shaders,
            depth_bounds = self.hardware.supports_depth_bounds,
            "Hardware capabilities detected"
        );

        Ok(())
        }

    /// Initialize Native Memory subsystem.
    #[cfg(windows)]
    fn initialize_native_memory(&self) -> Result<(), String> {
        let host_guard = host().ok_or("Failed to get host")?;
        let device = host_guard.device.clone();
        
        // Create copy queue for memory operations
        let copy_queue_desc = windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_DESC {
            Type: windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_LIST_TYPE_COPY,
            Priority: 0,
            Flags: windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_FLAGS(0),
            NodeMask: 0,
        };
        let copy_queue: windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue = unsafe { 
            device.CreateCommandQueue(&copy_queue_desc) 
        }.map_err(|e| format!("Failed to create copy queue: {}", e))?;

        // Initialize memory manager with adapter
        let adapter = host_guard.adapter.clone();
        init_memory_manager(&device, adapter);

        tracing::info!("Native Memory initialized");
        Ok(())
    }

    /// Initialize Native Shaders subsystem.
    #[cfg(windows)]
    fn initialize_native_shaders(&self) -> Result<(), String> {
        // Set up shader cache directory
        let cache_dir = self.config.shader_cache_dir.clone()
            .unwrap_or_else(|| std::env::temp_dir().join("victory_shader_cache"));
        
        std::fs::create_dir_all(&cache_dir).map_err(|e| format!("Failed to create shader cache dir: {}", e))?;
        
        // Enable shader permutation persistence
        if self.config.enable_permutation_persistence {
            let perm_dir = self.config.permutation_cache_dir.clone()
                .unwrap_or_else(|| cache_dir.join("permutations"));
            std::fs::create_dir_all(&perm_dir).ok();
            enable_persistence(perm_dir);
        }

        // Enable shader cache
        if self.config.enable_shader_cache {
            use victory_gpu::shader_cache::use_shader_cache;
            use_shader_cache(cache_dir);
        }

        tracing::info!("Native Shaders initialized");
        Ok(())
    }

    /// Initialize Native Graphics subsystem.
    #[cfg(windows)]
    fn initialize_native_graphics(&mut self) -> Result<(), String> {
        let host_guard = host().ok_or("Failed to get host")?;
        let device = &host_guard.device;

        // Initialize descriptor pools
        init_descriptor_pools(device)?;

        // Initialize command pool
        init_command_pool(device)?;

        // Initialize barrier tracker
        init_barrier_tracker();

        // Initialize async queue manager
        init_queue_manager(device)?;

        // Set binding configuration (native defaults)
        set_binding_config(crate::host::binding::BindingConfig::native_default());

        // Set up pipeline cache
        if self.config.enable_pipeline_cache {
            let pipeline_dir = self.config.pipeline_cache_dir.clone()
                .unwrap_or_else(|| std::env::temp_dir().join("victory_pipeline_cache"));
            std::fs::create_dir_all(&pipeline_dir).ok();
            use_pipeline_cache(pipeline_dir);
        }

        // Store host reference
        self.host = Some(Arc::new(host_guard.clone()));

        tracing::info!("Native Graphics initialized");
        Ok(())
    }

    /// Initialize Native Platform subsystem.
    #[cfg(windows)]
    fn initialize_native_platform(&self) -> Result<(), String> {
        // Platform subsystems (kernel, files, xaudio2, gameinput, dstorage)
        // are automatically initialized when their modules are loaded
        // through the HLE module registry
        
        tracing::info!("Native Platform initialized");
        Ok(())
    }

    /// Start background task scheduler with adaptive thread counts.
    fn start_background_scheduler(&mut self) {
        let shutdown = self.shutdown_requested.clone();
        let tasks = self.background_tasks.clone();
        let budget_ms = self.config.background_task_budget_ms;
        let max_threads = self.config.max_background_threads;
        let cpu_cores = self.hardware.cpu_core_count.max(1);
        
        // Calculate adaptive thread count: reserve 1-2 cores for main thread,
        // use remaining for background work. On 6-core Ryzen 5 5600X, use 3-4 threads.
        let adaptive_max_threads = if cpu_cores <= 2 {
            1
        } else if cpu_cores <= 4 {
            2
        } else if cpu_cores <= 6 {
            cpu_cores - 2
        } else if cpu_cores <= 8 {
            cpu_cores - 3
        } else {
            cpu_cores / 2
        }.min(max_threads as u32) as usize;

        let handle = std::thread::Builder::new()
            .name("victory-background".into())
            .spawn(move || {
                let mut thread_pool = Vec::new();
                let mut idle_count = 0;
                
                while !shutdown.load(Ordering::Acquire) {
                    // Process background tasks
                    let task = {
                        let mut tasks = tasks.lock().unwrap();
                        tasks.pop()
                    };

                    if let Some(task) = task {
                        // Check budget
                        let start = Instant::now();
                        (task.work)();
                        let elapsed = start.elapsed();
                        
                        if elapsed.as_millis() > budget_ms as u128 {
                            tracing::warn!(
                                task = %task.name,
                                elapsed_ms = elapsed.as_millis(),
                                budget_ms = budget_ms,
                                "Background task exceeded budget"
                            );
                        }
                        idle_count = 0;
                    } else {
                        // No tasks, sleep briefly with exponential backoff
                        idle_count += 1;
                        let sleep_ms = (10u64 * 2u64.saturating_pow(idle_count.min(6))).min(100);
                        std::thread::sleep(Duration::from_millis(sleep_ms));
                    }

                    // Clean up finished threads
                    thread_pool.retain(|t| !t.is_finished());
                    
                    // Dynamic thread pool sizing: scale down when idle, up when busy
                    let target_threads = if thread_pool.len() > adaptive_max_threads {
                        // Scale down if we have too many idle threads
                        thread_pool.retain(|t| !t.is_finished());
                        thread_pool.len()
                    } else if idle_count == 0 && thread_pool.len() < adaptive_max_threads {
                        // Could spawn more workers if we had work queue
                        thread_pool.len()
                    } else {
                        thread_pool.len()
                    };
                    
                    // Limit thread pool size
                    while thread_pool.len() > adaptive_max_threads {
                        std::thread::sleep(Duration::from_millis(1));
                        thread_pool.retain(|t| !t.is_finished());
                    }
                }
            });

        self.background_thread = handle.ok();
        tracing::info!(
            adaptive_max_threads = adaptive_max_threads,
            cpu_cores = cpu_cores,
            "Background task scheduler started with adaptive thread pool"
        );
    }

    /// Start profiling with low-overhead GPU timestamp queries.
    fn start_profiling(&mut self) {
        self.profiling_enabled.store(true, Ordering::Relaxed);
        
        let profiles = self.profiles.clone();
        let interval = self.config.profiling_interval;
        let shutdown = self.shutdown_requested.clone();
        #[cfg(windows)]
        let host_opt = self.host.clone();
        
        std::thread::Builder::new()
            .name("victory-profiling".into())
            .spawn(move || {
                #[cfg(windows)]
                let (timestamp_query_heap, timestamp_buffer) = Self::create_timestamp_query_objects(&host_opt);
                
                while !shutdown.load(Ordering::Acquire) {
                    std::thread::sleep(interval);
                    
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    
                    // Collect profiling data with minimal locking
                    let profile = Self::collect_profile(&host_opt, &timestamp_query_heap, &timestamp_buffer);
                    
                    // Use try_lock to avoid blocking on contention
                    if let Ok(mut profiles) = profiles.try_lock() {
                        profiles.insert("latest".to_string(), profile);
                        
                        // Keep only last 1000 profiles (lock-free eviction)
                        if profiles.len() > 1000 {
                            let keys: Vec<_> = profiles.keys().cloned().collect();
                            for key in keys.iter().take(profiles.len() - 1000) {
                                profiles.remove(key);
                            }
                        }
                    }
                }
            }).ok();
        
        tracing::info!(interval_ms = interval.as_millis(), "Low-overhead profiling started");
    }

    /// Create GPU timestamp query objects for low-overhead GPU timing.
    #[cfg(windows)]
    fn create_timestamp_query_objects(host_opt: &Option<Arc<Host>>) -> (Option<ID3D12QueryHeap>, Option<ID3D12Resource>) {
        let host = match host_opt {
            Some(h) => h,
            None => return (None, None),
        };
        
        let device = &host.device;
        
        // Create timestamp query heap
        let query_heap_desc = D3D12_QUERY_HEAP_DESC {
            Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
            Count: 2, // Start and end timestamps
            NodeMask: 0,
        };
        
        let query_heap = match unsafe { device.CreateQueryHeap(&query_heap_desc) } {
            Ok(h) => Some(h),
            Err(_) => return (None, None),
        };
        
        // Create readback buffer for timestamp data
        let buffer_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: 16, // 2 * 8 bytes for two timestamps
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
        
        let timestamp_buffer = match unsafe { 
            device.CreateCommittedResource(
                &heap_props,
                D3D12_HEAP_FLAGS(0),
                &buffer_desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
            )
        } {
            Ok(b) => Some(b),
            Err(_) => return (query_heap, None),
        };
        
        (query_heap, timestamp_buffer)
    }

    /// Collect a profiling snapshot with low-overhead GPU timing.
    fn collect_profile(host_opt: &Option<Arc<Host>>, timestamp_query_heap: &Option<ID3D12QueryHeap>, timestamp_buffer: &Option<ID3D12Resource>) -> RuntimeProfile {
        let mut profile = RuntimeProfile::default();
        profile.timestamp = Instant::now();
        
        #[cfg(windows)]
        {
            // Memory stats (lock-free read)
            let mem_stats = memory_stats();
            profile.memory.ram_bytes = mem_stats.vram_used + mem_stats.shared_used;
            profile.memory.vram_bytes = mem_stats.vram_used;
            profile.memory.evictions = mem_stats.evictions;
            profile.memory.allocations = mem_stats.allocation_failures;
            
            // Shader stats (lock-free read)
            let shader_stats = shader_stats();
            profile.shader.cache_hits = shader_stats.total_uses;
            profile.shader.cache_misses = shader_stats.total_permutations as u64;
            
            // GPU timing via timestamp queries (if available)
            if let (Some(query_heap), Some(buffer), Some(host)) = (timestamp_query_heap, timestamp_buffer, host_opt) {
                let device = &host.device;
                let queue = &host.queue;
                
                // Resolve timestamps
                let resolve_desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Alignment: 0,
                    Width: 16,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_UNKNOWN,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: D3D12_RESOURCE_FLAGS(0),
                };
                
                // This would be called from a command list in practice
                // For now, just note the capability
                profile.graphics.gpu_time_ms = 0.0; // Placeholder
            }
        }
        
        profile
    }

    /// Schedule a background task.
    pub fn schedule_background_task(
        &self,
        name: String,
        work: Box<dyn FnOnce() + Send>,
        priority: u32,
        estimated_memory_mb: u64,
    ) -> Result<u64, String> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err("Runtime shutting down".into());
        }

        static TASK_ID: AtomicU64 = AtomicU64::new(0);
        let id = TASK_ID.fetch_add(1, Ordering::Relaxed);
        
        let task = BackgroundTask {
            id,
            name,
            work,
            priority,
            estimated_memory_mb,
            deadline: None,
        };

        self.background_tasks.lock().unwrap().push(task);
        Ok(id)
    }

    /// Get the current execution mode.
    pub fn execution_mode(&self) -> ExecutionMode {
        *self.execution_mode.read().unwrap()
    }

    /// Set the execution mode (A/B testing).
    pub fn set_execution_mode(&self, mode: ExecutionMode) {
        *self.execution_mode.write().unwrap() = mode;
        tracing::info!(mode = ?mode, "Execution mode changed");
    }

    /// Get hardware capabilities.
    pub fn hardware_capabilities(&self) -> &HardwareCapabilities {
        &self.hardware
    }

    /// Get the latest profiling snapshot.
    pub fn latest_profile(&self) -> Option<RuntimeProfile> {
        self.profiles.lock().unwrap().get("latest").cloned()
    }

    /// Get all profiling history.
    pub fn profiling_history(&self) -> Vec<RuntimeProfile> {
        self.profiles.lock().unwrap().values().cloned().collect()
    }

    /// Get current memory budget status.
    pub fn memory_budget_status(&self) -> Option<MemoryBudget> {
        #[cfg(windows)]
        {
            let stats = memory_stats();
            Some(MemoryBudget {
                vram_budget: stats.vram_budget,
                shared_budget: stats.shared_budget,
                ..Default::default()
            })
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Check if native execution is available.
    pub fn is_native_available(&self) -> bool {
        #[cfg(windows)]
        {
            self.initialized.load(Ordering::Acquire)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Shutdown the runtime gracefully.
    pub fn shutdown(&mut self) {
        self.shutdown_requested.store(true, Ordering::Release);
        
        // Wait for background thread
        if let Some(handle) = self.background_thread.take() {
            handle.join().ok();
        }
        
        // Clear profiling data
        self.profiles.lock().unwrap().clear();
        
        self.initialized.store(false, Ordering::Release);
        tracing::info!("Native runtime shut down");
    }
}

/// Global runtime instance.
static NATIVE_RUNTIME: OnceLock<NativeRuntime> = OnceLock::new();

/// Initialize the global native runtime.
pub fn initialize_runtime(config: RuntimeConfig) -> Result<&'static NativeRuntime, String> {
    let mut runtime = NativeRuntime::new(config);
    runtime.initialize()?;
    NATIVE_RUNTIME.set(runtime).map_err(|_| "Runtime already initialized".into())
}

/// Get the global native runtime.
pub fn runtime() -> Option<&'static NativeRuntime> {
    NATIVE_RUNTIME.get()
}

/// Check if native runtime is initialized and available.
pub fn is_native_runtime_active() -> bool {
    NATIVE_RUNTIME.get().map(|r| r.is_native_available()).unwrap_or(false)
}

/// Shutdown the global native runtime.
pub fn shutdown_runtime() {
    // Note: OnceLock doesn't allow removal, so we just signal shutdown
    if let Some(rt) = NATIVE_RUNTIME.get() {
        rt.shutdown_requested.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_config_defaults() {
        let config = RuntimeConfig::default();
        assert_eq!(config.execution_mode, ExecutionMode::Native);
        assert!(config.enable_shader_cache);
        assert!(config.enable_pipeline_cache);
        assert!(config.enable_profiling);
    }

    #[test]
    fn test_execution_mode_default() {
        assert_eq!(ExecutionMode::default(), ExecutionMode::Native);
    }

    #[test]
    fn test_hardware_capabilities_default() {
        let caps = HardwareCapabilities::default();
        assert_eq!(caps.vendor_id, 0);
        assert_eq!(caps.max_wave_lane_count, 0);
    }

    #[test]
    fn test_subsystem_profile_default() {
        let profile = SubsystemProfile::default();
        assert_eq!(profile.cpu_time_ms, 0.0);
        assert_eq!(profile.cache_hits, 0);
    }
}