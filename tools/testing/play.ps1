# play.ps1 - drives a playtest with REAL keyboard and mouse input (SendInput), the same path a
# player uses, so movement, mouse look and menus are tested, not just an idle camera.
# Input is only sent while the game window is in the foreground; if anything else is in
# front, the step is skipped and logged, so nothing is ever typed into another app.
# Each cycle (after -StartAfter seconds of game uptime):
#   click to capture the mouse, walk W / sprint W+Shift / strafe A, D / back S / jump Space,
#   pan the camera 360 deg and up/down, open the pause menu (Esc), move in it (arrows),
#   close it, release the mouse (F8).
# Every step goes to actions.csv (uptime, step, foreground ok, picture change) and a
# screenshot is saved after each menu step to shots\<uptime>-<step>.png.
#   play.ps1 -Out <folder> [-Process victory] [-StartAfter 240] [-Cycle 90]
param([Parameter(Mandatory)] [string] $Out, [string] $Process = 'victory',
      [int] $StartAfter = 240, [int] $Cycle = 90)
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class PI {
  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx, dy; public uint data, flags, time; public IntPtr extra; }
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort vk, scan; public uint flags, time; public IntPtr extra; }
  [StructLayout(LayoutKind.Explicit)] public struct U { [FieldOffset(0)] public MOUSEINPUT m; [FieldOffset(0)] public KEYBDINPUT k; }
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public U u; }
  [DllImport("user32.dll")] static extern uint SendInput(uint n, INPUT[] i, int size);
  [DllImport("user32.dll")] static extern uint MapVirtualKey(uint c, uint t);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  public struct RECT { public int L, T, R, B; }
  public static uint FgPid() { uint p; GetWindowThreadProcessId(GetForegroundWindow(), out p); return p; }
  public static void Key(ushort vk, bool up) {
    var i = new INPUT[1]; i[0].type = 1; i[0].u.k.vk = vk; i[0].u.k.scan = (ushort)MapVirtualKey(vk, 0);
    i[0].u.k.flags = up ? 2u : 0u; SendInput(1, i, Marshal.SizeOf(typeof(INPUT)));
  }
  public static void Mouse(int dx, int dy, uint flags) {
    var i = new INPUT[1]; i[0].type = 0; i[0].u.m.dx = dx; i[0].u.m.dy = dy; i[0].u.m.flags = flags;
    SendInput(1, i, Marshal.SizeOf(typeof(INPUT)));
  }
  public static System.Drawing.Bitmap Grab(IntPtr h) {
    RECT r; GetClientRect(h, out r);
    int w = Math.Max(1, r.R - r.L), ht = Math.Max(1, r.B - r.T);
    var bmp = new System.Drawing.Bitmap(w, ht);
    using (var g = System.Drawing.Graphics.FromImage(bmp)) { IntPtr dc = g.GetHdc(); PrintWindow(h, dc, 3); g.ReleaseHdc(dc); }
    return bmp;
  }
}
'@ -ReferencedAssemblies System.Drawing
$VK = @{ W = 0x57; A = 0x41; S = 0x53; D = 0x44; Space = 0x20; Shift = 0xA0; Esc = 0x1B; Enter = 0x0D
         Up = 0x26; Down = 0x28; Left = 0x25; Right = 0x27; F8 = 0x77 }
New-Item -ItemType Directory -Force (Join-Path $Out 'shots') | Out-Null
$csv = Join-Path $Out 'actions.csv'
'uptime_s,step,foreground,picture_change' | Set-Content $csv
$script:p = $null; $script:prev = $null

function Get-Thumb {
    if (-not $script:p) { return $null }
    try { $b = [PI]::Grab($script:p.MainWindowHandle); $t = New-Object System.Drawing.Bitmap $b, 32, 18
          $o = foreach ($y in 0..17) { foreach ($x in 0..31) { $c = $t.GetPixel($x, $y); ($c.R * 3 + $c.G * 6 + $c.B) / 10 } }
          $t.Dispose(); $b.Dispose(); return $o } catch { return $null }
}
function Save-Shot($name) {
    try { $b = [PI]::Grab($script:p.MainWindowHandle); $s = [Math]::Min(1.0, 960.0 / $b.Width)
          $sm = New-Object System.Drawing.Bitmap $b, ([int]($b.Width * $s)), ([int]($b.Height * $s))
          $sm.Save((Join-Path $Out ("shots\{0:D4}s-{1}.png" -f (Up), $name))); $sm.Dispose(); $b.Dispose() } catch {}
}
function Up { [int]((Get-Date) - $script:p.StartTime).TotalSeconds }
function Test-Front { [PI]::FgPid() -eq [uint32]$script:p.Id }
function Log($step, $ok) {
    $t = Get-Thumb; $d = ''
    if ($t -and $script:prev) { $sum = 0; for ($i = 0; $i -lt $t.Count; $i++) { $sum += [Math]::Abs($t[$i] - $script:prev[$i]) }; $d = [Math]::Round($sum / $t.Count, 1) }
    if ($t) { $script:prev = $t }
    '{0},{1},{2},{3}' -f (Up), $step, $ok, $d | Add-Content $csv
}
function Hold($keys, [double] $sec, $step) {
    if (-not (Test-Front)) { Log $step 'skipped'; return }
    foreach ($k in $keys) { [PI]::Key($VK[$k], $false) }
    $end = (Get-Date).AddSeconds($sec)
    while ((Get-Date) -lt $end) { if (-not (Test-Front)) { break }; Start-Sleep -Milliseconds 50 }
    foreach ($k in $keys) { [PI]::Key($VK[$k], $true) }
    Log $step (Test-Front)
}
# held ~1 s: at 1-2 fps the title polls input once a frame, so a short tap is missed
function Tap($k, $step) { Hold @($k) 1.0 $step }
function Pan([int] $dx, [int] $dy, [double] $sec, $step) {
    if (-not (Test-Front)) { Log $step 'skipped'; return }
    $n = [int]($sec * 20)
    for ($i = 0; $i -lt $n; $i++) { if (-not (Test-Front)) { break }; [PI]::Mouse([int]($dx / $n), [int]($dy / $n), 1); Start-Sleep -Milliseconds 50 }
    Log $step (Test-Front)
}
function Focus {
    [PI]::ShowWindow($script:p.MainWindowHandle, 9) | Out-Null
    # Windows only lets a process take the foreground right after input: tap Alt first
    [PI]::Key(0x12, $false); [PI]::Key(0x12, $true)
    [PI]::SetForegroundWindow($script:p.MainWindowHandle) | Out-Null
    Start-Sleep -Milliseconds 400
    if (-not (Test-Front)) { Log 'focus' 'skipped'; return $false }
    $r = New-Object PI+RECT; [PI]::GetWindowRect($script:p.MainWindowHandle, [ref]$r) | Out-Null
    [PI]::SetCursorPos([int](($r.L + $r.R) / 2), [int](($r.T + $r.B) / 2)) | Out-Null
    [PI]::Mouse(0, 0, 2); Start-Sleep -Milliseconds 80; [PI]::Mouse(0, 0, 4)   # left click: capture mouse
    Log 'focus+click' $true; return $true
}

# wait for the game window
$t0 = Get-Date
while (-not ($script:p = Get-Process $Process -ErrorAction SilentlyContinue | Where-Object MainWindowHandle -ne 0 | Select-Object -First 1)) {
    if (((Get-Date) - $t0).TotalMinutes -gt 5) { exit }; Start-Sleep 2 }
while ((Up) -lt $StartAfter) { if ($script:p.HasExited) { exit }; Start-Sleep 2 }

$cycle = 0
while (-not $script:p.HasExited) {
    $start = Get-Date; $cycle++
    if (Focus) {
        Save-Shot "c$cycle-start"
        Hold @('W') 4 'walk'
        Hold @('W', 'Shift') 3 'sprint'
        Hold @('A') 1.5 'strafe-left'
        Hold @('D') 1.5 'strafe-right'
        Hold @('S') 2 'back'
        Tap 'Space' 'jump'
        Pan 1600 0 3 'look-360'
        Pan 0 -300 1 'look-up'
        Pan 0 300 1 'look-down'
        Save-Shot "c$cycle-moved"
        Tap 'Esc' 'pause-menu'; Start-Sleep 3; Save-Shot "c$cycle-menu"
        Tap 'Down' 'menu-down'; Start-Sleep 1; Tap 'Down' 'menu-down'; Start-Sleep 1
        Start-Sleep 2; Save-Shot "c$cycle-menu-nav"
        Tap 'Up' 'menu-up'; Start-Sleep 1; Tap 'Up' 'menu-up'
        Tap 'Esc' 'menu-close'; Start-Sleep 3
        Save-Shot "c$cycle-closed"
        Tap 'F8' 'release-mouse'
    }
    while (((Get-Date) - $start).TotalSeconds -lt $Cycle -and -not $script:p.HasExited) { Start-Sleep 2 }
}




