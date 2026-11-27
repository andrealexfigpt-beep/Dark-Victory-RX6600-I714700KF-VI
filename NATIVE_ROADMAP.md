# Native Shaders Roadmap — Phase 2 Implementation Status

Branch: `native-shaders-v1` (based on `native-memory-v1`)

## ✅ Implemented & Committed

### Shader Permutation Management (`shader_perm.rs`)
- `ShaderPermutationKey` — tracks all translation-affecting parameters (stage, container hash, translation fingerprint, root signature hash, wave mismatch, cross-lane)
- `ShaderPermutation` — stores translated DXIL + binding layout + usage statistics
- `PermutationTracker` — global deduplication, pipeline-shader relationship recording, PSO creation marking
- Export/import JSON manifests for offline compilation
- Persistence via `enable_persistence(dir)`
- Frame advancement via `next_frame()`
- Statistics: `total_permutations`, `permutations_with_pso`, `total_uses`

### Pipeline Integration
- `record_translation()` called in `pipeline.rs::translate()` at shader translation time
- `record_pipeline_shader()` + `mark_pso_created()` in `HostPipeline::new()` (compute + graphics paths)
- `compute_root_signature_hash()` made `pub(crate)` for cross-module access

### Offline Compilation CLI
- `victory shaders precompile [paths] --jobs N --manifest <path>` — translates all found shaders into cache, exports permutation manifest
- `victory shaders pso-precompile --manifest <path> --jobs N --limit M` — validates/imports manifest, reports diagnostics, bounds RAM/VRAM via `--limit`
- Reuses existing `victory-pipeline` content-addressed cache

### RDNA2 Optimization Paths
- Automatic hardware detection: `set_host_rdna(amd && WaveLaneCountMin==32 && WaveLaneCountMax==64)` → enables for RX 6600 XT (Navi 23)
- `VICTORY_RDNA_FLOAT` bits: MAX_MIN(1), LEGACY(2), MED3(4), FRACT(8), ALL(15)
- `host_settings()` includes `host_rdna()` → feeds translation fingerprint → cache keys on RDNA setting
- Generic fallback: `HOST_RDNA=false` spells out all operations (IEEE-compliant)

### Synthetic Rendering Tests (`synthetic_rendering_test.rs`)
| Test | Description | GPU Readback |
|------|-------------|--------------|
| `test_lighting_stability_across_frames` | Permutation tracker determinism across 5 frames | No (tracker API only) |
| `test_shadow_depth_comparison` | R32_FLOAT depth texture, DEPTH_WRITE→COPY_SOURCE→readback→DEPTH_WRITE | **Yes** (256×256 depth buffer) |
| `test_reflection_sampling_bindings` | 6-face cubemap 512×512 R16G16B16A16_FLOAT + UAV | No (descriptor verification) |
| `test_wave32_wave64_equivalence` | Translation fingerprint determinism, includes wave settings | No |
| `test_rdna_float_on_off` | `TEST_RDNA` thread-local toggle, fingerprint differs, all 4 bits validated | No |
| `test_cpu_gpu_resource_coherence` | Upload `i ^ 0xDEADBEEF` pattern → copy → readback → assert 256K u32 exact match | **Yes** (1 MiB buffer) |
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

### Unit Tests
- `emit::rdna_tests::rdna_constants_are_powers_and_all` — bit constants correct
- `emit::rdna_equiv_tests::rdna_all_bits_is_sum_of_parts` + `rdna_bits_are_distinct`

## ⏳ Incomplete / Unverified (Windows CI Required)

### CI Validation Pending
| Item | Status | Notes |
|------|--------|-------|
| Windows CI compilation | **Not yet run** | Pushed to native-shaders-v1; CI triggered |
| Windows CI test execution | **Not yet run** | Requires `native-memory` feature + GPU runner |
| Actual benchmark measurements | **Not yet run** | `cargo bench -p victory-gpu --features native-memory` on Windows |
| PSO library persistence across process restart | **Unverified** | Tests use in-memory cache; full persistence needs process restart test |
| Offline manifest import → PSO precompile end-to-end | **Unverified** | CLI commands wired; manifest format validated; runtime reuse untested |
| RDNA_FLOAT on/off rendering comparison | **Unverified** | Unit test verifies fingerprint change; pixel-equivalence needs GPU |
| Wave32/64 translated shader equivalence | **Unverified** | Fingerprint test only; actual shader diff needs DXIL compare |

### Known Gaps
1. **No Windows hardware in this sandbox** — cannot run D3D12 tests locally
2. **GitHub Actions Windows runners** — only validation path; may lack GPU for full render tests
3. **`victory-shader::emit::host::stub_vs/stub_ps`** — minimal shaders; real Xbox shader translation not exercised in tests
4. **Permutation tracker import path** — `import_permutations()` exists but not stress-tested with large manifests
5. **PSO library eviction/trim** — not benchmarked; VRAM bounds via `--limit` not fully implemented
6. **Cross-lane operation detection** — `cross_lane` field in key always `false` (needs IR analysis)

## 📋 Next Steps (Post-CI)

1. **Monitor Windows CI** — fix any compilation errors in `victory-gpu` (windows-sys, feature gating)
2. **If CI passes:** Run benchmarks manually on Windows hardware with RX 6600 XT
3. **Add missing test coverage:**
   - Large manifest import (10K+ permutations)
   - Process-restart PSO library reuse
   - Real Xbox shader container translation (not stubs)
   - Wave32/64 DXIL diff validation
4. **Optimize based on measurements:**
   - If cold translation >100ms: investigate parallelization
   - If cache hit rate <90%: tune permutation key
   - If RAM >500MB: add LRU eviction to tracker
5. **Document measured results** in this file

## 🚫 Not Started (Phase 3+)

- Runtime fallback for previously unknown shaders (infrastructure exists, not tested)
- Shader equivalence tests with actual Xbox title shaders
- Automated regression detection for lighting/shadow/reflection
- Integration with `victory` run command for end-to-end validation

## Commands to Run on Windows

```bash
# Build with native memory backend
cargo build -p victory --features native-memory

# Run synthetic rendering tests
cargo test -p victory-gpu --features native-memory synthetic_rendering

# Run benchmarks (requires GPU)
cargo bench -p victory-gpu --features native-memory

# Offline precompile
victory shaders precompile <game_dir> --manifest cache/permutations.json
victory shaders pso-precompile --manifest cache/permutations.json --limit 1000
```

---

**Last updated:** 2026-11-27 (commit e1dc650)
**CI:** Check GitHub Actions for native-shaders-v1 branch