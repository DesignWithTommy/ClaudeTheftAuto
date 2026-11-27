# keepawake.ps1 - stop Windows sleeping while a test runs (no settings changed; like a video player).
#   keepawake.ps1 [-Minutes 120]   exits after that long, or when killed
param([int] $Minutes = 120)
Add-Type @'
using System.Runtime.InteropServices;
public static class KA { [DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint f); }
'@
$end = (Get-Date).AddMinutes($Minutes)
while ((Get-Date) -lt $end) { [KA]::SetThreadExecutionState(0x80000003) | Out-Null; Start-Sleep -Seconds 30 }
