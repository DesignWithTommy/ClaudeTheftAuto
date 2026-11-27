# run-m1lk.ps1 - one complete M1LK playtest: keep-awake, event-driven screenshots (watch.ps1),
# memory/paging/GPU sampling (probe.ps1), scripted Enter presses through menus.
#   run-m1lk.ps1 -Name ram3 -Env @{ VICTORY_TEXTURE_MIP_DROP='2' } [-Seconds 1200] [-Console series-x] [-Mode --no-rt]
param([Parameter(Mandatory)] [string] $Name, [hashtable] $Env = @{}, [int] $Seconds = 1200,
      [string] $Console = 'series-x', [string] $Mode = '--no-rt',
      [string] $Install = (Join-Path $env:LOCALAPPDATA 'M1LK Test'),
      [int] $PlayAfter = 620, [switch] $NoPlay, [string] $FromSave = '',
      [ValidateSet('','short','quick','full','soak')] [string] $Kind = '')
$tools = $PSScriptRoot
# Test kinds: short = 8 min, load/menus/crash-at-load checks (no play; e.g. Series S fails at 2.5-4 min), quick = one lever A/B (~2-4 min of play after load), full = new build (20 min),
# soak = stability and memory growth (40 min). -Seconds still overrides when given.
if ($Kind -and -not $PSBoundParameters.ContainsKey('Seconds')) { $Seconds = @{ short = 480; quick = 900; full = 1200; soak = 2400 }[$Kind] }
# Wait (max 10 min) for builds to finish so they don't skew memory numbers.
$t0 = Get-Date
while ((Get-Process cargo, rustc -ErrorAction SilentlyContinue) -and ((Get-Date) - $t0).TotalMinutes -lt 10) { Start-Sleep 10 }
$game = (Get-Content (Join-Path $Install 'game-path.txt') -Raw).Trim()
foreach ($k in $Env.Keys) { Set-Item "env:$k" $Env[$k] }
$env:VICTORY_DEBUG_HUD = '1'
$env:VICTORY_MEMORY_CATEGORIES = '1'
if (-not $env:VICTORY_KEYS) { $env:VICTORY_KEYS = ((30..600 | Where-Object { $_ % 20 -eq 0 } | ForEach-Object { "${_}:Enter" }) -join ',') }
# Save library: -FromSave <name> starts from a stored checkpoint (another location, skips the intro).
# After every run, a new autosave is stored in ..\saves-library\<run>-<time>.
$library = Join-Path $tools '..\saves-library'
$saves = Get-ChildItem (Join-Path $Install 'state') -Directory -ErrorAction SilentlyContinue | ForEach-Object { Join-Path $_.FullName 'saves' } | Where-Object { Test-Path $_ } | Select-Object -First 1
if ($FromSave -and $saves) {
    $src = Join-Path $library $FromSave
    if (-not (Test-Path $src)) { throw "no save '$FromSave' in $library" }
    $bak = Join-Path $library ("_before-restore-{0:yyyyMMdd-HHmmss}" -f (Get-Date))
    Copy-Item $saves $bak -Recurse
    Remove-Item "$saves\*" -Recurse -Force; Copy-Item "$src\*" $saves -Recurse
}
$saveStamp = if ($saves) { (Get-ChildItem $saves -Recurse -File | Measure-Object LastWriteTime -Maximum).Maximum } else { $null }
$out = Join-Path $tools "..\probe-results\$Name-watch"
New-Item -ItemType Directory -Force $out | Out-Null
($Env.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) + "mode=$Mode console=$Console" +
    "power=$((Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue).BatteryStatus) (2=AC)" +
    "freeRAM_GB=$([math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1MB, 1))" |
    Set-Content (Join-Path $out 'settings.txt')
$ka = Start-Process powershell -WindowStyle Hidden -PassThru -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$tools\keepawake.ps1`" -Minutes $([int]($Seconds / 60) + 10)"
# per-second GPU clocks/temp/power/VRAM (laptop thermal throttling)
$gpu = Start-Process "$env:WINDIR\System32\nvidia-smi.exe" -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $out 'gpu.csv') -ArgumentList '--query-gpu=timestamp,clocks.gr,clocks.mem,temperature.gpu,power.draw,utilization.gpu,memory.used,pstate,clocks_throttle_reasons.active --format=csv -l 1'
$watch = Start-Process powershell -WindowStyle Hidden -PassThru -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$tools\watch.ps1`" -Out `"$out`" -LogDir `"$Install\logs`""
# real keyboard/mouse play: movement, mouse look, pause menu (tools\play.ps1)
if ($Kind -eq 'short') { $NoPlay = [switch]::Present }
$play = if (-not $NoPlay) { Start-Process powershell -WindowStyle Hidden -PassThru -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$tools\play.ps1`" -Out `"$out`" -StartAfter $PlayAfter" }
$launch = [scriptblock]::Create("Start-Process '$Install\victory-play.exe' -ArgumentList '$Mode','--console','$Console','`"$game`"' -WorkingDirectory '$Install'")
& "$tools\probe.ps1" -Name $Name -Launch $launch -Process 'victory' -Seconds $Seconds
Get-Process victory, victory-play -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 8
$watch, $play | Where-Object { $_ } | Stop-Process -ErrorAction SilentlyContinue; $gpu | Stop-Process -ErrorAction SilentlyContinue; $ka | Stop-Process -ErrorAction SilentlyContinue
# the log can still be flushing after the kill: retry until the copy has content
$srcLog = Get-ChildItem "$Install\logs" -Filter 'run-*.log' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
for ($i = 0; $srcLog -and $i -lt 6; $i++) {
    Copy-Item $srcLog.FullName -Destination $out -Force
    if ((Get-Item (Join-Path $out $srcLog.Name)).Length -gt 0) { break }
    Start-Sleep 5
}
Get-ChildItem "$Install\state" -Recurse -Filter '*.crash.log' -ErrorAction SilentlyContinue | Where-Object LastWriteTime -gt $t0 | Copy-Item -Destination $out
$runLog = Get-ChildItem $out -Filter 'run-*.log' | Select-Object -First 1
$samples = Get-ChildItem (Join-Path $tools '..\probe-results') -Directory -Filter "$Name-2*" | Sort-Object Name -Descending | Select-Object -First 1
if ($runLog) {
    $svg = Join-Path $out 'chart.svg'
    python -I (Join-Path $tools 'perf-chart.py') --log $runLog.FullName --samples (Join-Path $samples.FullName 'samples.csv') --title "M1LK $Name" --out $svg
    & "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe" --headless=new --disable-gpu --window-size=900,660 "--screenshot=$out\chart.png" "file:///$($svg -replace '\\','/')" 2>$null
}
if ($saves) {
    $now = (Get-ChildItem $saves -Recurse -File | Measure-Object LastWriteTime -Maximum).Maximum
    if ($now -gt $saveStamp) {
        $dest = Join-Path $library ("{0}-{1:MMdd-HHmm}" -f $Name, $now)
        Copy-Item $saves $dest -Recurse
        Get-ChildItem "$out\shots" -Filter '*.png' | Sort-Object Name | Select-Object -Last 1 | Copy-Item -Destination (Join-Path $dest 'where.png')
        "new checkpoint saved -> $dest"
    }
}"done $Name -> $out"





