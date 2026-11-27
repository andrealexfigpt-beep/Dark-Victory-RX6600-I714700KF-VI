# Dark VIctory

**A Dark fork of VIctory for Radeon RX 6600 and Intel Core i7-14700KF.**

Dark VIctory is a compatibility layer that runs an Xbox Series title on Windows
from files the user supplies. This fork focuses on making the translation layer
faster and more memory-efficient on mid-range AMD hardware, while keeping the
upstream architecture and credits intact.

**This repository contains no game files, code, or assets.** You need your own
copy of the game. This project is not affiliated with or endorsed by the game's
developer or publisher.

## Does it run?

Yes, through the prologue and into the playable map. The verified Dark milestone
is **stable around 10 FPS in the final map segment** on the reference machine
below. The work is not finished: the current target is **20 stable FPS**, with
better memory headroom and no crashes during a full validation run.

All performance numbers here are self-reported measurements from one machine.
They are not guarantees for other scenes, drivers, or systems.

## Verified reference machine

| Component | Tested configuration |
|---|---|
| OS | Windows 11 x64 |
| CPU | Intel Core i7-14700KF, 28 logical processors |
| GPU | AMD Radeon RX 6600, 8 GB, RDNA2 |
| RAM | 23.76 GB available to Windows |
| Output | 1920×1080, Full HD |
| Console profile | Xbox Series S |
| Ray tracing | Off by default |

## Current milestone

Verified milestone-15 result, measured in the final map segment:

| Metric | Result |
|---|---:|
| Frame rate | **9.5–10.7 FPS**, normally near 10 |
| CPU preparation | About 75 ms per frame |
| Buffer refresh checks | About 8,000–10,000 per frame |
| Worst descriptor synchronization stall | About 163 ms in one observed window |
| GPU-to-CPU readback/change traffic | About 50–53 MiB per frame |
| Private memory | About 19.5 GiB |
| Resident memory | About 15 GiB |
| Lowest Windows commit headroom during loading | About 224 MiB |
| VRAM headroom near the measured limit | About 200 MiB |

The next validation target is:

- Average frame rate of at least **20 FPS**.
- 95% of frames at or below **55 ms**.
- No crash during loading and at least 15 minutes in the map, including streaming.
- Better Windows commit headroom than milestone-15.
- Recorded RAM and VRAM peaks during loading.

**20+ FPS on high-end GPUs is the project target, not a verified result yet.**
The RX 6600 measurement is the current public baseline.

## What Dark VIctory changes

The Dark fork keeps the upstream VIctory foundation and adds focused work on:

- Bounded freshness proofs for repeated buffer-refresh checks.
- Tracked 4 KiB descriptor synchronization with a full-scan fallback.
- Async-compatible D3D12 pass timestamps for render, copy, compare, and readback.
- Live render-target/depth dimension logging, separate from swap-chain size.
- Smaller known-size host-copy windows for root constant-buffer views.
- Snapshot and host-window memory diagnostics.
- Experimental upload-pool growth controls adapted from the Cool Fox fork.
- Full HD output/fullscreen support and reproducible run-local save profiles.

See [docs/findings.md](docs/findings.md) and the source comments for the detailed
implementation and the meaning of each `VICTORY_...` switch.

## Requirements

- Windows 10/11 x64.
- CPU with AVX2.
- Direct3D 12 GPU with shader model 6.6 and mesh shaders.
- Rust toolchain pinned by `rust-toolchain.toml`.
- MSVC build tools and Windows SDK.
- Your own dump of the title and its archives.
- 24 GB RAM was used for the RX 6600 baseline; **32 GB is recommended** for
  additional headroom during streaming and loading.

## Build and run

```powershell
git clone https://github.com/darksidexc/Dark-Victory-RX6600-I714700KF-VI.git
cd Dark-Victory-RX6600-I714700KF-VI
copy victory.example.toml victory.toml
# Edit victory.toml and set game_dir, cache_dir, and data_dir for your machine.

cargo build --locked --profile diag -p victory --features diagnostics
scripts\play.ps1 -Exe <path-to-your-title-executable>
```

The Series S profile defaults to Full HD borderless, 1 MiB memory windows
for sized requests, compact tails and shader warm-up. It uses FSR 4 and
frame generation when the signed AMD SDK 2.4 is available. Pass
`-FidelityFxDir <path-to-signedbin>` to select your SDK installation.
`-Native` selects the original path, `-NoFrameGeneration` keeps
FidelityFX upscaling active, and `-Windowed` opens a window up to 1600×900.
See [the FidelityFX setup](docs/fidelityfx.md).

For the Dark Full HD/fullscreen profile:

```powershell
scripts\play.ps1 -Exe <path-to-your-title-executable> -Fullscreen
```

Use `scripts\play.ps1 -Safe` when diagnosing a new problem. Ray tracing is opt-in
with `-RayTracing`; the validated Dark baseline keeps it off.

Before the first run, shader and pipeline preparation can be done ahead of time:

```powershell
target\diag\victory.exe shaders <path-to-shader-archives>
target\diag\victory.exe warm --used
```

Upstream command reference:
[docs/README-fxceless.md](docs/README-fxceless.md).

## Known limitations

- The translation layer remains memory-heavy; 24 GB is enough for the measured
  baseline but leaves limited commit headroom during loading.
- Heavy scenes and streaming can still reduce frame rate well below the target.
- Some outdoor geometry and streamed textures can flicker or appear late.
- Indoor areas can be too dark without the optional ray-tracing path.
- A new driver or build can stutter while pipelines are created.
- Full HD scene-buffer dimensions are logged because output size alone does not
  prove every internal 3D pass is rendering at 1920×1080.

## Credits

Dark VIctory is a tree of other people's work with more on top. Upstream history
and authorship remain acknowledged.

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and contributors:
  loader, runtime services, Direct3D 12 back end, and shader recompiler.
- **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**:
  the high-performance AMD fork this tree builds on.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)** and its
  contributors.
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)**: ray-tracing fixes,
  cube gradients, and sparse textures.
- **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**:
  ambient light, pipeline workers, and work by romainhedouin.
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**.
- **[myokex/victory-amd](https://github.com/myokex/victory-amd)**: page-view guards
  and title-memory reads, with Spexivus, joroyt, tightfall, and Joshua Odom.
- **Geckomacabre** and **cxy0072019-sketch**: texture read-back.
- **[codec-d/cool_fox](https://github.com/codec-d/cool_fox)**: upload-pool growth
  and memory-admission work adapted for the Dark experiments.
- **[XCL](https://github.com/ivall/xcl)** was studied for platform behavior; no XCL
  code is included here.
- Dark fork maintenance and RX 6600/i7-14700KF tuning: **darksidexc**.

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE). The repository asset policy
is documented in [docs/asset-policy.md](docs/asset-policy.md).
