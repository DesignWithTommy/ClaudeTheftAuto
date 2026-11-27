<#
.SYNOPSIS
Collects M1LK logs, crash logs and a PC specs summary into a zip for a bug report.

.DESCRIPTION
Gathers the latest run and shader-warm logs, every crashes.log, an optional build log and a
summary of the PC (GPU, VRAM, driver, CPU, RAM, Windows build, storage, page file). Before
anything is written to the zip, personal information is replaced: the user profile path and user
name become <user>, the computer name becomes <pc>, and IP addresses and e-mail addresses are
removed.

Only text logs are collected. Title files, captures, shader caches, saves and memory dumps are
never included. Read the files in the zip before you attach it.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File tools\collect-logs.ps1

.EXAMPLE
powershell -ExecutionPolicy Bypass -File tools\collect-logs.ps1 -BuildLog build.txt
#>
[CmdletBinding()]
param(
    # The folder that holds m1lk-play.exe (Setup.cmd installs to %LOCALAPPDATA%\M1LK).
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'M1LK'),
    # The data folder used by direct `m1lk.exe run` launches (VICTORY_DATA_DIR overrides it).
    [string] $DataDir = $(if ($env:VICTORY_DATA_DIR) { $env:VICTORY_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'M1LK\data' }),
    # A saved copy of the full cargo output, for build problems.
    [string] $BuildLog,
    # How many recent run and warm logs to include.
    [int] $RecentLogs = 3,
    # Where to write the zip. Defaults to the current folder.
    [string] $OutFile = (Join-Path (Get-Location) ("m1lk-report-{0:yyyyMMdd-HHmmss}.zip" -f (Get-Date)))
)

$ErrorActionPreference = 'Stop'
$maxBytes = 4MB
if (![IO.Path]::IsPathRooted($OutFile)) { $OutFile = Join-Path (Get-Location) $OutFile }

# --- Scrubbing -------------------------------------------------------------------------------

$replacements = New-Object System.Collections.Generic.List[object]
function Add-Replacement([string] $Find, [string] $With, [string] $Options) {
    if ($Find -and $Find.Length -ge 2) { $replacements.Add(@($Find, $With, $Options)) }
}
# Paths first, then whole-word names. The computer name is matched case-sensitively so that
# hardware names such as "Laptop GPU" survive on a computer named LAPTOP.
Add-Replacement $env:USERPROFILE '<user>' 'IgnoreCase'
Add-Replacement ($env:USERPROFILE -replace '\\', '/') '<user>' 'IgnoreCase'
Add-Replacement $env:USERNAME '<user>' 'IgnoreCase'
Add-Replacement $env:COMPUTERNAME '<pc>' 'None'
Add-Replacement $env:USERDOMAIN '<pc>' 'None'

function Protect-Text([string] $Text, [switch] $KeepNames) {
    foreach ($pair in $replacements) {
        $pattern = [regex]::Escape($pair[0])
        if ($pair[0] -match '^\w+$') {
            if ($KeepNames) { continue }
            $pattern = '\b' + $pattern + '\b'
        }
        $Text = [regex]::Replace($Text, $pattern, $pair[1], $pair[2])
    }
    # 8.3 short forms of the profile folder, e.g. C:\Users\ABCDEF~1
    $Text = [regex]::Replace($Text, '(?i)([A-Z]:[\\/]Users[\\/])[^\\/\s"'']+', '$1<user>')
    $Text = [regex]::Replace($Text, '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}', '<email>')
    $Text = [regex]::Replace($Text, '\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b', '<ip>')
    $Text = [regex]::Replace($Text, '(?i)\b(?:[0-9a-f]{1,4}:){4,7}[0-9a-f]{1,4}\b', '<ip>')
    return $Text
}

function Read-Tail([string] $Path) {
    $info = Get-Item -LiteralPath $Path
    $stream = [IO.File]::Open($info.FullName, 'Open', 'Read', 'ReadWrite')
    try {
        $count = [Math]::Min($stream.Length, $maxBytes)
        $null = $stream.Seek(-$count, 'End')
        $buffer = New-Object byte[] $count
        $read = $stream.Read($buffer, 0, $count)
        $text = [Text.Encoding]::UTF8.GetString($buffer, 0, $read)
    } finally { $stream.Dispose() }
    if ($info.Length -gt $maxBytes) { $text = "[only the last $($maxBytes / 1MB) MB of this file]`n" + $text }
    return $text
}

$staging = Join-Path ([IO.Path]::GetTempPath()) ('m1lk-report-' + [IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $staging | Out-Null
$collected = New-Object System.Collections.Generic.List[string]

function Add-Log([string] $Path, [string] $Name) {
    if (!$Path -or !(Test-Path -LiteralPath $Path -PathType Leaf)) { return }
    $extension = [IO.Path]::GetExtension($Path).ToLowerInvariant()
    if ($extension -notin '.log', '.txt') { return }
    $target = Join-Path $staging $Name
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
    [IO.File]::WriteAllText($target, (Protect-Text (Read-Tail $Path)), [Text.UTF8Encoding]::new($false))
    $collected.Add($Name)
}

# --- Logs --------------------------------------------------------------------------------------

$logs = Join-Path $InstallDir 'logs'
if (Test-Path -LiteralPath $logs) {
    foreach ($pointer in 'latest.txt', 'latest-warm.txt') {
        $file = Join-Path $logs $pointer
        if (Test-Path -LiteralPath $file) {
            $target = (Get-Content -LiteralPath $file -Raw).Trim()
            Add-Log $target ('logs/' + $pointer.Replace('.txt', '.log'))
        }
    }
    foreach ($prefix in 'run', 'warm') {
        Get-ChildItem -LiteralPath $logs -Filter "$prefix-*.log" -File |
            Sort-Object LastWriteTime -Descending | Select-Object -First $RecentLogs |
            ForEach-Object { Add-Log $_.FullName ('logs/' + $_.Name) }
    }
}

$i = 0
foreach ($root in @((Join-Path $InstallDir 'state'), $DataDir) | Select-Object -Unique) {
    if (!(Test-Path -LiteralPath $root)) { continue }
    Get-ChildItem -LiteralPath $root -Filter 'crashes.log' -File -Recurse -Depth 2 -ErrorAction SilentlyContinue |
        ForEach-Object { $i++; Add-Log $_.FullName ("crashes/$i-" + $_.Directory.Name + '-crashes.log') }
}

if ($BuildLog) { Add-Log (Resolve-Path -LiteralPath $BuildLog).Path 'build.log' }

# --- Specs -------------------------------------------------------------------------------------

function Get-Vram($Controller) {
    # AdapterRAM is a 32-bit field; the driver's registry value has the real size.
    $class = 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}'
    foreach ($key in Get-ChildItem $class -ErrorAction SilentlyContinue) {
        $props = Get-ItemProperty $key.PSPath -ErrorAction SilentlyContinue
        if ($props.DriverDesc -eq $Controller.Name -and $props.'HardwareInformation.qwMemorySize') {
            return '{0:N1} GB' -f ([double]$props.'HardwareInformation.qwMemorySize' / 1GB)
        }
    }
    if ($Controller.AdapterRAM) { return '{0:N1} GB (may be capped at 4 GB)' -f ($Controller.AdapterRAM / 1GB) }
    return 'unknown'
}

$specs = New-Object System.Collections.Generic.List[string]
$specs.Add('M1LK report  ' + (Get-Date -Format 'yyyy-MM-dd HH:mm'))
$commit = $null
if (Get-Command git -ErrorAction SilentlyContinue) {
    $commit = & git -C $PSScriptRoot rev-parse --short HEAD 2>$null
}
$specs.Add('M1LK commit: ' + $(if ($commit) { $commit } else { 'unknown (add it to the issue)' }))
try {
    $os = Get-CimInstance Win32_OperatingSystem
    $specs.Add("Windows: $($os.Caption) $($os.Version) (build $($os.BuildNumber))")
    $specs.Add(('RAM: {0:N1} GB total, {1:N1} GB free' -f ($os.TotalVisibleMemorySize / 1MB), ($os.FreePhysicalMemory / 1MB)))
    $specs.Add(('Commit limit: {0:N1} GB' -f ($os.TotalVirtualMemorySize / 1MB)))
} catch { $specs.Add('Windows: unavailable') }
try {
    foreach ($cpu in Get-CimInstance Win32_Processor) {
        $specs.Add("CPU: $($cpu.Name.Trim()) ($($cpu.NumberOfCores) cores, $($cpu.NumberOfLogicalProcessors) threads)")
    }
} catch { $specs.Add('CPU: unavailable') }
try {
    foreach ($gpu in Get-CimInstance Win32_VideoController) {
        $specs.Add("GPU: $($gpu.Name), VRAM $(Get-Vram $gpu), driver $($gpu.DriverVersion) ($($gpu.DriverDate))")
    }
} catch { $specs.Add('GPU: unavailable') }
try {
    foreach ($disk in Get-PhysicalDisk) {
        $specs.Add(('Disk: {0}, {1}, {2:N0} GB' -f $disk.MediaType, $disk.BusType, ($disk.Size / 1GB)))
    }
} catch { $specs.Add('Disk: unavailable') }
try {
    foreach ($page in Get-CimInstance Win32_PageFileUsage) {
        $specs.Add("Page file: $($page.AllocatedBaseSize) MB allocated, peak use $($page.PeakUsage) MB")
    }
    if ((Get-CimInstance Win32_ComputerSystem).AutomaticManagedPagefile) { $specs.Add('Page file: system managed') }
} catch { }
$specs.Add('')
$specs.Add('Files: ' + $(if ($collected.Count) { $collected -join ', ' } else { 'no logs found' }))
[IO.File]::WriteAllText((Join-Path $staging 'specs.txt'), (Protect-Text ($specs -join "`r`n") -KeepNames), [Text.UTF8Encoding]::new($false))

# --- Zip ---------------------------------------------------------------------------------------

if (Test-Path -LiteralPath $OutFile) { Remove-Item -LiteralPath $OutFile }
# Entries use forward slashes so the zip opens the same everywhere.
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::Open($OutFile, 'Create')
try {
    foreach ($file in Get-ChildItem -LiteralPath $staging -File -Recurse) {
        $name = $file.FullName.Substring($staging.Length).TrimStart('\').Replace('\', '/')
        $null = [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $file.FullName, $name)
    }
} finally { $zip.Dispose() }
Remove-Item -LiteralPath $staging -Recurse -Force
Write-Host "Wrote $OutFile"
Write-Host 'Open it and check the contents before attaching it to an issue.'
if (!$collected.Count) {
    Write-Host "No logs were found under $InstallDir. Pass -InstallDir if M1LK is installed elsewhere."
}
