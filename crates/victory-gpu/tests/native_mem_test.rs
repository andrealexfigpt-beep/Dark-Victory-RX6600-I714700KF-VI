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

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::*;

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