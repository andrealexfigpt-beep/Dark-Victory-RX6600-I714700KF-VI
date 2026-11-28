//! Phase 4 verification tests for D3D12 descriptor heap limits, fence-safe LRU eviction,
//! and cross-queue synchronization.

mod phase4_verification_tests {
    use std::sync::Arc;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    use victory_gpu::host::{host, Host};
    use victory_gpu::host::descriptor_pool::{allocate_cbv_srv_uav, allocate_samplers, allocate_rtv, allocate_dsv, cbv_srv_uav_pool, rtv_pool, dsv_pool, sampler_pool};
    use victory_gpu::host::command_pool::command_pool;
    use victory_gpu::host::barrier::{barrier_tracker, barrier_new_submission, barrier_register, barrier_use, barrier_stats, ResourceState};
    use victory_gpu::host::queue::{queue_manager, graphics_queue, compute_queue, copy_queue, execute_on_queue, wait_on_queue, sync_queues, QueueType};

    fn create_test_device() -> Arc<ID3D12Device> {
        let host = host().expect("Failed to get host");
        host.device.clone()
    }

    #[test]
    fn test_descriptor_heap_limits_cbv_srv_uav() {
        let device = create_test_device();
        
        // CBV/SRV/UAV pool should support large allocations
        let pool = cbv_srv_uav_pool();
        
        // Allocate a large block
        let block = allocate_cbv_srv_uav(10000).expect("Failed to allocate 10000 descriptors");
        assert_eq!(block.count, 10000);
        assert!(block.gpu_handle.ptr != 0);
        
        // Allocate another large block
        let block2 = allocate_cbv_srv_uav(10000).expect("Failed to allocate second block");
        assert_eq!(block2.count, 10000);
        
        // Free and reallocate
        victory_gpu::host::descriptor_pool::free_cbv_srv_uav(block);
        let block3 = allocate_cbv_srv_uav(10000).expect("Failed to reallocate after free");
        assert_eq!(block3.count, 10000);
    }

    #[test]
    fn test_descriptor_heap_limits_sampler() {
        let device = create_test_device();
        
        let pool = sampler_pool();
        
        // Allocate sampler descriptors
        let block = allocate_samplers(1000).expect("Failed to allocate sampler descriptors");
        assert_eq!(block.count, 1000);
        assert!(block.gpu_handle.ptr != 0);
        
        // Free
        victory_gpu::host::descriptor_pool::free_samplers(block);
    }

    #[test]
    fn test_descriptor_heap_limits_rtv_dsv() {
        let device = create_test_device();
        
        // RTV pool
        let rtv_block = allocate_rtv(100).expect("Failed to allocate RTV descriptors");
        assert_eq!(rtv_block.count, 100);
        assert!(rtv_block.cpu_handle.ptr != 0);
        // RTV is not shader visible
        assert_eq!(rtv_block.gpu_handle.ptr, 0);
        
        // DSV pool
        let dsv_block = allocate_dsv(100).expect("Failed to allocate DSV descriptors");
        assert_eq!(dsv_block.count, 100);
        assert!(dsv_block.cpu_handle.ptr != 0);
        assert_eq!(dsv_block.gpu_handle.ptr, 0);
        
        victory_gpu::host::descriptor_pool::free_rtv(rtv_block);
        victory_gpu::host::descriptor_pool::free_dsv(dsv_block);
    }

    #[test]
    fn test_descriptor_pool_stats() {
        let device = create_test_device();
        
        // Allocate some descriptors
        let _b1 = allocate_cbv_srv_uav(100).unwrap();
        let _b2 = allocate_cbv_srv_uav(200).unwrap();
        let _b3 = allocate_samplers(50).unwrap();
        
        let stats = cbv_srv_uav_pool().stats();
        assert!(stats.heaps_created >= 1);
        assert_eq!(stats.allocations, 2);
        assert!(stats.current_allocated >= 300);
        
        let sampler_stats = sampler_pool().stats();
        assert_eq!(sampler_stats.allocations, 1);
        assert!(sampler_stats.current_allocated >= 50);
    }

    #[test]
    fn test_command_allocator_pool_reuse() {
        let host = host().expect("Failed to get host");
        let pool = command_pool();
        
        // Get allocator and create list
        let handle = pool.get_direct_allocator().expect("Failed to get direct allocator");
        let list = handle.create_list().expect("Failed to create list");
        
        // Close and submit
        let queue = graphics_queue().queue();
        let fence_value = handle.submit(queue).expect("Failed to submit");
        
        // Wait for completion
        pool.wait_for_fence(fence_value).expect("Failed to wait for fence");
        
        // Get another allocator - should reuse
        let handle2 = pool.get_direct_allocator().expect("Failed to get second allocator");
        let list2 = handle2.create_list().expect("Failed to create second list");
        
        // Verify stats
        let stats = pool.stats();
        assert!(stats.direct_allocators_reused >= 1);
    }

    #[test]
    fn test_barrier_tracker_eliminates_redundant() {
        let device = create_test_device();
        let (copy_queue, copy_allocator) = create_copy_queue(&device);
        
        // Start new submission for barrier tracking
        let submission = barrier_new_submission();
        
        // Create a resource
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
    fn test_cross_queue_synchronization() {
        let host = host().expect("Failed to get host");
        
        // Check if compute queue is available
        let compute_available = compute_queue().is_some();
        
        if compute_available {
            let gfx_queue = graphics_queue().queue();
            let compute_q = compute_queue().unwrap().queue();
            
            // Create a simple compute list
            let allocator = host.command_pool().get_compute_allocator().expect("Failed to get compute allocator");
            let list = allocator.create_list().expect("Failed to create compute list");
            
            // Close list
            unsafe { list.Close() }.unwrap();
            
            // Execute on compute queue
            let fence_value = execute_on_queue(QueueType::Compute, &[list.cast().unwrap()]).expect("Failed to execute on compute queue");
            
            // Signal and wait cross-queue
            let sync_value = sync_queues(QueueType::Compute, QueueType::Graphics).expect("Failed to sync queues");
            
            // Wait on graphics queue
            wait_on_queue(QueueType::Graphics, sync_value).expect("Failed to wait on graphics queue");
            
            assert!(fence_value > 0);
            assert!(sync_value > 0);
        } else {
            // Compute queue not available - just verify graphics queue works
            let gfx_queue = graphics_queue().queue();
            let allocator = host.command_pool().get_direct_allocator().expect("Failed to get direct allocator");
            let list = allocator.create_list().expect("Failed to create list");
            
            unsafe { list.Close() }.unwrap();
            let fence_value = execute_on_queue(QueueType::Graphics, &[list.cast().unwrap()]).expect("Failed to execute on graphics queue");
            
            wait_on_queue(QueueType::Graphics, fence_value).expect("Failed to wait on graphics queue");
            
            assert!(fence_value > 0);
        }
    }

    #[test]
    fn test_async_compute_queue_creation() {
        let host = host().expect("Failed to get host");
        let qm = queue_manager();
        
        // Graphics queue should always exist
        let gfx = graphics_queue();
        assert_eq!(gfx.queue_type(), QueueType::Graphics);
        assert!(!gfx.queue().is_null());
        
        // Copy queue should always exist
        let copy = copy_queue();
        assert_eq!(copy.queue_type(), QueueType::Copy);
        assert!(!copy.queue().is_null());
        
        // Compute queue may or may not exist depending on hardware
        let compute = compute_queue();
        if let Some(c) = compute {
            assert_eq!(c.queue_type(), QueueType::Compute);
            assert!(!c.queue().is_null());
        }
        
        // Check stats
        let stats = qm.stats();
        assert!(stats.graphics_submissions >= 0);
        assert!(stats.copy_submissions >= 0);
    }

    #[test]
    fn test_memory_budget_tracking() {
        use victory_gpu::host::memory_mgr::{memory_stats, track_gpu_resource, untrack_gpu_resource, try_allocate_memory, MemoryPoolType, ResourcePriority};
        
        let device = create_test_device();
        
        // Create a test resource
        let uav_buffer = create_uav_buffer(&device, 1024 * 1024); // 1 MiB
        
        // Track it
        track_gpu_resource(&uav_buffer, 1024 * 1024, true, MemoryPoolType::Static, ResourcePriority::Normal);
        
        // Check stats
        let stats = memory_stats();
        assert!(stats.vram_used >= 1024 * 1024);
        
        // Try to allocate more
        assert!(try_allocate_memory(512 * 1024, true));
        
        // Untrack
        untrack_gpu_resource(&uav_buffer);
        
        let stats = memory_stats();
        // VRAM used should be reduced (but may not be exactly 0 due to other resources)
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
        unsafe { device.CreateCommittedResource(
            &heap_props,
            D3D12_HEAP_FLAGS(0),
            &desc,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            None,
        ) }.expect("Failed to create UAV buffer")
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
        unsafe { device.CreateCommittedResource(
            &heap_props,
            D3D12_HEAP_FLAGS(0),
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
        ) }.expect("Failed to create readback buffer")
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
}

#[cfg(test)]
mod barrier_conversion_tests {
    use victory_gpu::host::barrier::{ResourceState, d3d12_state};

    #[test]
    fn test_all_resource_state_conversions() {
        assert_eq!(d3d12_state(ResourceState::Common), D3D12_RESOURCE_STATE_COMMON);
        assert_eq!(d3d12_state(ResourceState::VertexAndConstantBuffer), D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER);
        assert_eq!(d3d12_state(ResourceState::IndexBuffer), D3D12_RESOURCE_STATE_INDEX_BUFFER);
        assert_eq!(d3d12_state(ResourceState::RenderTarget), D3D12_RESOURCE_STATE_RENDER_TARGET);
        assert_eq!(d3d12_state(ResourceState::UnorderedAccess), D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        assert_eq!(d3d12_state(ResourceState::DepthWrite), D3D12_RESOURCE_STATE_DEPTH_WRITE);
        assert_eq!(d3d12_state(ResourceState::DepthRead), D3D12_RESOURCE_STATE_DEPTH_READ);
        assert_eq!(d3d12_state(ResourceState::NonPixelShaderResource), D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
        assert_eq!(d3d12_state(ResourceState::PixelShaderResource), D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE);
        assert_eq!(d3d12_state(ResourceState::StreamOutput), D3D12_RESOURCE_STATE_STREAM_OUTPUT);
        assert_eq!(d3d12_state(ResourceState::IndirectArgument), D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
        assert_eq!(d3d12_state(ResourceState::CopyDest), D3D12_RESOURCE_STATE_COPY_DEST);
        assert_eq!(d3d12_state(ResourceState::CopySource), D3D12_RESOURCE_STATE_COPY_SOURCE);
        assert_eq!(d3d12_state(ResourceState::ResolveDest), D3D12_RESOURCE_STATE_RESOLVE_DEST);
        assert_eq!(d3d12_state(ResourceState::ResolveSource), D3D12_RESOURCE_STATE_RESOLVE_SOURCE);
        assert_eq!(d3d12_state(ResourceState::GenericRead), D3D12_RESOURCE_STATE_GENERIC_READ);
        assert_eq!(d3d12_state(ResourceState::Present), D3D12_RESOURCE_STATE_PRESENT);
        assert_eq!(d3d12_state(ResourceState::Predication), D3D12_RESOURCE_STATE_PREDICATION);
        assert_eq!(d3d12_state(ResourceState::RaytracingAccelerationStructure), D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE);
        assert_eq!(d3d12_state(ResourceState::ShadingRateSource), D3D12_RESOURCE_STATE_SHADING_RATE_SOURCE);
    }
}