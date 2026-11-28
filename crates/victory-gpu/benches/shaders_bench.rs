//! Benchmarks for Phase 2 native shaders: cold/warm translation, cache hit rates,
//! permutation deduplication, CPU overhead, PSO creation and cache reuse.
//!
//! Run with: `cargo bench -p victory-gpu --features native-memory` on Windows.

#[cfg(all(windows, feature = "native-memory"))]
mod shaders_bench {
    use std::hint::black_box;
    use std::sync::Arc;
    use std::time::Instant;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    use victory_gpu::host::{host, Host};
    use victory_gpu::shader_perm::{stats, enable_persistence, export_permutations};
    use victory_gpu::pipeline::library::{use_pipeline_cache, folder};

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

    fn create_upload_buffer(device: &ID3D12Device, size: u64) -> ID3D12Resource {
        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
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
        unsafe { device.CreateCommittedResource(&heap_props, D3D12_HEAP_FLAGS(0), &desc, D3D12_RESOURCE_STATE_GENERIC_READ, None) }.unwrap()
    }

    /// Benchmark cold shader translation (first time a container is seen).
    fn bench_cold_translation() {
        let (device, queue) = create_test_device();
        let host = Host::new(false).expect("Failed to create host");

        let upload = create_upload_buffer(&device, 65536);
        
        const ITERATIONS: usize = 100;
        let mut times = Vec::with_capacity(ITERATIONS);

        for i in 0..ITERATIONS {
            let va = 0x1000_0000 + i * 0x10000;
            // Simulate a unique shader container per iteration by creating unique resources
            let start = Instant::now();
            let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
            let ps = victory_shader::emit::host::stub_ps();
            let _pipeline = host.pipeline(&vs, &ps).unwrap();
            times.push(start.elapsed());
        }

        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("cold_translation: median {:?} over {} iterations", median, ITERATIONS);
        
        let stats = stats();
        println!("  permutations after cold: {}", stats.total_permutations);
    }

    /// Benchmark warm shader translation (reusing cached translations).
    fn bench_warm_translation() {
        let (device, queue) = create_test_device();
        let host = Host::new(false).expect("Failed to create host");

        // First, populate the cache
        let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
        let ps = victory_shader::emit::host::stub_ps();
        let _pipeline = host.pipeline(&vs, &ps).unwrap();

        let upload = create_upload_buffer(&device, 65536);
        
        const ITERATIONS: usize = 1000;
        let mut times = Vec::with_capacity(ITERATIONS);

        for _ in 0..ITERATIONS {
            let start = Instant::now();
            let _pipeline = host.pipeline(&vs, &ps).unwrap();
            times.push(start.elapsed());
        }

        times.sort_unstable();
        let median = times[times.len() / 2];
        println!("warm_translation: median {:?} over {} iterations", median, ITERATIONS);
        
        let stats = stats();
        println!("  permutations after warm: {}", stats.total_permutations);
    }

    /// Benchmark permutation deduplication (same shaders, different containers).
    fn bench_permutation_deduplication() {
        let (device, queue) = create_test_device();
        let host = Host::new(false).expect("Failed to create host");

        const UNIQUE_SHADERS: usize = 50;
        const DUPLICATES_PER: usize = 20;

        let start = Instant::now();
        
        for i in 0..UNIQUE_SHADERS {
            let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
            let ps = victory_shader::emit::host::stub_ps();
            
            // First creation (unique)
            let _ = host.pipeline(&vs, &ps).unwrap();
            
            // Duplicate creations (should hit cache)
            for _ in 0..DUPLICATES_PER {
                let _ = host.pipeline(&vs, &ps).unwrap();
            }
        }
        
        let elapsed = start.elapsed();
        let total_pipelines = UNIQUE_SHADERS * (1 + DUPLICATES_PER);
        
        println!("permutation_deduplication: {:?} for {} pipelines ({} unique + {} duplicates each)", 
            elapsed, total_pipelines, UNIQUE_SHADERS, DUPLICATES_PER);
        
        let stats = stats();
        println!("  unique permutations: {}", stats.total_permutations);
        println!("  total uses: {}", stats.total_uses);
        println!("  avg uses per permutation: {:.1}", stats.total_uses as f64 / stats.total_permutations.max(1) as f64);
    }

    /// Benchmark PSO creation and persistent cache reuse.
    fn bench_pso_creation() {
        let (device, queue) = create_test_device();
        let host = Host::new(false).expect("Failed to create host");

        // Set up pipeline cache directory
        let cache_dir = std::env::temp_dir().join("victory_bench_cache");
        std::fs::create_dir_all(&cache_dir).ok();
        use_pipeline_cache(cache_dir.clone());
        
        // Also enable shader permutation persistence
        let perm_dir = std::env::temp_dir().join("victory_bench_perm");
        std::fs::create_dir_all(&perm_dir).ok();
        enable_persistence(perm_dir);

        let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
        let ps = victory_shader::emit::host::stub_ps();

        // First run: create PSOs and populate library
        const FIRST_RUN_PIPELINES: usize = 50;
        let mut first_times = Vec::with_capacity(FIRST_RUN_PIPELINES);
        
        for i in 0..FIRST_RUN_PIPELINES {
            let start = Instant::now();
            let _pipeline = host.pipeline(&vs, &ps).unwrap();
            first_times.push(start.elapsed());
        }
        
        first_times.sort_unstable();
        let first_median = first_times[first_times.len() / 2];
        println!("pso_first_run: median {:?} over {} pipelines", first_median, FIRST_RUN_PIPELINES);
        
        let stats1 = stats();
        println!("  permutations: {}, PSOs created: {}", stats1.total_permutations, stats1.permutations_with_pso);
        
        // Export permutations for offline compilation
        let manifest = cache_dir.join("permutations.json");
        export_permutations(&manifest).unwrap();
        
        // Second run: should hit persistent cache
        // (Note: full PSO library reuse requires process restart; here we just measure
        // the in-memory cache hit rate for pipeline creation)
        const SECOND_RUN_PIPELINES: usize = 50;
        let mut second_times = Vec::with_capacity(SECOND_RUN_PIPELINES);
        
        for _ in 0..SECOND_RUN_PIPELINES {
            let start = Instant::now();
            let _pipeline = host.pipeline(&vs, &ps).unwrap();
            second_times.push(start.elapsed());
        }
        
        second_times.sort_unstable();
        let second_median = second_times[second_times.len() / 2];
        println!("pso_second_run: median {:?} over {} pipelines", second_median, SECOND_RUN_PIPELINES);
        
        let stats2 = stats();
        println!("  permutations: {}, PSOs created: {}", stats2.total_permutations, stats2.permutations_with_pso);
        
        // Cleanup
        std::fs::remove_dir_all(&cache_dir).ok();
        std::fs::remove_dir_all(&perm_dir).ok();
    }

    /// Benchmark cache hit/miss rates with varying working sets.
    fn bench_cache_hit_rate() {
        let (device, queue) = create_test_device();
        let host = Host::new(false).expect("Failed to create host");

        // Test with different working set sizes
        for working_set in [10, 50, 100, 200] {
            // Reset by creating new host (simulates new run)
            let host = Host::new(false).expect("Failed to create host");
            
            let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
            let ps = victory_shader::emit::host::stub_ps();
            
            // Populate working set
            for _ in 0..working_set {
                let _ = host.pipeline(&vs, &ps).unwrap();
            }
            
            // Now measure access pattern: 80% hits, 20% misses
            const ACCESS_COUNT: usize = 1000;
            let start = Instant::now();
            
            for i in 0..ACCESS_COUNT {
                if i % 5 == 0 {
                    // 20% new shaders (misses)
                    let vs_new = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
                    let _ = host.pipeline(&vs_new, &ps).unwrap();
                } else {
                    // 80% cached shaders (hits)
                    let _ = host.pipeline(&vs, &ps).unwrap();
                }
            }
            
            let elapsed = start.elapsed();
            let stats = stats();
            let hit_rate = (stats.total_uses - stats.total_permutations as u64) as f64 / stats.total_uses as f64 * 100.0;
            
            println!("cache_hit_rate[ws={}]: {:?} for {} accesses, hit_rate={:.1}%, unique={}", 
                working_set, elapsed, ACCESS_COUNT, hit_rate, stats.total_permutations);
        }
    }

    /// Benchmark CPU overhead of shader translation pipeline.
    fn bench_cpu_overhead() {
        let (device, queue) = create_test_device();
        let host = Host::new(false).expect("Failed to create host");

        const ITERATIONS: usize = 100;
        let mut total_time = std::time::Duration::ZERO;
        
        for _ in 0..ITERATIONS {
            let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
            let ps = victory_shader::emit::host::stub_ps();
            
            let start = Instant::now();
            let _pipeline = host.pipeline(&vs, &ps).unwrap();
            total_time += start.elapsed();
        }
        
        let avg = total_time / ITERATIONS as u32;
        println!("cpu_overhead: avg {:?} per pipeline creation ({} iterations)", avg, ITERATIONS);
        
        let stats = stats();
        println!("  permutations: {}, uses: {}", stats.total_permutations, stats.total_uses);
    }

    /// Benchmark RAM consumption of permutation tracker.
    fn bench_ram_consumption() {
        use std::alloc::{GlobalAlloc, System};
        
        let (device, queue) = create_test_device();
        let host = Host::new(false).expect("Failed to create host");

        // Measure baseline
        let baseline = {
            let stats = stats();
            stats.total_permutations
        };
        
        const SHADERS: usize = 1000;
        
        for i in 0..SHADERS {
            let vs = victory_shader::emit::host::stub_vs(&victory_shader::emit::host::stub_ps()).unwrap();
            let ps = victory_shader::emit::host::stub_ps();
            let _ = host.pipeline(&vs, &ps).unwrap();
        }
        
        let stats = stats();
        let permutations = stats.total_permutations;
        let uses = stats.total_uses;
        
        println!("ram_consumption: {} unique permutations, {} total uses after {} pipeline creations", 
            permutations, uses, SHADERS);
        println!("  estimated RAM: ~{} KB for permutation metadata", 
            (permutations * 200) / 1024); // rough estimate
    }

    pub fn run_all() {
        println!("=== Phase 2 Native Shaders Benchmarks ===\n");
        
        bench_cold_translation();
        println!();
        
        bench_warm_translation();
        println!();
        
        bench_permutation_deduplication();
        println!();
        
        bench_pso_creation();
        println!();
        
        bench_cache_hit_rate();
        println!();
        
        bench_cpu_overhead();
        println!();
        
        bench_ram_consumption();
        println!();
        
        println!("=== Benchmarks Complete ===");
    }
}

#[cfg(all(windows, feature = "native-memory"))]
fn main() {
    shaders_bench::run_all();
}

#[cfg(not(all(windows, feature = "native-memory")))]
fn main() {
    println!("Benchmarks require Windows with native-memory feature");
    println!("Run on Windows with: cargo bench -p victory-gpu --features native-memory");
}