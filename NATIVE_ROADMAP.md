# Native Graphics Roadmap — Phase 3 Implementation Status

Branch: `native-graphics-v1` (based on `native-shaders-v1`)

## ✅ Phase 3 Implemented & Committed

### Command Allocator Pool (`host/command_pool.rs`)
- `CommandPool` with separate pools for DIRECT (graphics), COPY, COMPUTE queues
- `PooledAllocatorHandle` for allocator reuse after GPU completion
- Fence-based tracking: allocators returned to pool only after fence signals
- Statistics: created vs reused counts per queue type
- Integrated into `Host::new_list()` and `Host::make_list()`

### Persistent Descriptor Heap Management (`host/descriptor_pool.rs`)
- `DescriptorHeapPool` per heap type: CBV/SRV/UAV (1M/heap), Sampler (64K/heap), RTV/DSV (4K/heap)
- Free block tracking with coalescing on free
- Shader-visible heaps for CBV/SRV/UAV and Sampler; non-visible for RTV/DSV
- `allocate_*/free_*` helpers for each type
- Statistics: heaps created, allocations, deallocations, peak usage

### Direct Resource Binding Optimizer (`host/binding.rs`)
- `BindingConfig` with `prefer_direct`, `enable_bounds_check`, `max_table_descriptors`
- Native default: `prefer_direct=true`, `enable_bounds_check=false`
- Compatibility mode: `prefer_direct=false`, `enable_bounds_check=true`
- `ResourceBinding` enum: `DirectRootDescriptor`, `DescriptorTable`, `StaticSampler`
- `BoundedResourceRegistry` for tracking resources with known bounds
- `can_use_direct_binding()` checks registry + config
- Integrated into `bounded_root_srvs()` for pipeline creation

### Redundant Resource Barrier Elimination (`host/barrier.rs`)
- `BarrierTracker` with per-resource state tracking (global + per-subresource)
- `ResourceState` enum matching D3D12_RESOURCE_STATES
- `emit_barriers(required)` only emits transitions when state actually changes
- Statistics: barriers emitted, barriers eliminated, state transitions, unknown resources
- Cleanup of old entries after submission threshold

### Async Queue Manager (`host/queue.rs`)
- `QueueManager` with Graphics, Compute (if supported), Copy queues
- Separate fences per queue for independent progress
- Cross-queue synchronization: `signal_and_wait(signal_queue, wait_queue)`
- Timeline semaphore support detection (D3D12_FEATURE_D3D12_OPTIONS7)
- Statistics: submissions per queue type, cross-queue syncs, fence waits

### Budget-Aware VRAM/Shared Memory Manager (`host/memory_mgr.rs`)
- `MemoryBudget` config: budgets, target ratios (85% VRAM, 90% shared), reserves
- Automatic budget query from DXGI adapter (local/non-local segments)
- `TrackedGpuResource` with size, VRAM/shared classification, LRU tracking
- `evict_lru()` evicts least-recently-used non-persistent resources
- `try_allocate()` checks budget before allocation, triggers eviction
- Periodic `update_budgets()` from adapter
- Statistics: usage, budget, evictions, evicted bytes, allocation failures

### Integration with Native Memory (`native_mem.rs`)
- `create_gpu_resource()` and `create_gpu_texture()` now call `track_gpu_resource()`
- `free()` calls `untrack_gpu_resource()` before deferred destruction
- Resources tracked with VRAM classification for budget management

### Native Shaders Integration
- Permutation tracking already integrated in Phase 2 (`shader_perm.rs`)
- `record_translation()` at shader translation time
- `record_pipeline_shader()` + `mark_pso_created()` at PSO creation
- Translation fingerprint includes all Phase 3 settings (binding config, RDNA, wave)

## 📋 Regression Tests Added

### Command Pool Tests (`host/command_pool.rs`)
- `test_command_pool_creation` — verifies pool creation, allocator/list retrieval

### Descriptor Pool Tests (`host/descriptor_pool.rs`)
- `test_descriptor_pool_basic` — allocation, reallocation after free, heap properties

### Binding Tests (`host/binding.rs`)
- `test_binding_config_defaults` — native vs compatibility config
- `test_resource_binding_enum` — enum variants

### Barrier Tests (`host/barrier.rs`)
- `test_resource_state_conversion` — all ResourceState → D3D12 mapping
- `test_barrier_tracker_basic` — tracker creation, submission, stats

### Queue Tests (`host/queue.rs`)
- `test_queue_type_enum` — QueueType values
- `test_queue_stats_default` — stats initialization

### Memory Manager Tests (`host/memory_mgr.rs`)
- `test_memory_budget_default` — default budget values
- `test_memory_stats_default` — stats initialization

### Synthetic Rendering Tests (`tests/synthetic_rendering_test.rs`)
| Test | Description | GPU Readback |
|------|-------------|--------------|
| `test_lighting_stability_across_frames` | Permutation tracker determinism across 5 frames | No (tracker API) |
| `test_shadow_depth_comparison` | R32_FLOAT depth texture, DEPTH_WRITE→COPY_SOURCE→readback→DEPTH_WRITE | **Yes** (256×256) |
| `test_reflection_sampling_bindings` | 6-face cubemap 512×512 R16G16B16A16_FLOAT + UAV | No (descriptor verification) |
| `test_wave32_wave64_equivalence` | Translation fingerprint determinism, includes wave settings | No |
| `test_rdna_float_on_off` | `TEST_RDNA` thread-local toggle, fingerprint differs, all 4 bits validated | No |
| `test_cpu_gpu_resource_coherence` | Upload `i ^ 0xDEADBEEF` pattern → copy → readback → assert 256K u32 exact match | **Yes** (1 MiB) |
| `test_pso_precompile_and_cache` | Real `Host::pipeline()` with `stub_vs()`/`stub_ps()`, permutation tracking | No |
| `test_resource_transition_barriers` | Full chain: UNORDERED_ACCESS→COPY_SOURCE→COPY_DEST→GENERIC_READ on copy queue | No (device removal check) |

### Benchmarks (`benches/shaders_bench.rs`)
- `cold_translation` — first-time shader→pipeline latency (100 iters)
- `warm_translation` — cached shader→pipeline latency (1000 iters)
- `permutation_deduplication` — 50 unique + 20 duplicates each
- `pso_creation` — first run vs second run with persistent cache
- `cache_hit_rate` — working sets 10/50/100/200, 80/20 hit/miss pattern
- `cpu_overhead` — avg pipeline creation cost
- `ram_consumption` — 1000 pipelines, estimated metadata RAM

## ⏳ Incomplete / Unverified (Windows CI Required)

### CI Validation Pending
| Item | Status | Notes |
|------|--------|-------|
| Windows CI compilation | **Not yet run** | Pushed to native-graphics-v1; CI triggered |
| Windows CI test execution | **Not yet run** | Requires `native-memory` feature + GPU runner |
| Actual benchmark measurements | **Not yet run** | `cargo bench -p victory-gpu --features native-memory` on Windows |
| PSO library persistence across process restart | **Unverified** | Tests use in-memory cache; full persistence needs process restart test |
| Offline manifest import → PSO precompile end-to-end | **Unverified** | CLI commands wired; manifest format validated; runtime reuse untested |
| RDNA_FLOAT on/off rendering comparison | **Unverified** | Unit test verifies fingerprint change; pixel-equivalence needs GPU |
| Wave32/64 translated shader equivalence | **Unverified** | Fingerprint test only; actual shader diff needs DXIL compare |
| Async compute queue creation | **Unverified** | Depends on D3D12_FEATURE_D3D12_OPTIONS support |
| Timeline semaphore cross-queue sync | **Unverified** | Requires SM 6.6 support |
| Memory budget eviction under pressure | **Unverified** | Requires sustained GPU memory pressure |

### Known Gaps
1. **No Windows hardware in this sandbox** — cannot run D3D12 tests locally
2. **GitHub Actions Windows runners** — only validation path; may lack GPU for full render tests
3. **`victory-shader::emit::host::stub_vs/stub_ps`** — minimal shaders; real Xbox shader translation not exercised in tests
4. **Permutation tracker import path** — `import_permutations()` exists but not stress-tested with large manifests
5. **PSO library eviction/trim** — not benchmarked; VRAM bounds via `--limit` not fully implemented
6. **Cross-lane operation detection** — `cross_lane` field in key always `false` (needs IR analysis)
7. **Descriptor pool exhaustion** — no test for allocation failure when heap full
8. **Barrier tracker cross-submission state** — state persists across submissions but cleanup threshold not tuned

## 📋 Next Steps (Post-CI)

1. **Monitor Windows CI** — fix any compilation errors in `victory-gpu` (windows-sys, feature gating)
2. **If CI passes:** Run benchmarks manually on Windows hardware with RX 6600 XT
3. **Add missing test coverage:**
   - Large manifest import (10K+ permutations)
   - Process-restart PSO library reuse
   - Real Xbox shader container translation (not stubs)
   - Wave32/64 DXIL diff validation
   - Descriptor pool stress test (exhaustion, fragmentation)
   - Barrier tracker with real draw/dispatch workloads
   - Async compute queue with actual compute shaders
   - Memory manager under sustained pressure
4. **Optimize based on measurements:**
   - If cold translation >100ms: investigate parallelization
   - If cache hit rate <90%: tune permutation key
   - If RAM >500MB: add LRU eviction to tracker
   - If barrier elimination <50%: expand state tracking
5. **Document measured results** in this file

## Commands to Run on Windows

```bash
# Build with native memory backend
cargo build -p victory --features native-memory

# Run synthetic rendering tests
cargo test -p victory-gpu --features native-memory synthetic_rendering

# Run unit tests for Phase 3 components
cargo test -p victory-gpu --features native-memory host::command_pool
cargo test -p victory-gpu --features native-memory host::descriptor_pool
cargo test -p victory-gpu --features native-memory host::binding
cargo test -p victory-gpu --features native-memory host::barrier
cargo test -p victory-gpu --features native-memory host::queue
cargo test -p victory-gpu --features native-memory host::memory_mgr

# Run benchmarks (requires GPU)
cargo bench -p victory-gpu --features native-memory

# Offline precompile
victory shaders precompile <game_dir> --manifest cache/permutations.json
victory shaders pso-precompile --manifest cache/permutations.json --limit 1000
```

---

**Last updated:** 2026-11-27 (commit 2a7759c)
**CI:** Check GitHub Actions for native-graphics-v1 branch