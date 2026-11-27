# Starts the title with the settings this fork is played with (README.md, "How to run it").
#   scripts\play.ps1 -Exe <path of your own copy's executable>
# -Safe         without the newer, faster ways (the first thing to try when something is wrong)
# -RayTracing   the title's own ray-traced indirect light: lit interiors, about a third slower
# -Build        target\diag (the default, the build all measurements are of) or target\release
# -Fullscreen   request DXGI exclusive fullscreen
# -Windowed     use a decorated window up to 1600x900
# -Native       use the original presentation and upscaling path
# -NoFrameGeneration   keep FidelityFX upscaling and disable interpolation
# -FidelityFxDir       AMD SDK 2.4 signedbin folder if installed elsewhere
# -WarmShaders  warm up to 128 previously used pipelines before running
# -More         other settings: -More @{VICTORY_PANEL='1'}
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$Safe,
    [switch]$RayTracing,
    [ValidateSet('series-s', 'series-x')][string]$ConsoleProfile = 'series-s',
    [switch]$Fullscreen,
    [switch]$Windowed,
    [switch]$WarmShaders = $true,
    [switch]$Native,
    [switch]$NoFrameGeneration,
    [switch]$PrepareOnly,
    [ValidateSet('auto', 'fsr3', 'fsr4')][string]$Upscaler = 'fsr4',
    [string]$FidelityFxDir,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [hashtable]$More = @{}
)
$settings = @{
    VICTORY_DISPLAY_MODE = '1920x1080'
    VICTORY_BORDERLESS = if ($Fullscreen -or $Windowed -or $ConsoleProfile -ne 'series-s') { '0' } else { '1' }
    VICTORY_AMBIENT_HISTORY_LIMIT = '0'
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = '1'
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0.8'; VICTORY_D3D12_DRED = 'light'; VICTORY_SKIP_PIPELINES = 'none'
}
if ($Windowed -or ($ConsoleProfile -ne 'series-s' -and !$Fullscreen)) {
    $settings.VICTORY_WINDOW_MAX = '1600x900'
}
if ($Safe) {
    $settings.VICTORY_UNDER_WIDE = '0'
    $settings.VICTORY_LAND_ECHO = '0'
} else {
    $settings.VICTORY_JOIN_PARTS = '4'
    $settings.VICTORY_TEXTURE_STAGE_MS = '16'
    if ($ConsoleProfile -eq 'series-s') {
        $settings.VICTORY_PORT_DESCRIPTORS = '1'
        $settings.VICTORY_PORT_GUARD = '1'
        $settings.VICTORY_PORT_JOURNAL = '1'
        $settings.VICTORY_PORT_CONSTANTS = '1'
        $settings.VICTORY_WINDOW_KB = '1024'
        $settings.VICTORY_WINDOW_TAILS = '1'
    }
}
if ($Fullscreen) { $settings.VICTORY_FULLSCREEN = '1' }
if ($RayTracing) { $settings.VICTORY_BVH_BUILDER = '1' }
if (-not ($Native -or $Safe) -and $ConsoleProfile -eq 'series-s') {
    if (-not $FidelityFxDir) { $FidelityFxDir = $env:VICTORY_FFX_DIR }
    if (-not $FidelityFxDir -and $env:LOCALAPPDATA) {
        $FidelityFxDir = Join-Path $env:LOCALAPPDATA 'victory\fidelityfx-sdk-2.4.0\FidelityFX-SDK-2.4.0\Kits\FidelityFX\signedbin'
    }
    $sdk = if ($FidelityFxDir) { Resolve-Path -LiteralPath $FidelityFxDir -ErrorAction SilentlyContinue }
    $frameGeneration = -not ($NoFrameGeneration -or $Fullscreen)
    $names = @('amd_fidelityfx_loader_dx12.dll', 'amd_fidelityfx_upscaler_dx12.dll')
    if ($frameGeneration) { $names += 'amd_fidelityfx_framegeneration_dx12.dll' }
    if ($sdk) {
        foreach ($name in $names) {
            $file = Join-Path $sdk.Path $name
            $signature = if (Test-Path -LiteralPath $file -PathType Leaf) {
                Get-AuthenticodeSignature -LiteralPath $file
            }
            if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'CN=Advanced Micro Devices') {
                $sdk = $null
                break
            }
        }
    }
    if ($sdk) {
        $settings.VICTORY_FFX_DIR = $sdk.Path
        $settings.VICTORY_FFX_UPSCALER = $Upscaler
        $settings.VICTORY_FRAMEGEN = if ($frameGeneration) { '1' } else { '0' }
        if ($WarmShaders -and $Upscaler -eq 'fsr4') { $settings.VICTORY_FFX_WARM = '1' }
        $settings.VICTORY_CARD_BUFFERS_MB = '768'
        $settings.VICTORY_CARD_BUFFERS_MIN_KB = '4096'
        $settings.VICTORY_CARD_SMALL_MB = '256'
        $settings.VICTORY_UNDER_WIDE = '0'
        $settings.VICTORY_LAND_ECHO = '0'
        $settings.VICTORY_LAND_THREADS = '8'
    } else {
        Write-Warning 'AMD-signed FidelityFX SDK 2.4 DLLs were not found. Starting with the original presentation path.'
    }
}
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }

$victory = Join-Path $PSScriptRoot "..\target\$Build\victory.exe"
if ($settings.VICTORY_FFX_DIR) {
    $validatedBuild = Join-Path $PSScriptRoot "..\target\ffx-perf-validated\$Build\victory.exe"
    $sdkBuild = Join-Path $PSScriptRoot "..\target\fidelityfx\$Build\victory.exe"
    if (Test-Path -LiteralPath $validatedBuild -PathType Leaf) {
        $victory = $validatedBuild
    } elseif (!(Test-Path -LiteralPath $victory -PathType Leaf) -and (Test-Path -LiteralPath $sdkBuild -PathType Leaf)) {
        $victory = $sdkBuild
    }
}
if (-not (Test-Path $victory)) { Write-Host "No $victory. Build it first (README.md)."; return }
if (-not (Test-Path $Exe)) { Write-Host "No $Exe."; return }
if ($PrepareOnly) {
    [pscustomobject]@{
        executable = (Resolve-Path -LiteralPath $victory).Path
        upscaler = $settings.VICTORY_FFX_UPSCALER
        sdk = $settings.VICTORY_FFX_DIR
        frame_generation = $settings.VICTORY_FRAMEGEN -eq '1'
        warm_shaders = [bool]$WarmShaders
        display_mode = $settings.VICTORY_DISPLAY_MODE
        window_max = $settings.VICTORY_WINDOW_MAX
        memory_window_kib = $settings.VICTORY_WINDOW_KB
        compact_tails = $settings.VICTORY_WINDOW_TAILS -eq '1'
        borderless = $settings.VICTORY_BORDERLESS -eq '1' -and $settings.VICTORY_FULLSCREEN -ne '1'
    } | ConvertTo-Json
    return
}
# (Other settings left over in this window go; where your files are stays.)
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $paths } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }
$Exe = (Resolve-Path $Exe).Path
Push-Location (Join-Path $PSScriptRoot '..')   # victory.toml is looked for here
try {
    if ($WarmShaders) {
        & $victory warm --used --jobs 2 --limit 128
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    & $victory run $Exe --execute --console $ConsoleProfile
} finally {
    Pop-Location
}
