param(
    [Parameter(Mandatory)]
    [string]$Run,
    [DateTimeOffset]$FromUtc,
    [ValidateRange(0, 3600)]
    [int]$OffsetSeconds = 5,
    [ValidateRange(1, 3600)]
    [int]$DurationSeconds = 45
)

$ErrorActionPreference = 'Stop'
$culture = [Globalization.CultureInfo]::InvariantCulture
$log = Join-Path $Run 'console.log'
$ansi = [regex]::new('\x1b\[[0-?]*[ -/]*[@-~]')
$pattern = '^(?<utc>\d{4}-\d\d-\d\dT\S+Z)\s.+?native present samples \(us\).*?samples=\['
$first = Select-String -LiteralPath $log -Pattern 'ffx: first upscale recorded on the host GPU' -Context 0,3 | Select-Object -First 1
if (-not $first) { throw 'The first real FidelityFX dispatch has not been recorded.' }
if (-not $PSBoundParameters.ContainsKey('FromUtc')) {
    $anchor = [DateTimeOffset]::Parse(($ansi.Replace($first.Line, '') -split '\s+')[0], $culture)
    $FromUtc = $anchor.AddSeconds($OffsetSeconds)
}
$until = $FromUtc.AddSeconds($DurationSeconds)
$intervals = [Collections.Generic.List[long]]::new()
$firstSample = $null
$lastSample = $null
$latestBatch = $null
$reader = [IO.StreamReader]::new([IO.File]::Open($log, 'Open', 'Read', 'ReadWrite'))
try {
    while ($null -ne ($line = $reader.ReadLine())) {
        if (!$line.Contains('native present samples (us)')) { continue }
        $line = $ansi.Replace($line, '')
        if ($line -notmatch $pattern) { continue }
        $end = [DateTimeOffset]::Parse($Matches['utc'], $culture)
        $latestBatch = $end
        if ($end -lt $FromUtc) { continue }
        $values = ($line -split 'samples=\[', 2)[1]
        while (!$values.Contains(']') -and $null -ne ($next = $reader.ReadLine())) {
            if ($next -notmatch '^\s*[\d, ]+\]?\s*$') { break }
            $values += $next
        }
        if (!$values.Contains(']')) { continue } # the live log may end here
        $batch = @($values.TrimEnd(']', ' ').Split(',') | ForEach-Object { [long]$_ })
        for ($i = $batch.Count - 1; $i -ge 0; $i--) {
            $begin = $end.AddTicks(-10 * $batch[$i])
            if ($begin -ge $FromUtc -and $end -le $until) {
                $intervals.Add($batch[$i])
                if ($null -eq $firstSample -or $begin -lt $firstSample) { $firstSample = $begin }
                if ($null -eq $lastSample -or $end -gt $lastSample) { $lastSample = $end }
            }
            $end = $begin
        }
        if ($end -gt $until) { break }
    }
} finally {
    $reader.Dispose()
}
if ($intervals.Count -eq 0) { throw 'No native present intervals in the requested window.' }
$intervals.Sort()
$sum = ($intervals | Measure-Object -Sum).Sum
$p95 = $intervals[[int][math]::Ceiling($intervals.Count * 0.95) - 1]
function Get-CpuCallUs($entry) {
    if (!$entry) { return $null }
    $text = $ansi.Replace((@($entry.Line) + $entry.Context.PostContext -join ' '), '')
    if ($text -match 'cpu_us=(\d+)') { return [long]$Matches[1] }
}
$firstCall = Get-CpuCallUs $first
$warm = Select-String -LiteralPath $log -Pattern 'ffx: startup warm-up' -Context 0,2 | Select-Object -First 1
$warmCall = Get-CpuCallUs $warm
$memory = @(Import-Csv -LiteralPath (Join-Path $Run 'memory.csv'))
[pscustomobject]@{
    Run = $Run
    RequestedFromUtc = $FromUtc
    RequestedUntilUtc = $until
    Complete = $latestBatch -ge $until
    SampleFromUtc = $firstSample
    SampleUntilUtc = $lastSample
    NativeFrames = $intervals.Count
    NativeFps = [math]::Round($intervals.Count * 1e6 / $sum, 2)
    NativeP95Ms = [math]::Round($p95 / 1000.0, 2)
    NativeWorstMs = [math]::Round($intervals[$intervals.Count - 1] / 1000.0, 2)
    FirstUpscaleCpuMs = $(if ($null -ne $firstCall) { [math]::Round($firstCall / 1000.0, 2) })
    WarmupCpuMs = $(if ($null -ne $warmCall) { [math]::Round($warmCall / 1000.0, 2) })
    SessionMinCommitHeadroomMiB = ($memory | ForEach-Object { [int]$_.commit_available_mib } | Measure-Object -Minimum).Minimum
    SessionPeakPrivateMiB = ($memory | ForEach-Object { [int]$_.private_mib } | Measure-Object -Maximum).Maximum
}
