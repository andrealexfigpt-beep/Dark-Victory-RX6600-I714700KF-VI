mod comparative_benchmarks {
    use crate::native_mem::{NativeMemory, PAGE};
    fn create_test_device() -> (Arc<windows::Win32::Graphics::Direct3D12::ID3D12Device>, Arc<windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue>) { let host = crate::host::host().unwrap(); (Arc::new(host.device.clone()),host.copy_queue()) }
    use crate::shadow::Shadow;
    use std::collections::HashMap;
    use windows_sys::Win32::System::Memory::*;

    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    #[derive(Default)]
    struct LegacyBaseline {
        allocations: HashMap<u64, usize>,
        shadows: HashMap<u64, Arc<Shadow>>,
    }

    impl LegacyBaseline {
        fn allocate(&mut self, va: u64, size: u64, protection: u32, _: bool) -> anyhow::Result<()> {
            let ptr = unsafe { VirtualAlloc(va as _, size as usize, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, protection) };
            anyhow::ensure!(!ptr.is_null(), "legacy VirtualAlloc failed at {va:#x}");
            crate::title_memory_allocated(va as usize, size as usize);
            self.allocations.insert(va, size as usize);
            Ok(())
        }
        fn free(&mut self, va: u64) {
            let size = self.allocations.remove(&va).unwrap();
            if self.shadows.remove(&va).is_some() { crate::shadow::unregister(va as usize, va); }
            crate::title_memory_freed(va as usize, size);
            assert_ne!(unsafe { VirtualFree(va as _, 0, MEM_RELEASE) }, 0);
        }
        fn create_gpu_resource(&mut self, va: u64, size: u64) {
            crate::shadow::register(va as usize, va, size);
            self.shadows.insert(va, crate::shadow::find(va, size).expect("create legacy shadow"));
        }
        fn upload(&self, va: u64, data: &[u8]) {
            assert!(data.len() <= self.allocations[&va]);
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), va as *mut u8, data.len()); }
            self.shadows[&va].update(va as usize, data.len());
        }
    }
    impl Drop for LegacyBaseline {
        fn drop(&mut self) {
            for va in self.allocations.keys().copied().collect::<Vec<_>>() { self.free(va); }
        }
    }
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
            let va = 0x1000_0000 + i as u64 * PAGE * 4;
            native_mem.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }
        let native_alloc_time = start.elapsed();
        
        // Cleanup native
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i as u64 * PAGE * 4;
            native_mem.free(va);
        }
        
        // Test legacy backend (using the trait)
        let mut legacy_backend = LegacyBaseline::default();
        
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i as u64 * PAGE * 4;
            legacy_backend.allocate(va, PAGE, 0x04, false).expect("Allocation failed");
        }
        let legacy_alloc_time = start.elapsed();
        
        // Cleanup legacy
        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i as u64 * PAGE * 4;
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
        let mut legacy_backend = LegacyBaseline::default();
        let va_legacy = 0x4000_0000;
        legacy_backend.allocate(va_legacy, BUFFER_SIZE, 0x04, false).expect("Allocation failed");
        legacy_backend.create_gpu_resource(va_legacy, BUFFER_SIZE);
        
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
            legacy_backend.upload(va_legacy, &test_data);
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

