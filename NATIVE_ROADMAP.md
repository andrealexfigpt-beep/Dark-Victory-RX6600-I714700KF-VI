# Native Runtime Roadmap — Phase 5 Implementation Status

Branch: `native-runtime-v1` (based on `native-platform-v1`)

## ✅ Phase 5 Implemented & Committed

### Unified Native Runtime Initialization (`native_runtime.rs`)
- **NativeRuntime** struct coordinating all native subsystems
- **Initialization order** (dependencies first):
  1. Hardware capability detection (GPU vendor, VRAM, wave lanes, RDNA2, mesh shaders, depth bounds)
  2. Console timers & process priority (`use_console_timers`, above-normal priority)
  3. Native Memory (memory_mgr, native_mem, copy queue)
  4. Native Shaders (shader cache, permutation persistence, translation cache)
  5. Native Graphics (descriptor pools, command pool, barriers, async queues, binding config)
  6. Native Platform (kernel, files, XAudio2, GameInput, DirectStorage via HLE modules)
  7. Background task scheduler (budget-aware, priority queue)
  8. Profiling (periodic snapshots, subsystem breakdown)

### A/B Execution Modes
- **Native** (default): Full native implementation with all optimizations
- **Legacy**: Fallback mode for compatibility
- **Hybrid**: Native where validated, legacy elsewhere
- Runtime switching via `set_execution_mode()` / `execution_mode()`
- Used for gradual rollout and regression isolation

### Runtime Profiling
- **RuntimeProfile** with per-subsystem breakdown:
  - Memory (RAM, VRAM, evictions, allocations)
  - Shader (cache hits/misses, compilation time)
  - Graphics (GPU time, sync wait, draw calls)
  - Platform (I/O, audio, input)
  - Background (task scheduling, memory pressure)
- Periodic collection (configurable interval, default 100ms)
- History retention (last 1000 snapshots)
- Access via `latest_profile()` / `profiling_history()`

### Budget-Aware Streaming & Background Task Scheduling
- **BackgroundTask** with priority, memory estimate, deadline
- **Thread pool** with configurable max threads (default 4)
- **Per-task budget** enforcement (default 16ms per frame)
- **Streaming budget** (default 512 MB)
- `schedule_background_task()` API for async work

### Coordinated Resource Lifetime & Shutdown
- Graceful shutdown sequence: background thread → profiling → subsystems
- `shutdown_requested` flag for cooperative cancellation
- Resource cleanup order (reverse of initialization)
- `shutdown_runtime()` for graceful termination

### Automatic Hardware Capability Detection
- GPU vendor/device ID (AMD 0x1002 detection for RDNA)
- VRAM & shared memory budgets via DXGI
- Wave lane count (min/max) for wave32/64 handling
- Mesh shader support (D3D12_OPTIONS7)
- Depth bounds test support (D3D12_OPTIONS2)
- RDNA2 detection (Navi 2x device IDs 0x7300-0x73FF)
- CPU core count & logical processors

### Integration Tests (`tests/native_runtime_integration_test.rs`)
| Test | Description |
|------|-------------|
| `test_runtime_initialization` | Full init, hardware caps, mode check |
| `test_execution_mode_switching` | Native ↔ Legacy ↔ Hybrid |
| `test_background_task_scheduling` | Task scheduling with budget enforcement |
| `test_profiling_collection` | Periodic profile snapshots |
| `test_memory_budget_status` | VRAM/shared budget queries |
| `test_multiple_initialization_fails` | Idempotency protection |

### Unit Tests (`native_runtime.rs`)
- `test_execution_mode_variants` — enum values
- `test_runtime_config_defaults` — defaults validation
- `test_hardware_capabilities_default` — struct defaults
- `test_subsystem_profile_default` — profiling struct defaults
- `test_runtime_profile_default` — full profile defaults

## 📋 Regression Tests Added (All Phases)

### Phase 2 (Native Shaders)
- `emit::rdna_tests::rdna_constants_are_powers_and_all`
- `emit::rdna_equiv_tests::rdna_all_bits_is_sum_of_parts` + `rdna_bits_are_distinct`
- Shader permutation tracking integration tests

### Phase 3 (Native Graphics)
- `host::command_pool::test_command_pool_creation`
- `host::descriptor_pool::test_descriptor_pool_basic`
- `host::binding::test_binding_config_defaults` + `test_resource_binding_enum`
- `host::barrier::test_resource_state_conversion` + `test_barrier_tracker_basic`
- `host::queue::test_queue_type_enum` + `test_queue_stats_default`
- `host::memory_mgr::test_memory_budget_default` + `test_memory_stats_default`
- `tests/synthetic_rendering_test.rs`: 8 tests (2 with GPU readback)
- `tests/phase4_verification_test.rs`: 9 verification tests

### Phase 4 (Native Platform)
- `host::kernel` thread/sync/timer/virtual memory tests
- `host::files` async I/O tests
- `host::xaudio2` audio tests

### Phase 5 (Native Runtime)
- `tests/native_runtime_integration_test.rs`: 6 integration tests
- `native_runtime.rs`: 5 unit tests

## ⏳ Incomplete / Unverified (Windows CI Required)

### CI Validation Pending
| Item | Status | Notes |
|------|--------|-------|
| Windows CI compilation | **Not yet run** | Pushed to native-runtime-v1; CI triggered |
| Windows CI test execution | **Not yet run** | Requires `native-memory` feature + GPU runner |
| Actual benchmark measurements | **Not yet run** | `cargo bench -p victory-gpu --features native-memory` on Windows |
| End-to-end title execution | **Unverified** | Requires full title boot with native runtime |
| A/B mode switching under load | **Unverified** | Requires title with mixed native/legacy paths |
| Profiling overhead measurement | **Unverified** | Needs CPU/GPU timing with/without profiling |

### Known Gaps
1. **No Windows hardware in this sandbox** — cannot run D3D12/XAudio2 tests locally
2. **GitHub Actions Windows runners** — only validation path; may lack GPU for full render tests
3. **XAudio2 CreateFX** — returns E_NOTIMPL; effect support not implemented
4. **X3DAudio** — structures defined but calculation forwards to host (zeroed output)
5. **VirtualAllocEx** — forwards to host but process handle validation minimal
6. **System resource queries** — some return host values; may not match console expectations
7. **NativeRuntime shutdown** — OnceLock prevents true cleanup; only signals shutdown

## 📋 Next Steps (Post-CI)

1. **Monitor Windows CI** — fix any compilation errors in `victory-hle` / `victory-gpu`
2. **If CI passes:** Run Phase 5 integration tests on RX 6600 XT hardware
3. **Add missing test coverage:**
   - End-to-end title boot with native runtime
   - Profiling overhead benchmarks (with/without)
   - Background task stress test (1000+ concurrent tasks)
   - Memory budget pressure test (sustained allocation)
   - A/B mode switching during title execution
8. **Optimize based on measurements:**
   - If profiling overhead >1%: reduce collection frequency
   - If background task latency >budget: optimize scheduler
   - If hardware detection fails: add fallback paths
4. **Document measured results** in this file

## Commands to Run on Windows

```bash
# Build with native memory backend
cargo build -p victory --features native-memory

# Run Phase 3 synthetic rendering tests
cargo test -p victory-gpu --features native-memory synthetic_rendering

# Run Phase 4 verification tests
cargo test -p victory-gpu --features native-memory phase4_verification

# Run Phase 5 native runtime integration tests
cargo test -p victory-hle --features native-memory native_runtime_integration

# Run unit tests for all phases
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

# Offline precompile
victory shaders precompile <game_dir> --manifest cache/permutations.json
victory shaders pso-precompile --manifest cache/permutations.json --limit 1000
```

---

**Last updated:** 2026-11-27 (commit fd510c9)
**CI:** Check GitHub Actions for native-runtime-v1 branch