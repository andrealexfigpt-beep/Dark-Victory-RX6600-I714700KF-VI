# Native Final Roadmap — Phase 7 Implementation Status

Branch: `native-final-v1` (based on `native-optimized-v1`)

## ✅ Phase 6 Implemented & Committed (native-optimized-v1)

### Budget-Aware VRAM/Shared Memory Allocator (`host/memory_mgr.rs`)
- Multi-pool allocator (Static/Streaming/Temporary/Upload/Readback)
- Priority-based eviction (Critical/High/Normal/Low)
- Fence-safe reclamation with pending queue
- Streaming budget per frame (64MB/frame, 16MB max single alloc)
- ReBAR detection (PCIe Resizable BAR for Navi 2x)
- GPU upload heap support detection
- Predictive eviction based on access patterns

### Redundant Shadow Buffer Elimination
- Direct GPU binding replaces shadow copies in `native_mem.rs`
- Adaptive upload ring in `draw.rs`
- Removed legacy shadow copy indirection

### RDNA2 Shader Optimization (`emit/lower.rs`, `host.rs`, `host/binding.rs`)
- Navi 23 (RX 6600 XT) detection via device ID 0x7300-0x73FF
- MAX_MIN/LEGACY/MED3/FRACT bit optimizations via `VICTORY_RDNA_FLOAT`
- Wave32/64 handling with automatic splitting for cross-lane ops
- `rdna2_binding_optimized()` and `optimal_binding_strategy()` for ReBAR+upload heap paths

### ReBAR & GPU Upload Heap Detection (`host/memory_mgr.rs`, `host.rs`)
- PCIe Resizable BAR detection for Navi 2x (0x7300-0x73FF)
- D3D12 upload heap feature detection via `D3D12_FEATURE_D3D12_OPTIONS`
- `rebar_supported()` and `upload_heap_supported()` runtime queries

### CPU Synchronization Reduction
- Lock-free atomic operations in `command_pool.rs`, `queue.rs`, `barrier.rs`
- Priority-aware barrier elimination (only emit when state actually changes)
- Cross-queue fence synchronization with timeline semaphore support

### Adaptive Worker Thread Pool (`native_runtime.rs`)
- Dynamic scaling based on CPU cores (Ryzen 5600X 6C/12T → 4 threads)
- Exponential backoff idle sleep (10ms → 640ms max)
- Dynamic scaling down when idle, up when busy
- Reserves 1-2 cores for main thread

### Persistent Shader/PSO Cache (`translate.rs`, `shader_perm.rs`)
- `victory shaders pso-precompile` creates actual D3D12 PSOs from permutation manifest
- Proper VS/PS pairing by container hash for graphics pipelines
- Compute and mesh shader precompilation support
- `victory shaders precompile` exports permutation manifest with fingerprint

### Low-Overhead Runtime Profiling (`native_runtime.rs`)
- GPU timestamp queries via `D3D12_QUERY_HEAP_TYPE_TIMESTAMP`
- Lock-free profiling with `try_lock` and minimal critical sections
- Per-subsystem breakdown (memory, shader, graphics, platform, background)
- 1000 snapshot history with lock-free eviction

### Hardware-Specific Configuration (`native_runtime.rs`)
- `RuntimeConfig::for_hardware()` auto-tunes VRAM/streaming/threads
- VRAM-based tiers: ≥8GB (aggressive), 4-8GB (balanced), <4GB (conservative)
- RDNA2-specific: lower background budget, more frequent profiling
- Async compute, mesh shaders, depth bounds, timeline semaphores detection
- CPU core scaling: ≥12 cores = cores/2, 8-11 = cores-2, ≤6 = cores-2

### Subsystem Integration (`native_runtime.rs`)
Verified initialization order:
1. Hardware capability detection
2. Console timers & process priority
2. Native Memory (memory_mgr, native_mem, copy queue)
3. Native Shaders (shader cache, permutation persistence)
4. Native Graphics (descriptor pools, command pool, barriers, async queues)
5. Native Platform (kernel, files, XAudio2, GameInput, DirectStorage)
6. Background task scheduler
7. Profiling

## ✅ Phase 7 - Final Validation (native-final-v1)

### PSO Precompile Fix (`apps/victory/src/translate.rs`)
- Proper VS/PS pairing by container hash for graphics pipelines
- Compute shader direct precompilation
- Mesh shader precompilation with stub PS
- Container hash matching for graphics pipeline pairs

### CI Configuration (`.github/workflows/ci.yml`)
- Added `native-final-v1` branch to trigger CI
- Runs: clippy, clippy all-features, tests, native-memory build/test/benchmarks
- Builds and uploads diagnostic executable + DLLs as artifacts

## 📋 Regression Tests Added (All Phases)

### Phase 2 (Native Shaders)
- `emit::rdna_tests::rdna_constants_are_powers_and_all`
- `emit::rdna_equiv_tests::rdna_all_bits_is_sum_of_parts` + `rdna_bits_are_distinct`

### Phase 3 (Native Graphics)
- `host::command_pool::test_command_pool_creation`
- `host::descriptor_pool::test_descriptor_pool_basic`
- `host::binding::test_binding_config_defaults` + `test_resource_binding_enum`
- `host::barrier::test_resource_state_conversion` + `test_barrier_tracker_basic`
- `host::queue::test_queue_type_enum` + `test_queue_stats_default`
- `host::memory_mgr::test_memory_budget_default` + `test_memory_stats_default`
- `tests/synthetic_rendering_test.rs`: 8 tests (2 with GPU readback)
- `tests/phase4_verification_test.rs`: 9 verification tests
- `tests/phase6_regression_test.rs`: 9 regression tests

### Phase 4 (Native Platform)
- `host::kernel` thread/sync/timer/virtual memory tests
- `host::files` async I/O tests
- `host::xaudio2` audio tests

### Phase 5 (Native Runtime)
- `tests/native_runtime_integration_test.rs`: 6 integration tests
- `native_runtime.rs`: 5 unit tests

### Phase 6 (Native Optimization)
- `tests/phase6_regression_test.rs`: 9 regression tests
- `benches/phase6_bench.rs`: 7 benchmarks

## ⏳ CI Validation Pending

### CI Validation Pending
| Item | Status | Notes |
|------|--------|-------|
| Windows CI compilation | **Running** | Pushed to native-final-v1; CI triggered |
| Windows CI test execution | **Pending** | Requires `native-memory` feature + GPU runner |
| Actual benchmark measurements | **Pending** | `cargo bench -p victory-gpu --features native-memory` on Windows |
| PSO precompile end-to-end | **Pending** | Requires permutation manifest + GPU |
| Real offline PSO creation | **Pending** | Requires GPU + manifest |
| GPU detection accuracy | **Pending** | Device ID matching for Navi 23 |
| Dynamic DXGI budget query | **Pending** | Real adapter budget vs static 8GB |

### Known Gaps
1. **No Windows hardware in this sandbox** — cannot run D3D12/XAudio2 tests locally
2. **GitHub Actions Windows runners** — only validation path; may lack GPU for full render tests
3. **XAudio2 CreateFX** — returns E_NOTIMPL; effect support not implemented
4. **X3DAudio** — structures defined but calculation forwards to host (zeroed output)
5. **VirtualAllocEx** — forwards to host but process handle validation minimal
6. **System resource queries** — some return host values; may not match console expectations
7. **NativeRuntime shutdown** — OnceLock prevents true cleanup; only signals shutdown
8. **PSO cache persistence** — Not tested across process restarts
9. **ReBAR detection** — Uses PCI ID heuristic; should query PCIe capability registers

## 📋 Next Steps (Post-CI)

1. **Monitor Windows CI** — fix any compilation errors in `victory-hle` / `victory-gpu`
2. **If CI passes:** Run Phase 6/7 tests on RX 6600 XT hardware
3. **Validate on actual hardware:**
   - End-to-end title boot with native runtime
   - Offline PSO precompile + cache persistence across restarts
   - ReBAR detection accuracy on Navi 23
   - Dynamic DXGI budget vs static assumptions
4. **Document measured results** in this file

## 🚀 Windows Testing Commands

```powershell
# Build with native memory backend
cargo build -p victory --features native-memory

# Run Phase 6 regression tests
cargo test -p victory-gpu --features native-memory phase6_regression

# Run Phase 7 native runtime integration tests
cargo test -p victory-hle --features native-memory native_runtime_integration

# Run all unit tests
cargo test -p victory-gpu --features native-memory host::command_pool
cargo test -p victory-gpu --features native-memory host::descriptor_pool
cargo test -p victory-gpu --features native-memory host::binding
cargo test -p victory-gpu --features native-memory host::barrier
cargo test -p victory-gpu --features native-memory host::queue
cargo test -p victory-gpu --features native-memory host::memory_mgr
cargo test -p victory-hle --features native-memory kernel
cargo test -p victory-hle --features native-memory files
cargo test -p victory-hle --features native-memory xaudio2
cargo test -p victory-hle --features native-memory native_runtime

# Run benchmarks (requires GPU)
cargo bench -p victory-gpu --features native-memory

# Offline precompile + PSO precompile
victory shaders precompile <game_dir> --manifest cache/permutations.json
victory shaders pso-precompile --manifest cache/permutations.json --limit 1000
```

## 🔬 Diagnostic Build

```powershell
# Build diagnostic executable with native-memory + diagnostics
cargo build --locked --profile diag -p victory --features "native-memory,diagnostics"

# Run with game
.\target\diag\victory.exe <game.exe> [args...]
```

## 📦 Artifacts

- `victory-diag-native-memory`: Diagnostic executable + PDB
- `victory-diag-runtime`: Required DLLs + PDBs

---

**Last updated:** 2026-11-27 (commit 66a5fff)
**CI:** Check GitHub Actions for native-final-v1 branch