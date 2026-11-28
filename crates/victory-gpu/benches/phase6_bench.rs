//! Benchmarks for Phase 6 Native Optimization.

mod phase6_bench {
    use std::hint::black_box;
    use std::sync::Arc;
    use std::time::Instant;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    use victory_gpu::host::{host, Host};
    use victory_gpu::host::memory_mgr::{memory_stats, check_memory_budget, memory_new_submission, track_gpu_resource, untrack_gpu_resource, MemoryPoolType, ResourcePriority};
    use victory_gpu::host::command_pool::command_pool;
    use victory_gpu::host::binding::{binding_config, optimal_binding_strategy, rdna2_binding_optimized};
    use victory_gpu::host::queue::{queue_manager, execute_on_queue, wait_on_queue, sync_queues, QueueType};
    use victory_gpu::host::barrier::{barrier_tracker, barrier_new_submission, barrier_register, barrier_use, barrier_stats, ResourceState};
    use victory_gpu::shader_perm::{stats as shader_stats, enable_persistence};
    use victory_gpu::pipeline::library::{use_pipeline_cache, folder as pipeline_folder};

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

    fn create_readback_buffer(device: &ID3D12Device, size: u64) -> ID3D12Resource {
        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
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
            Flags: D3D12_RESOURCE_FLAGS(0),
        };
        let resource: ID3D12Resource = unsafe { 
            device.CreateCommittedResource(
                &heap_props,
                D3D12_HEAP_FLAGS(0),
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
            )
        }.expect("Failed to create readback buffer");
        resource
    }

    fn create_barriers_for_test(
        device: &ID3D12Device,
        copy_queue: &ID3D12CommandQueue,
        copy_allocator: &ID3D12CommandAllocator,
        resource: &ID3D12Resource,
        from_state: ResourceState,
        to_state: ResourceState,
    ) -> u32 {
        let list: ID3D12GraphicsCommandList = unsafe {
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, copy_allocator, None)
        }.expect("Failed to create command list");
        
        let count = victory_gpu::host::barrier::emit_tracked_barriers(
            &list,
            &[(*resource, from_state, 0xFFFFFFFF, (*resource, to_state, 0xFFFFFFFF))]
        );
        
        unsafe { list.Close() }.unwrap();
        let lists = [Some(list.cast().unwrap())];
        unsafe { copy_queue.ExecuteCommandLists(&lists) };
        
        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAGS(0)) }.unwrap();
        let event = unsafe { windows::Win32::Foundation::CreateEventW(None, false, false, None) }.unwrap();
        unsafe { copy_queue.Signal(&fence, 1) }.unwrap();
        unsafe { fence.SetEventOnCompletion(1, event) }.unwrap();
        unsafe { windows::Win32::Foundation::WaitForSingleObject(event, 5000) };
        
        count
    }

    /// Benchmark cold memory allocation (first allocation).
    fn bench_cold_memory_allocation() {
        let (device, queue) = create_test_device();
        
        const ITERATIONS: usize = 100;
        let mut times = Vec::with_capacity(ITERATIONS);

        for i in 0..ITERATIONS {
            let size = 1024 * 1024; // 1 MB
            let start = Instant::now();
            let buffer = create_uav_buffer(&device, size);
            track_gpu_resource(&buffer, size, true, victory_gpu::host::memory_mgr::MemoryPoolType::Streaming, victory_gpu::host::memory_mgr::ResourcePriority::Normal);
            untrack_gpu_resource(&buffer);
            times.push(start.elapsed());
        }

        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("cold_memory_allocation: median {:?} over {} iterations", median, ITERATIONS);
        
        let stats = memory_stats();
        println!("  VRAM used after: {} MB", stats.vram_used / (1024 * 1024));
    }

    /// Benchmark warm memory allocation (reusing allocator).
    fn bench_warm_memory_allocation() {
        let (device, queue) = create_test_device();
        
        // Pre-populate
        let mut buffers = Vec::new();
        for _ in 0..10 {
            let size = 1024 * 1024;
            let buffer = create_uav_buffer(&device, size);
            track_gpu_resource(&buffer, size, true, victory_gpu::host::memory_mgr::MemoryPoolType::Streaming, victory_gpu::host::memory_mgr::ResourcePriority::Normal);
            buffers.push(buffer);
        }
        
        const ITERATIONS: usize = 1000;
        let mut times = Vec::with_capacity(ITERATIONS);

        for _ in 0..ITERATIONS {
            let size = 1024 * 1024;
            let start = Instant::now();
            let buffer = create_uav_buffer(&device, size);
            track_gpu_resource(&buffer, size, true, victory_gpu::host::memory_mgr::MemoryPoolType::Streaming, victory_gpu::host::memory_mgr::ResourcePriority::Normal);
            untrack_gpu_resource(&buffer);
            times.push(start.elapsed());
        }

        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("warm_memory_allocation: median {:?} over {} iterations", median, ITERATIONS);
    }

    /// Benchmark command pool allocator reuse.
    fn bench_command_pool_reuse() {
        let host = host().expect("Failed to get host");
        let pool = command_pool();
        
        // Warm up
        for _ in 0..10 {
            let handle = pool.get_direct_allocator().expect("Failed to get allocator");
            let list = handle.create_list().expect("Failed to create list");
            unsafe { list.Close() }.unwrap();
            let queue = host.queue.clone();
            let fence_value = handle.submit(queue).expect("Failed to submit");
            pool.wait_for_fence(fence_value).expect("Failed to wait");
        }
        
        const ITERATIONS: usize = 10000;
        let mut times = Vec::with_capacity(ITERATIONS);

        for _ in 0..ITERATIONS {
            let start = Instant::now();
            let handle = pool.get_direct_allocator().expect("Failed to get allocator");
            let list = handle.create_list().expect("Failed to create list");
            unsafe { list.Close() }.unwrap();
            let queue = host.queue.clone();
            let fence_value = handle.submit(queue).expect("Failed to submit");
            pool.wait_for_fence(fence_value).expect("Failed to wait");
            times.push(start.elapsed());
        }

        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("command_pool_reuse: median {:?} over {} iterations", median, ITERATIONS);
        
        let stats = pool.stats();
        println!("  allocators created: {}, reused: {}", stats.direct_allocators_created, stats.direct_allocators_reused);
    }

    /// Benchmark barrier elimination.
    fn bench_barrier_elimination() {
        let (device, queue) = create_test_device();
        let (copy_queue, copy_allocator) = create_copy_queue(&device);
        
        const ITERATIONS: usize = 1000;
        let mut times = Vec::with_capacity(ITERATIONS);
        
        for i in 0..ITERATIONS {
            let buffer = create_uav_buffer(&device, 64 * 1024);
            barrier_register(&buffer, ResourceState::UnorderedAccess);
            
            let start = Instant::now();
            let _count = create_barriers_for_test(&device, &copy_queue, &copy_allocator, &buffer, ResourceState::UnorderedAccess, ResourceState::CopySource);
            times.push(start.elapsed());
            
            untrack_gpu_resource(&buffer);
        }

        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("barrier_elimination: median {:?} over {} iterations", median, ITERATIONS);
        
        let stats = barrier_stats();
        println!("  barriers emitted: {}, eliminated: {}", stats.barriers_emitted, stats.barriers_eliminated);
    }

    /// Benchmark cross-queue synchronization.
    fn bench_cross_queue_sync() {
        let qm = queue_manager();
        let (device, queue) = create_test_device();
        
        const ITERATIONS: usize = 100;
        let mut times = Vec::with_capacity(ITERATIONS);
        
        for _ in 0..ITERATIONS {
            // Create command list
            let handle = host().unwrap().command_pool().get_direct_allocator().unwrap();
            let list = handle.create_list().unwrap();
            unsafe { list.Close() }.unwrap();
            
            let start = Instant::now();
            let fence_value = execute_on_queue(QueueType::Graphics, &[list.cast().unwrap()]).unwrap();
            let sync_value = sync_queues(QueueType::Graphics, QueueType::Copy).unwrap();
            wait_on_queue(QueueType::Copy, sync_value).unwrap();
            times.push(start.elapsed());
        }
        
        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("cross_queue_sync: median {:?} over {} iterations", median, ITERATIONS);
        
        let stats = queue_manager().stats();
        println!("  graphics submissions: {}, copy submissions: {}, cross-queue syncs: {}", 
            stats.graphics_submissions, stats.copy_submissions, stats.cross_queue_syncs);
    }

    /// Benchmark PSO creation.
    fn bench_pso_creation() {
        let host = Host::new(false).expect("Failed to create host");
        
        let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
        let ps = victory_shader::emit::host::stub_ps();
        
        const ITERATIONS: usize = 100;
        let mut times = Vec::with_capacity(ITERATIONS);
        
        for _ in 0..ITERATIONS {
            let start = Instant::now();
            let _pipeline = host.pipeline(&vs, &ps).expect("Failed to create pipeline");
            times.push(start.elapsed());
        }
        
        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("pso_creation: median {:?} over {} iterations", median, ITERATIONS);
        
        let stats = shader_stats();
        println!("  permutations: {}, PSOs: {}", stats.total_permutations, stats.permutations_with_pso);
    }

    /// Benchmark binding strategy selection.
    fn bench_binding_strategy() {
        let config = binding_config();
        
        const ITERATIONS: usize = 100000;
        let mut times = Vec::with_capacity(ITERATIONS);
        
        for _ in 0..ITERATIONS {
            let start = Instant::now();
            let _strategy = optimal_binding_strategy(false, false);
            let _rdna2 = rdna2_binding_optimized();
            times.push(start.elapsed());
        }
        
        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("binding_strategy: median {:?} over {} iterations", median, ITERATIONS);
        
        println!("  RDNA2 optimized: {}, prefer_direct: {}, bounds_check: {}", 
            rdna2_binding_optimized(), config.prefer_direct, config.enable_bounds_check);
    }

    pub fn run_all() {
        println!("=== Phase 6 Native Optimization Benchmarks ===\n");
        
        bench_cold_memory_allocation();
        println!();
        
        bench_warm_memory_allocation();
        println!();
        
        bench_command_pool_reuse();
        println!();
        
        bench_barrier_elimination();
        println!();
        
        bench_cross_queue_sync();
        println!();
        
        bench_pso_creation();
        println!();
        
        bench_binding_strategy();
        println!();
        
        println!("=== All Benchmarks Complete ===");
    }
}

fn main() {
    phase6_bench::run_all();
}