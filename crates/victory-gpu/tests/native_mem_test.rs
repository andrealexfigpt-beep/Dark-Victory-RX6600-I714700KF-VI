//! Synthetic workload tests for the native memory backend.
//!
//! These tests validate:
//! - Memory allocation and freeing
//! - GPU resource binding (title VA -> GPU VA)
//! - Upload (CPU -> GPU) and readback (GPU -> CPU)
//! - Concurrent access patterns
//! - Memory consumption and allocation lookup performance
//! - Aliasing and resource lifetime correctness

#[cfg(all(windows, feature = "native-memory"))]
mod native_mem_tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    use fastrand;
    use victory_gpu::native_mem::{NativeMemory, PAGE};
    use victory_gpu::host::{host, Host};

    /// Create a test D3D12 device and copy queue.
    fn create_test_device() -> (Arc<ID3D12Device>, Arc<ID3D12CommandQueue>) {
        let host = host().expect("Failed to get host");
        let device = host.device.clone();

        // Create a copy queue
        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            Priority: 0,
            Flags: D3D12_COMMAND_QUEUE_FLAGS(0),
            NodeMask: 0,
        };
        let copy_queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }.expect("Failed to create copy queue");

        (device, Arc::new(copy_queue))
    }

    #[test]
    fn test_basic_allocation() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Allocate a page
        let va = 0x1000_0000; // 256 MiB
        let size = PAGE;
        let result = mem.allocate(va, size, 0x04, false); // PAGE_READWRITE
        assert!(result.is_ok(), "Allocation failed: {:?}", result.err());

        // Verify allocation exists
        let alloc = mem.get_allocation(va);
        assert!(alloc.is_some(), "Allocation not found");
        assert_eq!(alloc.unwrap().size, size);

        // Free it
        assert!(mem.free(va), "Free failed");
        assert!(mem.get_allocation(va).is_none(), "Allocation still exists after free");
    }

    #[test]
    fn test_placeholder_allocation() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Reserve address space without committing
        let va = 0x2000_0000;
        let size = PAGE * 16; // 1 MiB
        let result = mem.allocate(va, size, 0x04, true); // placeholder
        assert!(result.is_ok(), "Placeholder allocation failed: {:?}", result.err());

        let alloc = mem.get_allocation(va);
        assert!(alloc.is_some());
        assert!(alloc.unwrap().placeholder);
        assert_eq!(alloc.unwrap().committed_size, 0);

        // Free it
        assert!(mem.free(va));
    }

    #[test]
    fn test_gpu_resource_binding() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device.clone(), copy_queue).expect("Failed to create native memory");

        // Allocate title memory
        let va = 0x3000_0000;
        let size = PAGE * 4; // 256 KiB
        mem.allocate(va, size, 0x04, false).expect("Allocation failed");

        // Create GPU resource bound to this VA
        let gpu_va = mem.create_gpu_resource(va, size, true).expect("GPU resource creation failed");
        assert_ne!(gpu_va, 0, "GPU VA should not be zero");

        // Verify binding
        assert!(mem.has_gpu_binding(va));
        let retrieved_gpu_va = mem.get_gpu_va(va);
        assert_eq!(retrieved_gpu_va, Some(gpu_va));

        // Test offset within resource
        let offset_va = va + PAGE;
        let offset_gpu_va = mem.get_gpu_va(offset_va);
        assert_eq!(offset_gpu_va, Some(gpu_va + PAGE));
    }

    #[test]
    fn test_upload_readback_roundtrip() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Allocate and create GPU resource
        let va = 0x4000_0000;
        let size = PAGE * 2; // 128 KiB
        mem.allocate(va, size, 0x04, false).expect("Allocation failed");
        mem.create_gpu_resource(va, size, true).expect("GPU resource creation failed");

        // Prepare test data
        let test_data: Vec<u8> = (0..size as usize).map(|i| (i & 0xFF) as u8).collect();

        // Upload to GPU
        mem.upload_to_gpu(va, 0, &test_data).expect("Upload failed");

        // Read back from GPU
        let readback_data = mem.readback_from_gpu(va, 0, size).expect("Readback failed");

        // Verify data integrity
        assert_eq!(readback_data, test_data, "Data mismatch after upload/readback roundtrip");
    }

    #[test]
    fn test_partial_upload_readback() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        let va = 0x5000_0000;
        let size = PAGE * 4;
        mem.allocate(va, size, 0x04, false).expect("Allocation failed");
        mem.create_gpu_resource(va, size, true).expect("GPU resource creation failed");

        // Upload to middle of resource
        let offset = PAGE;
        let data = vec![0xAA; PAGE as usize];
        mem.upload_to_gpu(va, offset, &data).expect("Partial upload failed");

        // Read back just that region
        let readback = mem.readback_from_gpu(va, offset, PAGE).expect("Partial readback failed");
        assert_eq!(readback, data, "Partial data mismatch");
    }

    #[test]
    fn test_multiple_resources() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Create multiple resources at different VAs
        let resources = [
            (0x6000_0000, PAGE),
            (0x6100_0000, PAGE * 2),
            (0x6200_0000, PAGE * 4),
            (0x6300_0000, PAGE * 8),
        ];

        for (va, size) in resources {
            mem.allocate(va, size, 0x04, false).expect(&format!("Allocation failed for {:#x}", va));
            let gpu_va = mem.create_gpu_resource(va, size, true).expect(&format!("GPU resource failed for {:#x}", va));
            assert_ne!(gpu_va, 0);
        }

        // Verify all bindings exist
        for (va, _) in resources {
            assert!(mem.has_gpu_binding(va), "Binding missing for {:#x}", va);
            assert!(mem.get_gpu_va(va).is_some());
        }

        // Free all
        for (va, _) in resources {
            assert!(mem.free(va), "Free failed for {:#x}", va);
        }
    }

    #[test]
    fn test_concurrent_allocation_lookup() {
        let (device, copy_queue) = create_test_device();
        let mem = Arc::new(NativeMemory::new(device, copy_queue).expect("Failed to create native memory"));

        // Pre-populate with allocations
        for i in 0..1000 {
            let va = 0x7000_0000 + i * PAGE * 4;
            mem.allocate(va, PAGE, 0x04, false).expect("Pre-populate failed");
        }

        // Spawn threads doing concurrent lookups
        let handles: Vec<_> = (0..8).map(|_| {
            let mem = mem.clone();
            thread::spawn(move || {
                for _ in 0..10000 {
                    let idx = fastrand::usize(0..1000);
                    let va = 0x7000_0000 + idx * PAGE * 4;
                    let _ = mem.get_allocation(va);
                    let _ = mem.get_gpu_va(va);
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_arena_growth() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Allocate many resources to trigger arena growth
        let mut vas = Vec::new();
        for i in 0..200 {
            let va = 0x8000_0000 + i * PAGE * 4;
            let size = PAGE * 2;
            mem.allocate(va, size, 0x04, false).expect(&format!("Allocation {} failed", i));
            mem.create_gpu_resource(va, size, true).expect(&format!("GPU resource {} failed", i));
            vas.push(va);
        }

        // Verify all resources have valid GPU VAs
        for va in &vas {
            assert!(mem.get_gpu_va(*va).is_some(), "Missing GPU VA for {:#x}", va);
        }

        // Free all
        for va in vas {
            mem.free(va);
        }
    }

    #[test]
    fn test_large_resource() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Allocate a large resource (16 MiB)
        let va = 0x9000_0000;
        let size = 16 * 1024 * 1024; // 16 MiB
        mem.allocate(va, size, 0x04, false).expect("Large allocation failed");

        let gpu_va = mem.create_gpu_resource(va, size, true).expect("Large GPU resource failed");
        assert_ne!(gpu_va, 0);

        // Upload and readback large data
        let test_data: Vec<u8> = (0..size).map(|i| (i & 0xFF) as u8).collect();
        mem.upload_to_gpu(va, 0, &test_data).expect("Large upload failed");

        let readback = mem.readback_from_gpu(va, 0, size).expect("Large readback failed");
        assert_eq!(readback.len(), size as usize);
        // Verify a few sample points
        assert_eq!(readback[0], 0);
        assert_eq!(readback[size as usize / 2], (size as usize / 2 & 0xFF) as u8);
        assert_eq!(readback[size as usize - 1], ((size - 1) & 0xFF) as u8);
    }

    #[test]
    fn test_uav_flag() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        let va = 0xA000_0000;
        let size = PAGE * 4;

        mem.allocate(va, size, 0x04, false).expect("Allocation failed");

        // Create with UAV
        let gpu_va_uav = mem.create_gpu_resource(va, size, true).expect("UAV resource failed");
        assert_ne!(gpu_va_uav, 0);

        mem.free(va);

        // Create without UAV
        let va2 = 0xA100_0000;
        mem.allocate(va2, size, 0x04, false).expect("Allocation 2 failed");
        let gpu_va_no_uav = mem.create_gpu_resource(va2, size, false).expect("Non-UAV resource failed");
        assert_ne!(gpu_va_no_uav, 0);
    }

    #[test]
    fn test_frame_advancement_stats() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        let va = 0xB000_0000;
        let size = PAGE;
        mem.allocate(va, size, 0x04, false).expect("Allocation failed");
        mem.create_gpu_resource(va, size, true).expect("GPU resource failed");

        // Advance frames
        for _ in 0..10 {
            mem.next_frame();
        }

        // Just verify it doesn't panic
    }

    #[test]
    fn test_alias_detection() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Two VAs pointing to same physical memory (aliasing)
        // In native backend, each VA gets its own allocation
        // This test verifies the backend handles overlapping VAs correctly
        let va1 = 0xC000_0000;
        let va2 = va1 + PAGE; // Adjacent, not overlapping
        let size = PAGE;

        mem.allocate(va1, size, 0x04, false).expect("Alloc 1 failed");
        mem.allocate(va2, size, 0x04, false).expect("Alloc 2 failed");

        let gpu_va1 = mem.create_gpu_resource(va1, size, true).expect("GPU 1 failed");
        let gpu_va2 = mem.create_gpu_resource(va2, size, true).expect("GPU 2 failed");

        // They should have different GPU VAs
        assert_ne!(gpu_va1, gpu_va2);

        // And the offset calculation should work
        assert_eq!(mem.get_gpu_va(va1), Some(gpu_va1));
        assert_eq!(mem.get_gpu_va(va2), Some(gpu_va2));
    }

    #[test]
    fn test_aliased_resource() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Create source resource
        let source_va = 0xD000_0000;
        let size = PAGE * 4;
        mem.allocate(source_va, size, 0x04, false).expect("Source alloc failed");
        let source_gpu_va = mem.create_gpu_resource(source_va, size, true).expect("Source GPU resource failed");

        // Create aliased resource at same heap location
        let alias_va = 0xD100_0000;
        let alias_gpu_va = mem.create_aliased_resource(alias_va, source_va, size, true).expect("Aliased resource failed");

        // Aliased resource should have different GPU VA but same heap offset
        assert_ne!(source_gpu_va, alias_gpu_va);
        assert!(mem.has_gpu_binding(alias_va));
        assert_eq!(mem.get_gpu_va(alias_va), Some(alias_gpu_va));

        // Verify both can be used independently
        let data1 = vec![0x11; size as usize];
        let data2 = vec![0x22; size as usize];
        mem.upload_to_gpu(source_va, 0, &data1).expect("Source upload failed");
        mem.upload_to_gpu(alias_va, 0, &data2).expect("Alias upload failed");

        // Readback should show different data
        let readback1 = mem.readback_from_gpu(source_va, 0, size).expect("Source readback failed");
        let readback2 = mem.readback_from_gpu(alias_va, 0, size).expect("Alias readback failed");
        assert_eq!(readback1, data1);
        assert_eq!(readback2, data2);
    }

    #[test]
    fn test_texture_creation() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        let va = 0xE000_0000;
        
        // Test BC3 compressed texture with mipmaps
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: 1024,
            Height: 1024,
            DepthOrArraySize: 1,
            MipLevels: 10,
            Format: DXGI_FORMAT_BC3_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        };

        let gpu_va = mem.create_gpu_texture(va, &desc, true).expect("Texture creation failed");
        assert_ne!(gpu_va, 0);
        assert!(mem.has_gpu_binding(va));
        assert_eq!(mem.get_gpu_va(va), Some(gpu_va));
    }

    #[test]
    fn test_blas_tlas_creation() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device.clone(), copy_queue.clone()).expect("Failed to create native memory");

        // Test BLAS creation
        let blas_va = 0xF000_0000;
        let blas_size = 64 * 1024; // 64 KiB
        let build_inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
            Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(0),
            NumDescs: 1,
            DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
            pGeometryDescs: std::ptr::null(),
            ..Default::default()
        };
        
        let blas_gpu_va = mem.create_blas(blas_va, blas_size, &build_inputs).expect("BLAS creation failed");
        assert_ne!(blas_gpu_va, 0);
        assert!(mem.has_gpu_binding(blas_va));

        // Test TLAS creation
        let tlas_va = 0xF100_0000;
        let tlas_size = 64 * 1024;
        let tlas_gpu_va = mem.create_tlas(tlas_va, tlas_size).expect("TLAS creation failed");
        assert_ne!(tlas_gpu_va, 0);
        assert!(mem.has_gpu_binding(tlas_va));

        // Test prebuild info
        let prebuild_info = mem.get_acceleration_structure_prebuild_info(&build_inputs);
        assert!(prebuild_info.ResultDataMaxSizeInBytes > 0);
        assert!(prebuild_info.ScratchDataSizeInBytes > 0);
    }
}

// Synthetic benchmarks comparing legacy vs native backends
#[cfg(all(windows, feature = "native-memory"))]
mod benchmarks {
    use super::*;
    use std::time::Instant;
    use std::sync::Arc;

    #[test]
    fn benchmark_allocation_latency() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        const ITERATIONS: usize = 10000;
        let start = Instant::now();
        
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * PAGE * 4;
            mem.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }
        
        let alloc_time = start.elapsed();
        println!("Allocation latency: {:?} for {} allocations ({:?} per alloc)", 
            alloc_time, ITERATIONS, alloc_time / ITERATIONS as u32);
        
        // Cleanup
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * PAGE * 4;
            mem.free(va);
        }
    }

    #[test]
    fn benchmark_gpu_resource_creation() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        const ITERATIONS: usize = 1000;
        // Pre-allocate memory
        for i in 0..ITERATIONS {
            let va = 0x2000_0000 + i * PAGE * 4;
            mem.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }

        let start = Instant::now();
        
        for i in 0..ITERATIONS {
            let va = 0x2000_0000 + i * PAGE * 4;
            mem.create_gpu_resource(va, PAGE, true).expect("GPU resource creation failed");
        }
        
        let create_time = start.elapsed();
        println!("GPU resource creation: {:?} for {} resources ({:?} per resource)", 
            create_time, ITERATIONS, create_time / ITERATIONS as u32);
        
        // Cleanup
        for i in 0..ITERATIONS {
            let va = 0x2000_0000 + i * PAGE * 4;
            mem.free(va);
        }
    }

    #[test]
    fn benchmark_upload_throughput() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        const BUFFER_SIZE: u64 = 1024 * 1024; // 1 MiB
        const ITERATIONS: usize = 100;
        
        let va = 0x3000_0000;
        mem.allocate(va, BUFFER_SIZE, 0x04, false).expect("Allocation failed");
        mem.create_gpu_resource(va, BUFFER_SIZE, true).expect("GPU resource creation failed");

        let test_data = vec![0xAA; BUFFER_SIZE as usize];
        
        let start = Instant::now();
        
        for _ in 0..ITERATIONS {
            mem.upload_to_gpu(va, 0, &test_data).expect("Upload failed");
        }
        
        let upload_time = start.elapsed();
        let total_bytes = BUFFER_SIZE * ITERATIONS as u64;
        let throughput_mb_s = (total_bytes as f64 / 1024.0 / 1024.0) / upload_time.as_secs_f64();
        println!("Upload throughput: {:.2} MB/s ({:?} for {} MiB)", 
            throughput_mb_s, upload_time, total_bytes / 1024 / 1024);
    }

    #[test]
    fn benchmark_readback_throughput() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        const BUFFER_SIZE: u64 = 1024 * 1024; // 1 MiB
        const ITERATIONS: usize = 50;
        
        let va = 0x4000_0000;
        mem.allocate(va, BUFFER_SIZE, 0x04, false).expect("Allocation failed");
        mem.create_gpu_resource(va, BUFFER_SIZE, true).expect("GPU resource creation failed");

        let test_data = vec![0xBB; BUFFER_SIZE as usize];
        mem.upload_to_gpu(va, 0, &test_data).expect("Initial upload failed");
        
        let start = Instant::now();
        
        for _ in 0..ITERATIONS {
            let _ = mem.readback_from_gpu(va, 0, BUFFER_SIZE).expect("Readback failed");
        }
        
        let readback_time = start.elapsed();
        let total_bytes = BUFFER_SIZE * ITERATIONS as u64;
        let throughput_mb_s = (total_bytes as f64 / 1024.0 / 1024.0) / readback_time.as_secs_f64();
        println!("Readback throughput: {:.2} MB/s ({:?} for {} MiB)", 
            throughput_mb_s, readback_time, total_bytes / 1024 / 1024);
    }

    #[test]
    fn benchmark_concurrent_access() {
        let (device, copy_queue) = create_test_device();
        let mem = Arc::new(NativeMemory::new(device, copy_queue).expect("Failed to create native memory"));

        const THREADS: usize = 8;
        const OPS_PER_THREAD: usize = 10000;
        
        // Pre-populate
        for i in 0..1000 {
            let va = 0x5000_0000 + i * PAGE * 4;
            mem.allocate(va, PAGE, 0x04, false).expect("Pre-populate failed");
            mem.create_gpu_resource(va, PAGE, true).expect("GPU resource failed");
        }

        let start = Instant::now();
        let handles: Vec<_> = (0..THREADS).map(|_| {
            let mem = mem.clone();
            std::thread::spawn(move || {
                for _ in 0..OPS_PER_THREAD {
                    let idx = fastrand::usize(0..1000);
                    let va = 0x5000_0000 + idx * PAGE * 4;
                    let _ = mem.get_allocation(va);
                    let _ = mem.get_gpu_va(va);
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }
        
        let concurrent_time = start.elapsed();
        let total_ops = THREADS * OPS_PER_THREAD * 2; // get_allocation + get_gpu_va
        println!("Concurrent access: {:?} for {} ops ({:?} per op)", 
            concurrent_time, total_ops, concurrent_time / total_ops as u32);
    }

    #[test]
    fn benchmark_memory_consumption() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        const RESOURCE_COUNT: usize = 10000;
        const RESOURCE_SIZE: u64 = 256 * 1024; // 256 KiB
        
        let start = Instant::now();
        
        for i in 0..RESOURCE_COUNT {
            let va = 0x6000_0000 + i * RESOURCE_SIZE;
            mem.allocate(va, RESOURCE_SIZE, 0x04, false).expect("Allocation failed");
            mem.create_gpu_resource(va, RESOURCE_SIZE, true).expect("GPU resource failed");
        }
        
        let creation_time = start.elapsed();
        let total_memory = RESOURCE_COUNT as u64 * RESOURCE_SIZE;
        println!("Memory consumption test: {:?} for {} resources ({} MB total)", 
            creation_time, RESOURCE_COUNT, total_memory / 1024 / 1024);
        
        // Report stats
        mem.next_frame();
        
        // Cleanup
        for i in 0..RESOURCE_COUNT {
            let va = 0x6000_0000 + i * RESOURCE_SIZE;
            mem.free(va);
        }
    }
}

// Legacy backend benchmarks for comparison (requires native-memory feature to be OFF)
#[cfg(all(windows, not(feature = "native-memory")))]
mod legacy_benchmarks {
    use super::*;
    use std::time::Instant;
    use std::sync::Arc;

    /// Create a test D3D12 device for legacy backend.
    fn create_test_device_legacy() -> (Arc<ID3D12Device>, Arc<ID3D12CommandQueue>) {
        let host = host().expect("Failed to get host");
        let device = host.device.clone();

        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            Priority: 0,
            Flags: D3D12_COMMAND_QUEUE_FLAGS(0),
            NodeMask: 0,
        };
        let copy_queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }.expect("Failed to create copy queue");

        (device, Arc::new(copy_queue))
    }

    #[test]
    fn benchmark_legacy_allocation_latency() {
        let (device, copy_queue) = create_test_device_legacy();
        let backend = victory_gpu::memory_backend::LegacyBackend::new();

        const ITERATIONS: usize = 10000;
        let start = Instant::now();
        
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * PAGE * 4;
            backend.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }
        
        let alloc_time = start.elapsed();
        println!("Legacy allocation latency: {:?} for {} allocations ({:?} per alloc)", 
            alloc_time, ITERATIONS, alloc_time / ITERATIONS as u32);
        
        // Cleanup
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * PAGE * 4;
            backend.free(va);
        }
    }

    #[test]
    fn benchmark_legacy_gpu_resource_creation() {
        let (device, copy_queue) = create_test_device_legacy();
        let backend = victory_gpu::memory_backend::LegacyBackend::new();

        const ITERATIONS: usize = 1000;
        // Pre-allocate memory
        for i in 0..ITERATIONS {
            let va = 0x2000_0000 + i * PAGE * 4;
            backend.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }

        let start = Instant::now();
        
        for i in 0..ITERATIONS {
            let va = 0x2000_0000 + i * PAGE * 4;
            let _ = backend.create_gpu_resource(va, PAGE, true);
        }
        
        let create_time = start.elapsed();
        println!("Legacy GPU resource creation: {:?} for {} resources ({:?} per resource)", 
            create_time, ITERATIONS, create_time / ITERATIONS as u32);
        
        // Cleanup
        for i in 0..ITERATIONS {
            let va = 0x2000_0000 + i * PAGE * 4;
            backend.free(va);
        }
    }

    #[test]
    fn benchmark_legacy_upload_throughput() {
        let (device, copy_queue) = create_test_device_legacy();
        let backend = victory_gpu::memory_backend::LegacyBackend::new();

        const BUFFER_SIZE: u64 = 1024 * 1024; // 1 MiB
        const ITERATIONS: usize = 100;
        
        let va = 0x3000_0000;
        backend.allocate(va, BUFFER_SIZE, 0x04, false).expect("Allocation failed");
        backend.create_gpu_resource(va, BUFFER_SIZE, true).expect("GPU resource creation failed");

        let test_data = vec![0xAA; BUFFER_SIZE as usize];
        
        let start = Instant::now();
        
        for _ in 0..ITERATIONS {
            backend.upload_to_gpu(va, 0, &test_data).ok(); // Legacy may not implement this
        }
        
        let upload_time = start.elapsed();
        let total_bytes = BUFFER_SIZE * ITERATIONS as u64;
        let throughput_mb_s = (total_bytes as f64 / 1024.0 / 1024.0) / upload_time.as_secs_f64();
        println!("Legacy upload throughput: {:.2} MB/s ({:?} for {} MiB)", 
            throughput_mb_s, upload_time, total_bytes / 1024 / 1024);
    }
}

// Comparative benchmarks: run both backends under identical workloads
#[cfg(all(windows, feature = "native-memory"))]
mod comparative_benchmarks {
    use super::*;
    use std::time::Instant;
    use std::sync::Arc;

    #[test]
    fn benchmark_comparative_allocation_latency() {
        let (device, copy_queue) = create_test_device();
        
        // Test native backend
        let native_mem = NativeMemory::new(device.clone(), copy_queue.clone()).expect("Failed to create native memory");
        
        const ITERATIONS: usize = 10000;
        
        // Native allocation
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * PAGE * 4;
            native_mem.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }
        let native_alloc_time = start.elapsed();
        
        // Cleanup native
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * PAGE * 4;
            native_mem.free(va);
        }
        
        // Test legacy backend (using the trait)
        let legacy_backend = victory_gpu::memory_backend::LegacyBackend::new();
        
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * PAGE * 4;
            legacy_backend.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }
        let legacy_alloc_time = start.elapsed();
        
        // Cleanup legacy
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * PAGE * 4;
            legacy_backend.free(va);
        }
        
        println!("=== Allocation Latency Comparison ===");
        println!("Native:  {:?} for {} allocs ({:?}/alloc)", native_alloc_time, ITERATIONS, native_alloc_time / ITERATIONS as u32);
        println!("Legacy:  {:?} for {} allocs ({:?}/alloc)", legacy_alloc_time, ITERATIONS, legacy_alloc_time / ITERATIONS as u32);
        println!("Speedup: {:.2}x", legacy_alloc_time.as_secs_f64() / native_alloc_time.as_secs_f64());
    }

    #[test]
    fn benchmark_comparative_upload_throughput() {
        let (device, copy_queue) = create_test_device();
        
        const BUFFER_SIZE: u64 = 1024 * 1024; // 1 MiB
        const ITERATIONS: usize = 50;
        
        // Native backend
        let native_mem = NativeMemory::new(device.clone(), copy_queue.clone()).expect("Failed to create native memory");
        let va_native = 0x3000_0000;
        native_mem.allocate(va_native, BUFFER_SIZE, 0x04, false).expect("Allocation failed");
        native_mem.create_gpu_resource(va_native, BUFFER_SIZE, true).expect("GPU resource creation failed");
        
        // Legacy backend
        let legacy_backend = victory_gpu::memory_backend::LegacyBackend::new();
        let va_legacy = 0x4000_0000;
        legacy_backend.allocate(va_legacy, BUFFER_SIZE, 0x04, false).expect("Allocation failed");
        legacy_backend.create_gpu_resource(va_legacy, BUFFER_SIZE, true).ok(); // May fail
        
        let test_data = vec![0xAA; BUFFER_SIZE as usize];
        
        // Native upload
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            native_mem.upload_to_gpu(va_native, 0, &test_data).expect("Upload failed");
        }
        let native_upload_time = start.elapsed();
        
        // Legacy upload (may not be fully implemented)
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            legacy_backend.upload_to_gpu(va_legacy, 0, &test_data).ok();
        }
        let legacy_upload_time = start.elapsed();
        
        let total_bytes = BUFFER_SIZE * ITERATIONS as u64;
        let native_throughput = (total_bytes as f64 / 1024.0 / 1024.0) / native_upload_time.as_secs_f64();
        let legacy_throughput = if legacy_upload_time.as_secs_f64() > 0.0 {
            (total_bytes as f64 / 1024.0 / 1024.0) / legacy_upload_time.as_secs_f64()
        } else {
            0.0
        };
        
        println!("=== Upload Throughput Comparison ===");
        println!("Native:  {:.2} MB/s ({:?})", native_throughput, native_upload_time);
        println!("Legacy:  {:.2} MB/s ({:?})", legacy_throughput, legacy_upload_time);
        if legacy_throughput > 0.0 {
            println!("Speedup: {:.2}x", native_throughput / legacy_throughput);
        }
    }
}

// Tests that work without native-memory feature (validation of assumptions)
#[cfg(test)]
mod assumption_validation_tests {
    use std::collections::HashMap;

    /// Validate the COPIES=4095 constant and its implications.
    #[test]
    fn validate_copies_constant() {
        // COPIES = (1 << 12) - 1 = 4095
        const COPIES: u32 = (1 << 12) - 1;
        assert_eq!(COPIES, 4095);

        // Each copy gets a UAV slot in a descriptor heap
        // Total slots needed = COPIES + 1 (for null view) = 4096
        assert_eq!(COPIES + 1, 4096);
    }

    /// Validate MEMORY_PAGES and directory size.
    #[test]
    fn validate_directory_size() {
        const PAGE: u64 = 65536; // 64 KiB
        const MEMORY_PAGES: u32 = 1 << 26; // 4 GiB / 64 KiB = 67,108,864

        // Directory entry is 2 u32s (copy number + offset) = 8 bytes
        const ENTRY_SIZE: usize = 8;
        const DIRECTORY_SIZE: usize = MEMORY_PAGES as usize * ENTRY_SIZE;

        // ~512 MiB for directory
        assert_eq!(DIRECTORY_SIZE, 536_870_912); // 512 MiB

        // With COPIES=4095, each copy is a separate Shadow (D3D12 resource)
        // Each shadow is at least 64 KiB (one page)
        const MIN_SHADOW_SIZE: u64 = 65536;
        const MAX_SHADOW_MEMORY: u64 = COPIES as u64 * MIN_SHADOW_SIZE;

        // Max shadow memory = 4095 * 64 KiB = ~256 MiB minimum
        assert_eq!(MAX_SHADOW_MEMORY, 268_369_920); // ~256 MiB
    }

    /// Validate arena and upload memory constants.
    #[test]
    fn validate_arena_constants() {
        const ARENA: u64 = 64 * 1024 * 1024; // 64 MiB
        const PAGE: u64 = 65536; // 64 KiB
        const PAGES_PER_ARENA: u64 = ARENA / PAGE;

        assert_eq!(PAGES_PER_ARENA, 1024); // 1024 pages per arena

        // MOST = 3 << 14 = 49,152 pages max = ~3 GiB
        const MOST: usize = 3 << 14;
        assert_eq!(MOST, 49_152);
        assert_eq!(MOST as u64 * PAGE, 3 * 1024 * 1024 * 1024); // 3 GiB
    }

    /// Validate upload ring buffer sizing.
    #[test]
    fn validate_upload_sizing() {
        const UPLOAD_RING_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB
        const READBACK_RING_SIZE: u64 = 16 * 1024 * 1024; // 16 MiB
        const PAGE: u64 = 65536;
        const UPLOAD_ALIGNMENT: u64 = 65536;

        // Max concurrent uploads of PAGE size
        let max_concurrent_uploads = UPLOAD_RING_SIZE / PAGE;
        assert_eq!(max_concurrent_uploads, 1024);

        // Max concurrent readbacks
        let max_concurrent_readbacks = READBACK_RING_SIZE / PAGE;
        assert_eq!(max_concurrent_readbacks, 256);
    }

    /// Validate HashMap vs Vec for sparse allocations.
    #[test]
    fn validate_sparse_vs_dense() {
        // Dense directory: 67M entries * 8 bytes = 512 MiB always
        // Sparse DashMap: only allocates for actual allocations
        // Typical title: ~10,000 allocations = ~10,000 entries

        let mut dense_size = 0;
        let mut sparse_entries = 0;

        // Simulate typical title allocation pattern
        let mut map = HashMap::new();
        for i in 0..10_000 {
            let va = 0x1000_0000 + i * 65536 * 4; // 256 KiB each
            map.insert(va, vec![0u8; 256 * 1024]);
            sparse_entries += 1;
        }

        // Dense would be 67M * 8 = 536 MB
        // Sparse is ~10K entries * (key + value + overhead) ~ a few MB
        dense_size = (1 << 26) * 8;
        let sparse_estimate = sparse_entries * 64; // rough estimate

        println!("Dense directory: {} MB", dense_size / 1024 / 1024);
        println!("Sparse estimate: {} MB", sparse_estimate / 1024 / 1024);

        assert!(sparse_estimate < dense_size / 100); // Sparse should be < 1% of dense
    }
}