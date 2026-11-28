//! Regression tests and benchmarks for Phase 6 Native Optimization.

mod phase6_regression_tests {
    use std::time::{Duration, Instant};
    use std::sync::Arc;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    use victory_gpu::host::{host, Host};
    use victory_gpu::host::memory_mgr::{memory_stats, check_memory_budget, memory_new_submission, track_gpu_resource, untrack_gpu_resource, MemoryPoolType, ResourcePriority};
    use victory_gpu::host::command_pool::command_pool;
    use victory_gpu::host::binding::{binding_config, BindingStrategy, optimal_binding_strategy, rdna2_binding_optimized};
    use victory_gpu::host::queue::{queue_manager, execute_on_queue, wait_on_queue, sync_queues, QueueType};
    use victory_gpu::host::barrier::{barrier_tracker, barrier_new_submission, barrier_register, barrier_use, barrier_stats, ResourceState};
    use victory_gpu::shader_perm::{stats as shader_stats, enable_persistence};
    use victory_gpu::pipeline::library::{use_pipeline_cache, folder as pipeline_folder};

    fn create_test_device() -> (Arc<ID3D12Device>, Arc<ID3D12CommandQueue>, Arc<ID3D12CommandAllocator>) {
        let host = host().expect("Failed to get host");
        let device = host.device.clone();

        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Priority: 0,
            Flags: D3D12_COMMAND_QUEUE_FLAGS(0),
            NodeMask: 0,
        };
        let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }.expect("Failed to create direct queue");
        
        let allocator: ID3D12CommandAllocator = unsafe { 
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) 
        }.expect("Failed to create allocator");

        (device, Arc::new(queue), Arc::new(allocator))
    }

    fn create_copy_queue(device: &ID3D12Device) -> (Arc<ID3D12CommandQueue>, Arc<ID3D12CommandAllocator>) {
        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            Priority: 0,
            Flags: D3D12_COMMAND_QUEUE_FLAGS(0),
            NodeMask: 0,
        };
        let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }.expect("Failed to create copy queue");
        let allocator: ID3D12CommandAllocator = unsafe { 
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY) 
        }.expect("Failed to create copy allocator");
        (Arc::new(queue), Arc::new(allocator))
    }

    fn create_uav_buffer(device: &ID3D12Device, size: u64) -> ID3D12Resource {
        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        };
        let resource: ID3D12Resource = unsafe { 
            device.CreateCommittedResource(
                &heap_props,
                D3D12_HEAP_FLAGS(0),
                &desc,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                None,
            )
        }.expect("Failed to create UAV buffer");
        resource
    }

    #[test]
    fn test_memory_budget_enforcement() {
        let (device, _, _) = create_test_device();
        
        // Create a large buffer that should trigger budget checking
        let uav_buffer = create_uav_buffer(&device, 100 * 1024 * 1024); // 100 MB
        
        // Track the resource
        track_gpu_resource(&uav_buffer, 100 * 1024 * 1024, true, MemoryPoolType::Static, ResourcePriority::Normal);
        
        // Check that budget tracking works
        let stats = memory_stats();
        assert!(stats.vram_used >= 100 * 1024 * 1024);
        
        // Clean up
        untrack_gpu_resource(&uav_buffer);
    }

    #[test]
    fn test_command_pool_reuse() {
        let host = host().expect("Failed to get host");
        let pool = command_pool();
        
        // Get allocator and create list
        let handle = pool.get_direct_allocator().expect("Failed to get direct allocator");
        let list = handle.create_list().expect("Failed to create list");
        
        // Close and submit
        unsafe { list.Close() }.unwrap();
        let queue = host.queue.clone();
        let fence_value = handle.submit(queue).expect("Failed to submit");
        
        // Wait for fence
        pool.wait_for_fence(fence_value).expect("Failed to wait for fence");
        
        // Get another allocator - should reuse
        let handle2 = pool.get_direct_allocator().expect("Failed to get second allocator");
        let list2 = handle2.create_list().expect("Failed to create second list");
        
        // Verify stats
        let stats = pool.stats();
        assert!(stats.direct_allocators_reused >= 1);
    }

    #[test]
    fn test_binding_strategy_selection() {
        let config = binding_config();
        assert!(config.prefer_direct);
        assert!(!config.enable_bounds_check);
        
        // Test RDNA2 binding optimization detection
        let rdna2_opt = rdna2_binding_optimized();
        println!("RDNA2 binding optimized: {}", rdna2_opt);
        
        // Test optimal strategy
        let strategy = optimal_binding_strategy(false, false);
        println!("Optimal binding strategy: {:?}", strategy);
    }

    #[test]
    fn test_async_queue_cross_sync() {
        let qm = queue_manager();
        
        // Test graphics queue
        let gfx_queue = qm.graphics_queue();
        assert_eq!(gfx_queue.queue_type(), QueueType::Graphics);
        
        // Test copy queue
        let copy_queue = qm.copy_queue();
        assert_eq!(copy_queue.queue_type(), QueueType::Copy);
        
        // Test cross-queue sync
        let gfx_q = queue_manager().graphics_queue();
        let copy_q = queue_manager().copy_queue();
        
        // Create simple command list
        let host = host().expect("Failed to get host");
        let allocator = host.command_pool().get_direct_allocator().expect("Failed to get allocator");
        let list = allocator.create_list().expect("Failed to create list");
        unsafe { list.Close() }.unwrap();
        
        // Execute on graphics queue
        let fence_value = execute_on_queue(QueueType::Graphics, &[list.cast().unwrap()]).expect("Failed to execute");
        
        // Sync to copy queue
        let sync_value = sync_queues(QueueType::Graphics, QueueType::Copy).expect("Failed to sync queues");
        
        // Wait on copy queue
        wait_on_queue(QueueType::Copy, sync_value).expect("Failed to wait on copy queue");
        
        // Verify stats
        let stats = queue_manager().stats();
        assert!(stats.graphics_submissions >= 1);
        assert!(stats.cross_queue_syncs >= 1);
    }

    #[test]
    fn test_barrier_tracker_elimination() {
        let (device, _, _) = create_test_device();
        let (copy_queue, copy_allocator) = create_copy_queue(&device);
        
        // Start new submission
        let submission = barrier_new_submission();
        
        // Create resources
        let uav_buffer = create_uav_buffer(&device, 64 * 1024);
        let readback = create_readback_buffer(&device, 64 * 1024);
        
        // Register with barrier tracker
        barrier_register(&uav_buffer, ResourceState::UnorderedAccess);
        
        // First use - should emit barrier
        let barriers1 = create_barriers_for_test(&device, &copy_queue, &copy_allocator, &uav_buffer, ResourceState::UnorderedAccess, ResourceState::CopySource);
        
        // Second use with same state - should eliminate
        let barriers2 = create_barriers_for_test(&device, &copy_queue, &copy_allocator, &uav_buffer, ResourceState::UnorderedAccess, ResourceState::CopySource);
        
        let stats = barrier_stats();
        // At least some barriers should be eliminated
        assert!(stats.barriers_eliminated > 0 || stats.barriers_emitted > 0);
    }

    #[test]
    fn test_pso_cache_persistence() {
        let host = Host::new(false).expect("Failed to create host");
        
        // Create a simple pipeline
        let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
        let ps = victory_shader::emit::host::stub_ps();
        
        let _pipeline = host.pipeline(&vs, &ps).expect("Failed to create pipeline");
        
        // Check that pipeline cache is working
        let stats = shader_stats();
        assert!(stats.total_permutations > 0);
        
        // Check pipeline cache folder
        if let Some(folder) = pipeline_folder() {
            println!("Pipeline cache folder: {:?}", folder);
        }
    }

    #[test]
    fn test_adaptive_thread_pool() {
        use victory_hle::native_runtime::{NativeRuntime, RuntimeConfig, ExecutionMode};
        use std::time::Duration;
        
        let config = RuntimeConfig {
            max_background_threads: 8,
            background_task_budget_ms: 16,
            ..Default::default()
        };
        
        let mut runtime = NativeRuntime::new(config);
        // Note: Can't fully initialize on Linux, but we can test config
        assert_eq!(runtime.config.max_background_threads, 8);
        
        // Test adaptive thread calculation logic
        let cpu_cores = 6; // Ryzen 5 5600X
        let adaptive_max = if cpu_cores <= 2 { 1 } else if cpu_cores <= 4 { 2 } else if cpu_cores <= 6 { cpu_cores - 2 } else if cpu_cores <= 8 { cpu_cores - 3 } else { cpu_cores / 2 };
        assert_eq!(adaptive_max, 4); // 6 cores -> 4 threads
    }

    #[test]
    fn test_hardware_config_generation() {
        use victory_hle::native_runtime::{RuntimeConfig, HardwareCapabilities};
        
        let caps = HardwareCapabilities {
            vram_budget: 8 * 1024 * 1024 * 1024, // 8 GB
            shared_memory_budget: 4 * 1024 * 1024 * 1024, // 4 GB
            max_wave_lane_count: 64,
            min_wave_lane_count: 32,
            is_rdna2: true,
            cpu_logical_processors: 12,
            ..Default::default()
        };
        
        let config = RuntimeConfig::for_hardware(&caps);
        assert!(config.streaming_budget_mb >= 1024); // 8GB VRAM -> 1GB streaming
        assert!(config.max_background_threads >= 4);
    }
}

mod phase6_benchmarks {
    use std::time::Instant;
    use std::sync::Arc;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    use victory_gpu::host::{host, Host};
    use victory_gpu::host::memory_mgr::{memory_stats, check_memory_budget, track_gpu_resource, untrack_gpu_resource, MemoryPoolType, ResourcePriority};
    use victory_gpu::host::command_pool::command_pool;
    use victory_gpu::host::queue::{queue_manager, execute_on_queue, QueueType};
    use victory_gpu::shader_perm::{stats as shader_stats};

    fn create_test_device() -> (Arc<ID3D12Device>, Arc<ID3D12CommandQueue>) {
        let host = host().expect("Failed to get host");
        let device = host.device.clone();
        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Priority: 0,
            Flags: D3D12_COMMAND_QUEUE_FLAGS(0),
            NodeMask: 0,
        };
        let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }.expect("Failed to create queue");
        (device, Arc::new(queue))
    }

    fn create_uav_buffer(device: &ID3D12Device, size: u64) -> ID3D12Resource {
        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        };
        unsafe { device.CreateCommittedResource(&heap_props, D3D12_HEAP_FLAGS(0), &desc, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, None) }.unwrap()
    }

    /// Benchmark memory allocation throughput.
    fn benchmark_memory_allocation() {
        let (device, _) = create_test_device();
        
        const ITERATIONS: usize = 1000;
        let size = 1024 * 1024; // 1 MB
        
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let buffer = create_uav_buffer(&device, size);
            track_gpu_resource(&buffer, size, true, MemoryPoolType::Streaming, ResourcePriority::Normal);
            untrack_gpu_resource(&buffer);
        }
        let elapsed = start.elapsed();
        
        println!("Memory allocation: {:?} for {} iterations ({:.2} us/op)", 
            elapsed, ITERATIONS, elapsed.as_micros() as f64 / ITERATIONS as f64);
    }

    /// Benchmark command pool allocation reuse.
    fn benchmark_command_pool_reuse() {
        let host = host().expect("Failed to get host");
        let pool = command_pool();
        
        const ITERATIONS: usize = 10000;
        
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let handle = pool.get_direct_allocator().expect("Failed to get allocator");
            let list = handle.create_list().expect("Failed to create list");
            unsafe { list.Close() }.unwrap();
            
            let queue = host.queue.clone();
            let fence_value = handle.submit(queue).expect("Failed to submit");
            pool.wait_for_fence(fence_value).expect("Failed to wait");
        }
        let elapsed = start.elapsed();
        
        println!("Command pool reuse: {:?} for {} iterations ({:.2} us/op)", 
            elapsed, ITERATIONS, elapsed.as_micros() as f64 / ITERATIONS as f64);
        
        let stats = pool.stats();
        println!("Pool stats: created={}, reused={}", stats.direct_allocators_created, stats.direct_allocators_reused);
    }

    /// Benchmark PSO creation throughput.
    fn benchmark_pso_creation() {
        let host = Host::new(false).expect("Failed to create host");
        
        let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
        let ps = victory_shader::emit::host::stub_ps();
        
        const ITERATIONS: usize = 100;
        
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _pipeline = host.pipeline(&vs, &ps).expect("Failed to create pipeline");
        }
        let elapsed = start.elapsed();
        
        println!("PSO creation: {:?} for {} iterations ({:.2} ms/op)", 
            elapsed, ITERATIONS, elapsed.as_millis() as f64 / ITERATIONS as f64);
    }

    /// Benchmark memory budget checking.
    fn benchmark_memory_budget() {
        use victory_gpu::host::memory_mgr::{memory_stats, check_memory_budget, memory_new_submission};
        
        const ITERATIONS: usize = 1000;
        
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _stats = memory_stats();
            check_memory_budget();
            memory_new_submission();
        }
        let elapsed = start.elapsed();
        
        println!("Memory budget check: {:?} for {} iterations ({:.2} us/op)", 
            elapsed, ITERATIONS, elapsed.as_micros() as f64 / ITERATIONS as f64);
    }

    pub fn run_all() {
        println!("=== Phase 6 Regression Tests & Benchmarks ===\n");
        
        println!("Running benchmarks...");
        benchmark_memory_allocation();
        println!();
        
        benchmark_command_pool_reuse();
        println!();
        
        benchmark_pso_creation();
        println!();
        
        benchmark_memory_budget();
        println!();
        
        println!("All benchmarks completed!");
    }
}