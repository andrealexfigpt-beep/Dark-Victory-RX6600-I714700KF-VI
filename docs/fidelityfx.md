# FidelityFX experiment

`scripts/play.ps1` defaults to FidelityFX upscaling and frame generation
in the borderless Full HD Series S profile when AMD-signed SDK 2.4 DX12
DLLs are available. Set `-FidelityFxDir` to the SDK's `signedbin` folder
or set `VICTORY_FFX_DIR` before running the launcher.
`-Upscaler fsr4` and `-Upscaler fsr3` select the respective providers.
The default `auto` lets the SDK select one for the GPU.
`-NoFrameGeneration` keeps upscaling active. `-Native` restores the
original path. `-Fullscreen` uses exclusive presentation with
interpolation disabled. Shader warm-up is enabled by default.

The local `_play-fidelityfx.ps1` profile can also run the isolated
`target/fidelityfx/diag/victory.exe` build. Its
`-FrameGeneration:$false` switch disables interpolation.

The upscaler bridge and RGB9E5 conversion come from
[PastiiJay/klbf-lab at 0a8c5777](https://github.com/PastiiJay/klbf-lab/tree/0a8c5777fbbcab6409c142b0bc1ad7081212fac4).
The frame generation proxy and submission-ordered input handling are
adapted from [AwkGrep256's VIctory PR #1917](https://github.com/Marvisak/VIctory/pull/1917).
Separate UI registration follows [PastiiJay's klbf-amd experiment](https://github.com/PastiiJay/klbf-lab-beta/tree/klbf-amd).
The port retains the upstream GPL-3.0-or-later terms. AMD SDK DLLs are
external dependencies distributed under AMD's SDK terms.

For comparisons, keep the save, route, output resolution and game
settings constant. The host frame meter counts application frames.
The FidelityFX log distinguishes application frames from successful
interpolation callback output. CPU time in Present measures API
blocking. It does not measure input-to-display latency. Use an external
display or latency capture for that metric.
