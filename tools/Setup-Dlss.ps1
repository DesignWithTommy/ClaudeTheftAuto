<#
.SYNOPSIS
  Installs NVIDIA's DLSS Super Resolution DLL for the optional DLSS upscaler.

.DESCRIPTION
  Downloads nvngx_dlss.dll from a pinned release of NVIDIA's public DLSS
  repository (https://github.com/NVIDIA/DLSS), checks its SHA-256 against the
  pinned value, and copies it to <InstallDir>\dlss. Nothing from NVIDIA is
  shipped with this project; the DLL is fetched from NVIDIA by you, under
  NVIDIA's own license (see the LICENSE in that repository).

  The DLL is used only with VICTORY_UPSCALER=dlss VICTORY_DLSS=<mode>. The
  AMD/FSR path stays the default and needs none of this. The NGX core that
  exposes the D3D12 entry points comes with the NVIDIA display driver.

.PARAMETER InstallDir
  Folder of victory.exe. The DLL goes to <InstallDir>\dlss (or set
  VICTORY_DLSS_DLL_DIR to any folder holding it).

.PARAMETER Tag
  Release tag of github.com/NVIDIA/DLSS to take the DLL from.

.PARAMETER Sha256
  Expected SHA-256 of the DLL for that tag. The script refuses to install
  without one.

.PARAMETER File
  Use a DLL you downloaded yourself instead of fetching one (still checked).

.EXAMPLE
  .\tools\Setup-Dlss.ps1 -InstallDir .\target\release
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $PSScriptRoot '..\target\release'),
    [string]$Tag = 'v310.2.1',
    # TODO: pin the SHA-256 of lib/Windows_x86_64/rel/nvngx_dlss.dll at $Tag.
    # It could not be computed without downloading the file; run once with
    # -File <your download> and read the hash printed on refusal, compare it
    # with the release page, then record it here.
    [string]$Sha256 = '',
    [string]$File = ''
)

$ErrorActionPreference = 'Stop'
$source = "https://raw.githubusercontent.com/NVIDIA/DLSS/$Tag/lib/Windows_x86_64/rel/nvngx_dlss.dll"
$dest = Join-Path $InstallDir 'dlss'

if (-not (Test-Path -LiteralPath $InstallDir -PathType Container)) {
    throw "InstallDir '$InstallDir' does not exist."
}

$temp = Join-Path ([System.IO.Path]::GetTempPath()) ("dlss-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    $dll = Join-Path $temp 'nvngx_dlss.dll'
    if ($File) {
        Copy-Item -LiteralPath $File -Destination $dll
    } else {
        Write-Host "Downloading $source"
        Invoke-WebRequest -Uri $source -OutFile $dll -UseBasicParsing
    }
    $actual = (Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Host "SHA-256 of the DLL: $actual"
    if (-not $Sha256) {
        throw "No pinned SHA-256 (see the TODO in this script). Compare the hash above with NVIDIA's release page, then run again with -Sha256 $actual"
    }
    if ($actual -ne $Sha256.ToLowerInvariant()) {
        throw "SHA-256 mismatch: expected $Sha256, got $actual. Not installed."
    }
    New-Item -ItemType Directory -Path $dest -Force | Out-Null
    Copy-Item -LiteralPath $dll -Destination (Join-Path $dest 'nvngx_dlss.dll') -Force
    Write-Host "Installed $dest\nvngx_dlss.dll"
    Write-Host "Use it with: VICTORY_UPSCALER=dlss VICTORY_DLSS=quality|balanced|performance|ultra"
} finally {
    Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
}
