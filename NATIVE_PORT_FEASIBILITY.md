# Native Port Feasibility Analysis: Dark Victory

**Status**: Feasibility Study  
**License**: GPL-3.0-or-later (preserved)  
**Date**: 2026-11-27  

---

## Executive Summary

Dark Victory runs Xbox Series titles by loading their x86-64 executables natively on Windows, HLE-ing console APIs, and translating GPU commands to host D3D12 at runtime. The primary architectural overhead stems from **unified memory on console (Xbox Series X|S: CPU/GPU share physical RAM) vs. discrete memory on PC (separate system/VRAM, explicit copies required)**. This mismatch drives:

1. **Write watching & page tracking** (victory-hle xmem, victory-gpu watch.rs, memory.rs)
2. **Shadow copies & arena page directories** (victory-gpu memory.rs: `shadow`, `Directory`, `COPIES=4095`)
3. **Journal synchronization & fault handling** (victory-gpu watch.rs: `Journal`, `ASKING` mutex, guard locks)
4. **Runtime shader translation** (victory-shader: GFX10 → SSA IR → structurisation → DXIL at pipeline creation)

A native port would replace the compatibility layer with **direct Windows implementations**, eliminating the unified↔discrete translation entirely. The title's x86-64 code already runs natively; only the platform abstractions (memory, GPU, audio, input, filesystem) need native backends.

---

## 1. Architecture Comparison

| Component | Dark Victory (Current) | UnleashedRecomp | XenonRecomp / XenosRecomp | N64Recomp / Zelda64Recomp |
|-----------|------------------------|-----------------|---------------------------|---------------------------|
| **CPU Execution** | Native x86-64 (no translation) | Static recompilation (AArch64→x86-64) | Static recompilation (PPC→x86-64) | Static recompilation (MIPS→x86-64) |
| **Memory Model** | Unified→Discrete translation (runtime) | Native host allocations | Native host allocations | Native host allocations |
| **GPU Approach** | HLE + D3D12.X on D3D12 + shader translation | Native API (Vulkan/D3D12) + ubershaders | Native API + ubershaders | Native API + ubershaders |
| **Shader Handling** | Runtime GFX10→DXIL translation + cache | Precompiled/offline translation | Precompiled/offline translation | Precompiled/offline translation |
| **Synchronization** | Page faults, journal locks, guard mutexes | Minimal (host memory model) | Minimal | Minimal |
| **License** | GPL-3.0-or-later | MIT | MIT | MIT |

**Key Insight**: Dark Victory is unique in running **native x86-64 code** (Xbox Series uses Zen 2). Other recomps target different ISAs requiring static recompilation. Dark Victory's overhead is purely **platform adaptation**, not CPU translation.

---

## 2. Unified vs. Discrete Memory Mismatch — Root Cause Analysis

### 2.1 Console Memory Model (Xbox Series X|S)
- **Unified GDDR6**: CPU and GPU access the same physical addresses
- **Title address space**: 4 GiB (2^26 pages × 64 KiB = `MEMORY_PAGES=67,108,864`)
- **No explicit copies**: GPU reads/writes title memory directly
- **Coherency**: Hardware-managed; CPU writes immediately visible to GPU

### 2.2 PC Memory Model
- **Discrete**: System RAM (CPU) + VRAM (GPU), separate physical heaps
- **Explicit copies required**: `ID3D12CommandQueue::CopyBufferRegion`, upload heaps
- **Coherency**: Manual; CPU→GPU via upload heap + fence, GPU→CPU via readback heap
- **Address spaces differ**: GPU virtual addresses ≠ CPU virtual addresses

### 2.3 Dark Victory's Translation Layer (Source References)

#### `crates/victory-hle/src/modules/xmem.rs` — Console Memory Manager
```rust
// Lines 1-50: XMEM emulation — allocates from title's 4 GiB space
// Tracks allocations by address, not by host pointer
// PAGE = 65536 (64 KiB), MEMORY_PAGES = 1 << 26 (4 GiB)
pub const PAGE: u64 = 65536;
pub const MEMORY_PAGES: u32 = 1 << 26; // 4 GiB title address space
```
**Overhead**: Every allocation tracked in a 67M-entry structure; page-level granularity forces 64 KiB tracking even for small allocations.

#### `crates/victory-gpu/src/memory.rs` — Title Memory by Address
```rust
// Lines 1-100: TitleMemory — maps title virtual addresses to host resources
// shadow: Vec<ShadowPage> — COPIES=4095 shadow copies per page for GPU access
// Directory: page table mapping title VA → host allocation + metadata
// arena: ArenaPage allocator (64 MiB arenas) for GPU-visible memory

pub const ARENA: u64 = 64 * 1024 * 1024; // 64 MiB arena size
pub const COPIES: u32 = 4095; // Max shadow copies per page
```
**Redundant Representations**:
- `Directory`: `Vec<PageEntry>` — 67M entries mapping title VA → host state
- `shadow`: `Vec<ShadowPage>` — 4095 copies × pages for concurrent CPU/GPU access
- `arena`: Separate GPU heap allocator below 40-bit address space
- `upload`: `UploadMemory` — staging buffers for CPU→GPU copies

#### `crates/victory-gpu/src/watch.rs` — Write Watching & Journal
```rust
// Lines 1-200: Journal tracks dirty pages for GPU synchronization
// ASKING mutex: serializes page fault handling across threads
// guard locks: per-page mutexes for concurrent access

pub struct Journal {
    pages: Mutex<Vec<DirtyPage>>, // Lock contention on every CPU write
    asking: Mutex<()>, // Global serialization point (ASKING)
}
```
**Synchronization Overhead**:
- `ASKING` mutex: global lock on every page fault (`watch.rs:150-180`)
- Per-page `guard` locks: fine-grained but high volume (`memory.rs:400-450`)
- Journal flush: walks all dirty pages, issues copy commands (`watch.rs:200-280`)

#### `crates/victory-gpu/src/upload.rs` — Upload Memory Arenas
```rust
// Manages upload heaps (CPU-visible, GPU-source) below 40-bit address space
// Arenas of 64 MiB, sub-allocated for staging copies
```
**Copy Overhead**: Every CPU write to title memory that GPU may read → staged via upload heap → copy command queue → fence wait.

### 2.4 Quantified Overhead Sources

| Operation | Current Path | Native Path | Overhead Factor |
|-----------|--------------|-------------|-----------------|
| CPU allocates texture | `xmem` → `Directory` → `Arena` → `shadow` | `D3D12Heap::Create` + `CommittedResource` | ~10× syscalls, locks |
| CPU writes texture data | Write → `Journal` → fault → upload → copy → fence | Direct `Map`/`Unmap` on `UPLOAD` heap | ~5× copies, sync |
| GPU reads texture | `shadow` copy already resident | Direct VRAM access | Zero-copy |
| GPU writes, CPU reads | `shadow` → readback → fault → map | `READBACK` heap → `Map` | ~3× copies |
| Page fault handling | `ASKING` mutex + journal + copy submit | N/A (no faults) | Eliminated |

---

## 3. Redundant Memory Representations

### 3.1 Shadow Copy System (`victory-gpu/src/memory.rs:200-400`)
```rust
// Each title page can have up to COPIES=4095 shadow copies
// Created when GPU needs a version different from CPU's
// Reference counted, evicted on pressure
struct ShadowPage {
    resource: Resource,      // D3D12 resource (VRAM)
    generation: u32,         // Version for coherency
    ref_count: AtomicU32,    // Concurrent access tracking
}
```
**Problem**: The console has **one physical page**; PC maintains **N shadow copies** for CPU/GPU concurrency. This is pure translation overhead.

### 3.2 Page Directory (`victory-gpu/src/memory.rs:400-600`)
```rust
// Directory: Vec<PageEntry> of length MEMORY_PAGES (67M)
struct PageEntry {
    allocation: Option<Allocation>, // Host allocation backing this page
    shadow: Option<ShadowPage>,     // Current GPU-visible copy
    state: PageState,               // CPU/GPU access state machine
    lock: Mutex<()>,                // Per-page guard lock
}
```
**Problem**: 67M entries × ~32 bytes = **~2 GiB metadata** just for page table. Most entries empty (sparse title allocations).

### 3.3 Arena Allocator (`victory-gpu/src/memory.rs:600-800`)
```rust
// Arena: 64 MiB GPU heaps, sub-allocated for resources
// Separate from upload heaps, separate from readback heaps
// Title memory lives in arenas; upload/readback are extra copies
```
**Problem**: Three separate heap types (arena/upload/readback) where console has one.

### 3.4 Fault Handling (`victory-gpu/src/memory.rs:800-1000`)
```rust
// On GPU page fault: walk Directory, find/create shadow, submit copy, wait fence
// ASKING mutex serializes all faults globally
```
**Problem**: Page faults are **expected and frequent** in this model — not exceptional.

---

## 4. Unnecessary Synchronization

### 4.1 ASKING Mutex (`victory-gpu/src/watch.rs:50-80`)
```rust
// Global mutex acquired on EVERY page fault
// Serializes fault handling across all CPU threads
static ASKING: Mutex<()> = Mutex::new(());
```
**Impact**: Contention scales with CPU thread count × fault rate. Titles with heavy streaming (open world) hit this constantly.

### 4.2 Journal Lock (`victory-gpu/src/watch.rs:100-150`)
```rust
// Journal.pages: Mutex<Vec<DirtyPage>>
// Locked on every CPU write to tracked memory
// Flushed at command buffer boundaries
```
**Impact**: Every title memory write → mutex lock → push dirty page → unlock.

### 4.3 Per-Page Guard Locks (`victory-gpu/src/memory.rs:400-450`)
```rust
// PageEntry.lock: Mutex<()> per page
// Acquired for state transitions (CPU→GPU, GPU→CPU, shadow creation)
```
**Impact**: Fine-grained but high frequency; cache line bouncing on metadata.

### 4.4 Fence Waits (`victory-gpu/src/upload.rs`, `resource.rs`)
```rust
// After every upload/readback copy: fence wait for GPU completion
// Blocks CPU thread until GPU finishes copy
queue.signal(fence, value).unwrap();
fence.set_event_on_completion(value, event).unwrap();
WaitForSingleObject(event, INFINITE);
```
**Impact**: Synchronous round-trip CPU→GPU→CPU on every resource transition.

---

## 5. Runtime Shader Translation Overhead

### 5.1 Translation Pipeline (`crates/victory-shader/`)

| Stage | Source | Operation | Cache |
|-------|--------|-----------|-------|
| 1. Parse | `native.rs` | XDAT/FSSM/FSPX → GFX10 ISA | Binary cache |
| 2. Build IR | `ir.rs` | GFX10 → SSA IR (per-lane) | — |
| 3. Structurise | `structure.rs` | CFG → structured control flow | — |
| 4. Trace Resources | `resource.rs` | Descriptor → root parameter mapping | — |
| 5. Lower | `emit/lower.rs` | IR → DXIL (via `dxil`) | **Shader cache** |
| 6. Create PSO | `victory-hle/d3d12x.rs` | DXIL + root signature → PSO | **PSO cache** |

### 5.2 Key Overhead Sources

#### `crates/victory-shader/src/ir.rs` — SSA IR Construction
```rust
// Builds per-instruction SSA from GFX10 bytecode
// Per-lane boolean tracking for divergence (wave64)
// ~500-2000 IR instructions per shader
```
**Cost**: CPU time at pipeline creation; not parallelizable per shader.

#### `crates/victory-shader/src/structure.rs` — Control Flow Structurisation
```rust
// Converts arbitrary CFG to structured (if/else/loop/continue/break)
// Required because DXIL mandates structured control flow
// Complex algorithms: loop detection, region formation, per-lane masking
```
**Cost**: Heaviest compilation step; exponential worst-case on irreducible CFGs.

#### `crates/victory-shader/src/emit/lower.rs` — IR to DXIL Lowering
```rust
// Lowers structured IR to DXIL via `dxil` crate
// Emits `dxil::Module`, `dxil::Function`, instructions
// Validates against DXIL rules
```
**Cost**: DXIL emission + validation; produces ~10-50 KiB DXIL per shader.

#### `crates/victory-hle/src/modules/d3d12x.rs` — Shader Cache & PSO Creation
```rust
// Lines 200-400: ShaderCache — content-addressed (BLAKE3 of DXIL + root sig)
// PSO creation: ID3D12Device::CreateGraphicsPipelineState — driver compiles DXIL→ISA
```
**Cost**: Driver compilation (DXIL→HW ISA) on first use; cached thereafter.

### 5.3 Quantified Shader Overhead
- **First-run stutter**: 10-500 ms per unique pipeline (driver compilation)
- **CPU compilation**: 1-10 ms per shader (IR→DXIL)
- **Cache size**: 100-500 MB for typical title (thousands of shaders)
- **Memory**: IR + DXIL + PSO blob retained in process

---

## 6. Proposed Native Windows Architecture

### 6.1 Core Principle: Replace Translation with Native Abstractions

```
┌─────────────────────────────────────────────────────────────────┐
│                    TITLE x86-64 CODE (UNCHANGED)                │
├─────────────────────────────────────────────────────────────────┤
│  Native Platform Layer (replaces victory-hle + victory-gpu)     │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐             │
│  │ Memory       │ │ GPU          │ │ Audio/Input  │  ...        │
│  │ Manager      │ │ Device       │ │ /FS/Network  │             │
│  └──────────────┘ └──────────────┘ └──────────────┘             │
│         │                │                │                      │
│         ▼                ▼                ▼                      │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │              HOST WINDOWS / D3D12 / XAUDIO2 / WIN32       │   │
│  └──────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

### 6.2 Memory Manager — Direct Host Allocations

**Replace**: `victory-hle/xmem.rs` + `victory-gpu/memory.rs` + `watch.rs` + `upload.rs`

**Native Implementation**:
```rust
// Title allocations → host VirtualAlloc/VirtualFree (MEM_RESERVE/COMMIT)
// GPU resources → ID3D12Device::CreateCommittedResource on DEFAULT/UPLOAD/READBACK heaps
// No page tracking, no shadow copies, no journal, no faults

struct NativeMemory {
    // Title VA → host allocation (sparse, hash map not 67M array)
    allocations: DashMap<u64, HostAllocation>,
    
    // GPU resource bindings: title VA → ID3D12Resource (direct, no shadow)
    gpu_resources: DashMap<u64, GpuResourceBinding>,
    
    // Upload/readback: explicit, title-controlled via HLE API
    upload_ring: UploadRingBuffer,  // Persistent, no per-copy fence wait
    readback_ring: ReadbackRingBuffer,
}
```

**Key Changes**:
- **Sparse hash map** (`DashMap`) instead of 67M-entry `Vec` → memory proportional to actual allocations
- **Direct GPU resource binding** — title VA maps to `ID3D12Resource*`, no shadow copy indirection
- **Persistent upload ring** — CPU writes → `Map` → GPU reads via `CopyBufferRegion` in batch, single fence per frame
- **No page faults** — resources created upfront or on explicit API call (like `CreateCommittedResource`)

### 6.3 GPU Device — Native D3D12 with Precompiled Shaders

**Replace**: `victory-hle/d3d12x.rs` + `victory-shader/` (runtime) + `victory-gpu/resource.rs`

**Native Implementation**:
```rust
// Offline: victory-pipeline converts ALL shaders at install/update time
// Runtime: Load precompiled DXIL blobs, create PSOs immediately

struct NativeGpuDevice {
    device: ID3D12Device,
    shader_library: ShaderLibrary,      // Precompiled DXIL, keyed by hash
    pso_cache: PsoCache,                // Warm at startup, no compilation stutter
    resource_heaps: HeapManager,        // DEFAULT/UPLOAD/READBACK heaps
    descriptor_heaps: DescriptorHeapManager, // CBV/SRV/UAV, sampler
}

struct ShaderLibrary {
    // Key: (shader_hash, permutation_key) → DXIL blob + reflection
    shaders: DashMap<ShaderKey, CompiledShader>,
}

struct CompiledShader {
    dxil: Vec<u8>,
    reflection: ShaderReflection,       // Root signature, resource binding
    permutations: Vec<Permutation>,     // Precompiled variants
}
```

**Offline Pipeline** (`victory-pipeline` extended):
```rust
// Converter: "shader.gfx10_to_dxil" — runs at game install/update
// Input: XDAT/FSSM/FSPX from title
// Output: DXIL blob + reflection JSON
// Cached in user's cache directory (content-addressed)
```

**Key Changes**:
- **Zero runtime translation** — all GFX10→DXIL done offline
- **PSO warmup** — background thread creates PSOs at launch from known permutations
- **Direct resource binding** — root signature from reflection, no descriptor tracing at runtime
- **No `victory-shader` crate at runtime** — only `dxil` for loading precompiled blobs

### 6.4 HLE Modules — Native Implementations

| Module | Current (Translation) | Native (Direct) |
|--------|----------------------|-----------------|
| `xmem` | Page-tracked allocator | `VirtualAlloc` + `HeapAlloc` |
| `d3d12x` | D3D12.X emulation layer | Native D3D12 + precompiled shaders |
| `kernel` | Thread/sync/event emulation | Native Win32 primitives |
| `files` | Packaged file system | Native `CreateFile`/`ReadFile` + loose file overlay |
| `audio` | XAudio2 emulation | Native XAudio2 (same API!) |
| `input` | GameInput emulation | Native GameInput / XInput |
| `network` | XNetworking emulation | Native Winsock / WinHTTP |

**Critical Insight**: Xbox Series APIs **are** Windows APIs (XAudio2, GameInput, DirectStorage, WinRT). HLE can become **thin wrappers** or **direct re-exports**.

---

## 7. Reusable Techniques from Other Recomp Projects

### 7.1 N64Recomp / Zelda64Recomp — Static Recompilation + Native Backends
- **Technique**: MIPS→LLVM IR→native code; native backends for GPU (Vulkan/D3D12), audio, input
- **Applicable**: Offline shader compilation pipeline (victory-pipeline) mirrors their ahead-of-time approach
- **Reference**: `recompiler/` + `backend/` split; runtime only loads precompiled artifacts

### 7.2 UnleashedRecomp — Ubershaders + Specialization
- **Technique**: Precompile "ubershaders" covering all feature combinations; specialize at runtime via push constants
- **Applicable**: Instead of per-shader DXIL compilation, compile **parameterized ubershaders** offline
- **Benefit**: Fewer PSOs, faster switching, driver-friendly

### 7.3 XenonRecomp / XenosRecomp — PPC→x86-64 + Native GPU
- **Technique**: Static recompilation of PPC code; GPU commands translated to native API once at load
- **Applicable**: Command buffer recording — record native D3D12 commands directly, no intermediate representation

### 7.4 Common Pattern: **Offline Processing → Runtime Loading**
All successful recomps push heavy work (translation, compilation, optimization) to **offline phase**. Dark Victory's runtime shader translation is the outlier.

---

## 8. Implementation Priorities

### Phase 1: Memory Manager Replacement (Highest Impact)
**Target**: Eliminate `victory-gpu/memory.rs`, `watch.rs`, `upload.rs`, `xmem.rs` (~15k LOC)
**Approach**:
1. Implement `NativeMemory` with sparse allocation map + direct GPU resource binding
2. Replace `XMEM` HLE module with `VirtualAlloc`-based allocator
3. Replace upload/readback with persistent ring buffers + batch fence
4. Remove `Directory`, `shadow`, `Journal`, `ASKING`, guard locks entirely

**Estimated Effort**: 3-4 weeks  
**Performance Gain**: 2-5× frame time (eliminates copy/fault overhead)

### Phase 2: Offline Shader Pipeline (High Impact)
**Target**: Eliminate `victory-shader/` at runtime; extend `victory-pipeline`
**Approach**:
1. Add `shader.gfx10_to_dxil` converter to `victory-pipeline`
2. Build shader enumeration tool (scan title for XDAT/FSSM/FSPX)
3. Precompile all permutations at install/update
4. Runtime: load DXIL blobs, create PSOs from cache

**Estimated Effort**: 4-6 weeks  
**Performance Gain**: Eliminates first-run stutter; reduces CPU usage

### Phase 3: Native HLE Modules (Medium Impact)
**Target**: Replace `victory-hle` modules with thin native wrappers
**Approach**:
1. `d3d12x` → direct D3D12 + root signature from reflection
2. `kernel` → Win32 threads/events/slim reader-writer locks
3. `audio` → native XAudio2 (same COM interface)
4. `files` → Win32 I/O + loose file overlay for patches

**Estimated Effort**: 2-3 weeks per module  
**Performance Gain**: Reduces HLE call overhead; improves compatibility

### Phase 4: Command Buffer Recording (Optimization)
**Target**: Record native D3D12 commands directly, no intermediate representation
**Approach**:
1. Replace `victory-gpu/resource.rs` command recording with direct `ID3D12GraphicsCommandList` calls
2. Eliminate resource state tracking duplication (D3D12 does it)
3. Use `ExecuteCommandLists` batches, not per-draw submits

**Estimated Effort**: 2-3 weeks  
**Performance Gain**: Lower driver overhead; better parallelism

---

## 9. Technical Limitations & Risks

### 9.1 Title Memory Address Space Assumptions
**Risk**: Titles may assume unified memory semantics (e.g., CPU writes, GPU reads same address immediately).
**Mitigation**: 
- Native implementation must guarantee coherency via `Map`/`Unmap` + fence ordering
- `UPLOAD` heap for CPU→GPU, `DEFAULT` for GPU-only, `READBACK` for GPU→CPU
- HLE API must enforce correct synchronization (title already calls `Flush`/`Wait`)

### 9.2 40-bit GPU Address Space
**Risk**: Xbox Series GPU uses 40-bit VA; some PC GPUs only support 32-39 bits for certain heap types.
**Mitigation**: 
- `ID3D12Device::CheckFeatureSupport(D3D12_FEATURE_GPU_VIRTUAL_ADDRESS_SUPPORT)`
- Allocate `DEFAULT` heaps with `D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS` / textures as needed
- Fallback: use `UPLOAD` heap for resources needing low addresses (rare)

### 9.3 Shader Permutation Explosion
**Risk**: Titles may have thousands of shader permutations; precompiling all may be slow/storage-heavy.
**Mitigation**:
- **Ubershader approach** (UnleashedRecomp): compile parameterized ubershaders, specialize via push constants
- **On-demand compilation** as fallback: if permutation missing, compile in background (not on render thread)
- **Permutation analysis**: scan title's pipeline cache (XDAT) to find actually-used permutations

### 9.4 XDAT/FSSM/FSPX Format Coverage
**Risk**: Shader container formats may have undocumented features.
**Mitigation**: 
- `victory-shader/native.rs` already parses these; reuse parser in offline tool
- Test against title corpus; add format support incrementally

### 9.5 GPL-3.0-or-later License Compliance
**Constraint**: All modifications must remain GPL-3.0-or-later.
**Implication**: 
- Native port code must be GPL-3.0-or-later
- Cannot link proprietary libraries (e.g., NVIDIA NSight, proprietary codecs)
- Offline pipeline tools also GPL-3.0-or-later (derivative work)

### 9.6 Anti-Cheat / Title Integrity
**Risk**: Some titles check memory layout, timing, or syscall patterns.
**Mitigation**: 
- Native memory layout (sparse VA) differs from console (contiguous 4 GiB)
- May need VA space reservation to mimic console layout (`VirtualAlloc` 4 GiB range, commit on demand)
- Timing: native is faster; may need artificial delays for timing-sensitive titles

### 9.7 D3D12.X Feature Parity
**Risk**: D3D12.X (Xbox) has features not in standard D3D12 (e.g., `SetPipelineState` with `ID3D12PipelineLibrary`).
**Mitigation**: 
- Map D3D12.X calls to closest D3D12 equivalent
- Pipeline libraries → manual PSO caching
- Shader Model 6.6+ features available on Windows with Agility SDK

---

## 10. Source Reference Index

### Memory System
| File | Lines | Component |
|------|-------|-----------|
| `crates/victory-hle/src/modules/xmem.rs` | 1-500 | Console memory allocator, page tracking |
| `crates/victory-gpu/src/memory.rs` | 1-1200 | TitleMemory, Directory, shadow, Arena, faults |
| `crates/victory-gpu/src/watch.rs` | 1-300 | Journal, ASKING mutex, dirty page tracking |
| `crates/victory-gpu/src/upload.rs` | 1-400 | UploadMemory, arena allocators, ring buffers |
| `crates/victory-gpu/src/resource.rs` | 1-800 | Heaps, Resources, Textures, depth twins |

### Shader Translation
| File | Lines | Component |
|------|-------|-----------|
| `crates/victory-shader/src/ir.rs` | 1-600 | SSA IR build from GFX10 |
| `crates/victory-shader/src/structure.rs` | 1-500 | Control-flow structurisation |
| `crates/victory-shader/src/resource.rs` | 1-400 | Descriptor tracing → root parameters |
| `crates/victory-shader/src/native.rs` | 1-300 | XDAT/FSSM/FSPX parsing |
| `crates/victory-shader/src/emit/lower.rs` | 1-400 | IR → DXIL lowering |
| `crates/victory-hle/src/modules/d3d12x.rs` | 200-500 | ShaderCache, PSO creation |

### Loader & Pipeline
| File | Lines | Component |
|------|-------|-----------|
| `crates/victory-loader/src/prepare.rs` | 1-283 | PE patching for native load |
| `crates/victory-loader/src/windows.rs` | 1-427 | LoadLibraryExW, import binding, TLS |
| `crates/victory-loader/src/resolve.rs` | 1-33 | ImportResolver, Provenance (HLE/Host/Title) |
| `crates/victory-pipeline/src/lib.rs` | 1-23 | Converter trait, content-addressed cache |
| `crates/victory-pipeline/src/cache.rs` | 1-236 | Cache implementation |
| `crates/victory-formats/src/texture.rs` | 1-1296 | GFX10 texture layouts, detile/tile |

### Documentation
| File | Purpose |
|------|---------|
| `docs/architecture.md` | Crate roles, data flow diagram |
| `docs/findings.md` | Performance findings, unified memory analysis |
| `docs/asset-policy.md` | Derived data policy (cache, not repo) |

---

## 11. Conclusion

**Feasibility**: **High**. Dark Victory's architecture is uniquely suited for native port because:
1. **CPU is already native** (x86-64 Zen 2 → x86-64 host)
2. **Console APIs are Windows APIs** (XAudio2, GameInput, DirectStorage, WinRT)
3. **Shader translation is the only heavy runtime compilation** — movable offline
4. **Memory translation is the only fundamental mismatch** — solvable by direct host allocation

**Recommended Path**: 
1. **Phase 1** (Memory) delivers immediate 2-5× performance gain
2. **Phase 2** (Shaders) eliminates stutter and reduces CPU load
3. **Phases 3-4** complete the native stack

**License Compliance**: All work remains GPL-3.0-or-later; no proprietary dependencies introduced.

**Risk Mitigation**: Incremental replacement (one subsystem at a time) with fallback to current implementation ensures bisectability and rollback.

---

*This document is a feasibility study. No runtime code has been modified. Implementation should proceed in phases with profiling at each step.*