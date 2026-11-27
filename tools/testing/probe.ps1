# probe.ps1 — run one test of a runtime build and record what happened.
# Samples the runtime process every $Every seconds: private/working-set memory,
# hard page faults, GPU dedicated/shared memory, CPU; watches the log for growth;
# stops after $Seconds or when the process exits; writes samples.csv + summary.txt.
#
#   probe.ps1 -Name neptune -Launch { & run-test.ps1 ... } -Process victory -Log $env:TEMP\neptune-last.log.err
param(
    [Parameter(Mandatory)] [string] $Name,
    [Parameter(Mandatory)] [scriptblock] $Launch,
    [string] $Process = 'victory*',
    [string[]] $Log = @(),
    [int] $Seconds = 900,
    [int] $Every = 2,
    [int] $StallSeconds = 120,
    [string] $Out = (Join-Path $PSScriptRoot "..\probe-results")
)
$ErrorActionPreference = 'Continue'
$dir = Join-Path $Out ("{0}-{1:yyyyMMdd-HHmmss}" -f $Name, (Get-Date))
New-Item -ItemType Directory -Force $dir | Out-Null
$csv = Join-Path $dir 'samples.csv'
't,private_mb,ws_mb,hard_faults_s,gpu_ded_mb,gpu_shared_mb,cpu_pct,free_ram_mb,log_kb' | Set-Content $csv

$job = Start-Job -ScriptBlock $Launch
$t0 = Get-Date
$proc = $null
while (-not $proc -and ((Get-Date) - $t0).TotalSeconds -lt 600) {
    Start-Sleep -Milliseconds 500
    $proc = Get-Process -Name $Process -ErrorAction SilentlyContinue |
        Where-Object { $_.StartTime -ge $t0.AddSeconds(-5) -and $_.MainModule.FileName -notmatch 'warm' } |
        Sort-Object WorkingSet64 -Descending | Select-Object -First 1
}
if (-not $proc) { "no process matching $Process started" | Set-Content (Join-Path $dir 'summary.txt'); exit 1 }
$started = Get-Date
$peakPriv = 0; $peakWs = 0; $peakDed = 0; $peakShared = 0; $minFree = [int]::MaxValue
$lastLog = 0; $lastGrowth = Get-Date; $stalled = $false; $prevCpu = $proc.TotalProcessorTime
$cores = [Environment]::ProcessorCount
while (-not $proc.HasExited -and ((Get-Date) - $started).TotalSeconds -lt $Seconds) {
    Start-Sleep -Seconds $Every
    $proc.Refresh(); if ($proc.HasExited) { break }
    $t = [int]((Get-Date) - $started).TotalSeconds
    $priv = [int]($proc.PrivateMemorySize64 / 1MB); $ws = [int]($proc.WorkingSet64 / 1MB)
    $cpu = [int](100 * ($proc.TotalProcessorTime - $prevCpu).TotalSeconds / ($Every * $cores)); $prevCpu = $proc.TotalProcessorTime
    $hf = 0; $ded = 0; $sh = 0
    try {
        $c = Get-Counter -Counter @('\Memory\Pages Input/sec',
            "\GPU Process Memory(pid_$($proc.Id)*)\Dedicated Usage",
            "\GPU Process Memory(pid_$($proc.Id)*)\Shared Usage") -ErrorAction SilentlyContinue
        foreach ($s in $c.CounterSamples) {
            if ($s.Path -match 'pages input') { $hf = [int]$s.CookedValue }
            elseif ($s.Path -match 'dedicated') { $ded += [int]($s.CookedValue / 1MB) }
            elseif ($s.Path -match 'shared') { $sh += [int]($s.CookedValue / 1MB) }
        }
    } catch {}
    $free = [int]((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1024)
    $logKb = 0; foreach ($l in $Log) { if (Test-Path $l) { $logKb += [int]((Get-Item $l).Length / 1KB) } }
    if ($logKb -ne $lastLog) { $lastLog = $logKb; $lastGrowth = Get-Date }
    elseif (((Get-Date) - $lastGrowth).TotalSeconds -gt $StallSeconds) { $stalled = $true }
    "$t,$priv,$ws,$hf,$ded,$sh,$cpu,$free,$logKb" | Add-Content $csv
    $peakPriv = [Math]::Max($peakPriv, $priv); $peakWs = [Math]::Max($peakWs, $ws)
    $peakDed = [Math]::Max($peakDed, $ded); $peakShared = [Math]::Max($peakShared, $sh); $minFree = [Math]::Min($minFree, $free)
}
$exited = $proc.HasExited
$code = if ($exited) { try { $proc.ExitCode } catch { '?' } } else { 'running (stopped by probe)' }
if (-not $exited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
$dur = [int]((Get-Date) - $started).TotalSeconds

# copy logs and pull out the interesting lines
$hits = @()
foreach ($l in $Log) {
    if (Test-Path $l) {
        Copy-Item $l $dir
        $hits += Select-String -Path $l -Pattern 'panic|crash|exception|device removed|DEVICE_REMOVED|0x887a|out of memory|watchdog|fps|frame time|ms/frame|unsupported|failed' -CaseSensitive:$false |
            Select-Object -Last 60 | ForEach-Object { $_.Line.Trim() }
    }
}
$heavyPaging = (Import-Csv $csv | Where-Object { [int]$_.hard_faults_s -gt 2000 }).Count
@(
    "name:            $Name",
    "duration_s:      $dur",
    "exit:            $code",
    "log_stalled:     $stalled (no log growth for >$StallSeconds s)",
    "peak_private_mb: $peakPriv",
    "peak_ws_mb:      $peakWs",
    "peak_gpu_ded_mb: $peakDed",
    "peak_gpu_shr_mb: $peakShared",
    "min_free_ram_mb: $minFree",
    "heavy_paging_samples (>2000 pages/s): $heavyPaging",
    "",
    "--- notable log lines (last 60) ---"
) + $hits | Set-Content (Join-Path $dir 'summary.txt')
Remove-Job $job -Force -ErrorAction SilentlyContinue
Write-Output $dir
