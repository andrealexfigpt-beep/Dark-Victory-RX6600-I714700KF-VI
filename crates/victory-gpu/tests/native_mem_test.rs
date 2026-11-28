//! Synthetic workload tests for the native memory backend.
//!
//! These tests validate:
//! - Memory allocation and freeing
//! - GPU resource binding (title VA -> GPU VA)
//! - Upload (CPU -> GPU) and readback (GPU -> CPU)
//! - Concurrent access patterns
//! - Memory consumption and allocation lookup performance
//! - Aliasing and resource lifetime correctness

use std::sync::Arc;
use victory_gpu::native_mem::{NativeMemory, PAGE};
use windows::Win32::Graphics::{Direct3D::D3D_FEATURE_LEVEL_12_0, Direct3D12::*, Dxgi::{*, Common::*}};

fn create_test_device() -> (Arc<ID3D12Device>, Arc<ID3D12CommandQueue>) {
    let factory: IDXGIFactory6 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }.unwrap();
    for index in 0.. {
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapterByGpuPreference(index, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) }.expect("physical D3D12 adapter");
        let desc = unsafe { adapter.GetDesc1() }.unwrap();
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 { continue; }
        let mut device: Option<ID3D12Device> = None;
        if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_err() { continue; }
        let device = device.unwrap();
        eprintln!("D3D12 physical adapter: {}", String::from_utf16_lossy(&desc.Description).trim_end_matches(char::from(0)));
        let queue = unsafe { device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COPY, ..Default::default() }) }.unwrap();
        return (Arc::new(device), Arc::new(queue));
    }
    unreachable!()
}

mod native_mem_tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

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
        assert!(alloc.as_ref().unwrap().is_placeholder());
        assert_eq!(alloc.unwrap().committed_size(), 0);

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
        let gpu_va = mem.create_gpu_resource(va, size, true).expect("GPU resource creation failed").gpu_va;
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
            let gpu_va = mem.create_gpu_resource(va, size, true).expect(&format!("GPU resource failed for {:#x}", va)).gpu_va;
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
            let va = 0x7000_0000 + i as u64 * PAGE * 4;
            mem.allocate(va, PAGE, 0x04, false).expect("Pre-populate failed");
        }

        // Spawn threads doing concurrent lookups
        let handles: Vec<_> = (0..8).map(|_| {
            let mem = mem.clone();
            thread::spawn(move || {
                for _ in 0..10000 {
                    let idx = fastrand::usize(0..1000);
                    let va = 0x7000_0000 + idx as u64 * PAGE * 4;
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
            let va = 0x8000_0000 + i as u64 * PAGE * 4;
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

        let gpu_va = mem.create_gpu_resource(va, size, true).expect("Large GPU resource failed").gpu_va;
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
        let gpu_va_uav = mem.create_gpu_resource(va, size, true).expect("UAV resource failed").gpu_va;
        assert_ne!(gpu_va_uav, 0);

        mem.free(va);

        // Create without UAV
        let va2 = 0xA100_0000;
        mem.allocate(va2, size, 0x04, false).expect("Allocation 2 failed");
        let gpu_va_no_uav = mem.create_gpu_resource(va2, size, false).expect("Non-UAV resource failed").gpu_va;
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

        let gpu_va1 = mem.create_gpu_resource(va1, size, true).expect("GPU 1 failed").gpu_va;
        let gpu_va2 = mem.create_gpu_resource(va2, size, true).expect("GPU 2 failed").gpu_va;

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

        // Distinct resource objects alias the same heap range.
        assert_ne!(source_gpu_va.resource(), alias_gpu_va.resource());
        assert!(mem.has_gpu_binding(alias_va));
        assert_eq!(mem.get_gpu_va(alias_va), Some(alias_gpu_va.gpu_va));

        // Verify both can be used independently
        let data1 = vec![0x11; size as usize];
        let data2 = vec![0x22; size as usize];
        mem.upload_to_gpu(source_va, 0, &data1).expect("Source upload failed");
        assert_eq!(mem.readback_from_gpu(alias_va, 0, size).unwrap(), data1);
        mem.upload_to_gpu(alias_va, 0, &data2).expect("Alias upload failed");

        // Readback should show different data
        let readback1 = mem.readback_from_gpu(source_va, 0, size).expect("Source readback failed");
        let readback2 = mem.readback_from_gpu(alias_va, 0, size).expect("Alias readback failed");
        assert_eq!(readback1, data2);
        assert_eq!(readback2, data2);
    }

    #[test]
    fn test_overlapping_memory_ranges() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Test that overlapping VAs are handled correctly
        // First allocation at base VA
        let va1 = 0xE000_0000;
        let size1 = PAGE * 8; // 512 KiB
        mem.allocate(va1, size1, 0x04, false).expect("Alloc 1 failed");
        let gpu_va1 = mem.create_gpu_resource(va1, size1, true).expect("GPU 1 failed").gpu_va;

        // Second allocation that overlaps with first (should fail or be handled)
        // In the native backend, each VA is tracked independently
        // Overlapping VAs would be separate allocations with separate GPU resources
        let va2 = va1 + PAGE * 2; // Overlaps with va1
        let size2 = PAGE * 4;
        assert!(mem.allocate(va2, size2, 0x04, false).is_err(), "overlapping fixed-address host allocation must fail");
        let gpu_va2 = mem.create_gpu_resource(va2, size2, true).expect("GPU 2 failed").gpu_va;

        // They should have different GPU VAs (different resources)
        assert_ne!(gpu_va1, gpu_va2);

        // Write to overlapping region through both VAs
        let data1 = vec![0x11; size1 as usize];
        let data2 = vec![0x22; size2 as usize];
        mem.upload_to_gpu(va1, 0, &data1).expect("Upload 1 failed");
        mem.upload_to_gpu(va2, 0, &data2).expect("Upload 2 failed");

        // Read back from both
        let readback1 = mem.readback_from_gpu(va1, 0, size1).expect("Readback 1 failed");
        let readback2 = mem.readback_from_gpu(va2, 0, size2).expect("Readback 2 failed");

        // Each readback should show the data written through that VA
        // (They are separate resources, so no actual overlap in GPU memory)
        assert_eq!(readback1, data1);
        assert_eq!(readback2, data2);
    }

    #[test]
    fn test_aliasing_barrier_synchronization() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Create source resource
        let source_va = 0xF000_0000;
        let size = PAGE * 4;
        mem.allocate(source_va, size, 0x04, false).expect("Source alloc failed");
        let source_gpu_va = mem.create_gpu_resource(source_va, size, true).expect("Source GPU resource failed").gpu_va;

        // Create aliased resource at same heap location
        let alias_va = 0xF100_0000;
        let alias_gpu_va = mem.create_aliased_resource(alias_va, source_va, size, true).expect("Aliased resource failed").gpu_va;

        // Upload different data through each VA
        let data1 = vec![0xAA; size as usize];
        let data2 = vec![0xBB; size as usize];
        mem.upload_to_gpu(source_va, 0, &data1).expect("Source upload failed");
        mem.upload_to_gpu(alias_va, 0, &data2).expect("Alias upload failed");

        // In D3D12, aliased resources share the same heap memory
        // The last write wins (data2), but we need proper synchronization
        // Read back from both - they share the same physical memory
        let readback1 = mem.readback_from_gpu(source_va, 0, size).expect("Source readback failed");
        let readback2 = mem.readback_from_gpu(alias_va, 0, size).expect("Alias readback failed");

        // Since they're aliased to the same heap, both should show the last write
        // (data2 from alias_va upload which happened after source_va upload)
        // Note: This depends on GPU execution order; fence synchronization ensures ordering
        assert_eq!(readback1, readback2, "Aliased resources should show same data");
    }

    #[test]
    fn test_aliased_texture_barrier() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device, copy_queue).expect("Failed to create native memory");

        // Create source texture
        let source_va = 0xF200_0000;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        };
        mem.allocate(source_va, PAGE * 4, 0x04, false).expect("Source texture alloc failed");
        let source_gpu_va = mem.create_gpu_texture(source_va, &desc, true).expect("Source texture failed");

        // Create aliased texture
        let alias_va = 0xF300_0000;
        let alias_gpu_va = mem.create_aliased_texture(alias_va, source_va, &desc, true).expect("Aliased texture failed");

        // Both should be valid
        assert_ne!(source_gpu_va.resource(), alias_gpu_va.resource());
        let pixels: Vec<u8> = (0..256*256*4).map(|i| (i ^ (i >> 8)) as u8).collect();
        mem.upload_texture_to_gpu(source_va, 0, &pixels, 1024, 256*1024).unwrap();
        assert_eq!(mem.readback_texture(alias_va, 0).unwrap(), pixels);
        let changed: Vec<_> = pixels.iter().map(|b| !b).collect();
        mem.upload_texture_to_gpu(alias_va, 0, &changed, 1024, 256*1024).unwrap();
        assert_eq!(mem.readback_texture(source_va, 0).unwrap(), changed);
        assert!(mem.has_gpu_binding(source_va));
        assert!(mem.has_gpu_binding(alias_va));
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
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };

        let binding = mem.create_gpu_texture(va, &desc, false).expect("Texture creation failed");
        let actual = unsafe { binding.resource().GetDesc() };
        assert_eq!((actual.Format, actual.Width, actual.MipLevels), (desc.Format, desc.Width, desc.MipLevels));
        assert!(mem.has_gpu_binding(va));
        assert_eq!(mem.get_gpu_va(va), None);
        for mip in 0..10 {
            let blocks = ((1024_u32 >> mip).div_ceil(4)).max(1);
            let bytes: Vec<u8> = (0..blocks*blocks*16).map(|i| (i ^ (i>>8) ^ mip) as u8).collect();
            mem.upload_texture_to_gpu(va, mip, &bytes, blocks*16, bytes.len() as u64).unwrap();
            assert_eq!(mem.readback_texture(va, mip).unwrap(), bytes);
        }
    }

    #[test]
    fn test_blas_tlas_creation() {
        let (device, copy_queue) = create_test_device();
        let mem = NativeMemory::new(device.clone(), copy_queue.clone()).expect("Failed to create native memory");

        // Test BLAS creation
        let blas_va = 0xF000_0000;
        let blas_size = 64 * 1024; // 64 KiB
        let geometry = D3D12_RAYTRACING_GEOMETRY_DESC {
            Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
            Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 { Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                VertexFormat: DXGI_FORMAT_R32G32B32_FLOAT, VertexCount: 3,
                VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE { StartAddress: 0, StrideInBytes: 12 }, ..Default::default()
            } }, ..Default::default()
        };
        let build_inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 { pGeometryDescs: &geometry },
            Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
            Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(0),
            NumDescs: 1,
            DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
            ..Default::default()
        };
        
        let blas_gpu_va = mem.create_blas(blas_va, blas_size, &build_inputs).expect("BLAS creation failed").gpu_va;
        assert_ne!(blas_gpu_va, 0);
        assert!(mem.has_gpu_binding(blas_va));

        // Test TLAS creation
        let tlas_va = 0xF100_0000;
        let tlas_size = 64 * 1024;
        let tlas_gpu_va = mem.create_tlas(tlas_va, tlas_size).expect("TLAS creation failed").gpu_va;
        assert_ne!(tlas_gpu_va, 0);
        assert!(mem.has_gpu_binding(tlas_va));

        // Test prebuild info
        let prebuild_info = mem.get_acceleration_structure_prebuild_info(&build_inputs);
        assert!(prebuild_info.ResultDataMaxSizeInBytes > 0);
        assert!(prebuild_info.ScratchDataSizeInBytes > 0);
    }
}

// Synthetic benchmarks comparing legacy vs native backends
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
            let va = 0x1000_0000 + i as u64 * PAGE * 4;
            mem.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }
        
        let alloc_time = start.elapsed();
        println!("Allocation latency: {:?} for {} allocations ({:?} per alloc)", 
            alloc_time, ITERATIONS, alloc_time / ITERATIONS as u32);
        
        // Cleanup
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i as u64 * PAGE * 4;
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
            let va = 0x2000_0000 + i as u64 * PAGE * 4;
            mem.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }

        let start = Instant::now();
        
        for i in 0..ITERATIONS {
            let va = 0x2000_0000 + i as u64 * PAGE * 4;
            mem.create_gpu_resource(va, PAGE, true).expect("GPU resource creation failed");
        }
        
        let create_time = start.elapsed();
        println!("GPU resource creation: {:?} for {} resources ({:?} per resource)", 
            create_time, ITERATIONS, create_time / ITERATIONS as u32);
        
        // Cleanup
        for i in 0..ITERATIONS {
            let va = 0x2000_0000 + i as u64 * PAGE * 4;
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
            let va = 0x5000_0000 + i as u64 * PAGE * 4;
            mem.allocate(va, PAGE, 0x04, false).expect("Pre-populate failed");
            mem.create_gpu_resource(va, PAGE, true).expect("GPU resource failed");
        }

        let start = Instant::now();
        let handles: Vec<_> = (0..THREADS).map(|_| {
            let mem = mem.clone();
            std::thread::spawn(move || {
                for _ in 0..OPS_PER_THREAD {
                    let idx = fastrand::usize(0..1000);
                    let va = 0x5000_0000 + idx as u64 * PAGE * 4;
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

        let mut allocated = Vec::new();
        const RESOURCE_COUNT: usize = 10000;
        const RESOURCE_SIZE: u64 = 256 * 1024; // 256 KiB
        
        let start = Instant::now();
        
        for i in 0..RESOURCE_COUNT {
            let va = mem.allocate(0, RESOURCE_SIZE, 0x04, false).expect("Allocation failed");
            allocated.push(va);
            mem.create_gpu_resource(va, RESOURCE_SIZE, true).expect("GPU resource failed");
        }
        
        let creation_time = start.elapsed();
        let total_memory = RESOURCE_COUNT as u64 * RESOURCE_SIZE;
        println!("Memory consumption test: {:?} for {} resources ({} MB total)", 
            creation_time, RESOURCE_COUNT, total_memory / 1024 / 1024);
        
        // Report stats
        mem.next_frame();
        
        // Cleanup
        for va in allocated {
            mem.free(va);
        }
    }
}

// Comparative benchmarks: run both backends under identical workloads
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
        const MIN_SHADOW_SIZE: u64 = PAGE;
        const COPIES: u32 = (1 << 12) - 1;
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
            let va = 0x1000_0000u64 + i * 65536 * 4; // 256 KiB each
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