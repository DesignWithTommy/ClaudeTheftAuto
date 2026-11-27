# watch.ps1 - event-driven screenshots and status while the runtime runs.
# Every $Poll seconds it grabs a small thumbnail of the game window and decides whether
# to save a full screenshot (game + HUD):
#   - the picture changed a lot since the last saved shot (new scene, menu, black/white screen)
#   - private memory grew by >= $MemStepGB since the last shot
#   - the runtime log gained a new WARN/ERROR/crash line
#   - nothing saved for $MaxGap seconds (heartbeat)
# Each save appends the reason to events.txt. On exit, copies the newest runtime logs.
#   watch.ps1 -Out <folder> [-Process victory] [-LogDir <dir>] [-Poll 3] [-MaxGap 60]
param([Parameter(Mandatory)] [string] $Out, [string] $Process = 'victory', [string] $LogDir = '',
      [int] $Poll = 3, [int] $MaxGap = 60, [double] $Change = 18, [double] $MemStepGB = 1.0)
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class WW {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  public struct RECT { public int L, T, R, B; }
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  public static IntPtr Largest(uint pid) {
    IntPtr best = IntPtr.Zero; long area = 0;
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p);
      if (p == pid && IsWindowVisible(h)) { RECT r; GetClientRect(h, out r);
        long a = (long)(r.R - r.L) * (r.B - r.T); if (a > area) { area = a; best = h; } }
      return true; }, IntPtr.Zero);
    return best;
  }
  public static System.Drawing.Bitmap Grab(IntPtr h) {
    RECT r; GetClientRect(h, out r);
    int w = Math.Max(1, r.R - r.L), ht = Math.Max(1, r.B - r.T);
    var bmp = new System.Drawing.Bitmap(w, ht);
    using (var g = System.Drawing.Graphics.FromImage(bmp)) { IntPtr dc = g.GetHdc(); PrintWindow(h, dc, 3); g.ReleaseHdc(dc); }
    return bmp;
  }
  // 32x18 grey thumbnail as bytes
  public static byte[] Thumb(System.Drawing.Bitmap b) {
    using (var t = new System.Drawing.Bitmap(b, 32, 18)) {
      var o = new byte[32 * 18];
      for (int y = 0; y < 18; y++) for (int x = 0; x < 32; x++) { var c = t.GetPixel(x, y); o[y * 32 + x] = (byte)((c.R * 3 + c.G * 6 + c.B) / 10); }
      return o;
    }
  }
}
'@ -ReferencedAssemblies System.Drawing
New-Item -ItemType Directory -Force (Join-Path $Out 'shots') | Out-Null
$events = Join-Path $Out 'events.txt'
function Save-Bmp($bmp, $path) {
    $scale = [Math]::Min(1.0, 960.0 / $bmp.Width)
    $small = New-Object System.Drawing.Bitmap $bmp, ([int]($bmp.Width * $scale)), ([int]($bmp.Height * $scale))
    $small.Save($path); $small.Dispose()
}
$started = Get-Date; $seen = $false
$lastThumb = $null; $lastSave = [datetime]::MinValue; $lastMem = 0.0; $logPos = 0L
while ($true) {
    $p = Get-Process $Process -ErrorAction SilentlyContinue | Where-Object MainWindowHandle -ne 0 | Select-Object -First 1
    if (-not $p) {
        # Keep waiting while the process exists, or (for up to 3 min) until it first appears.
        if ((Get-Process $Process -ErrorAction SilentlyContinue) -or (-not $seen -and ((Get-Date) - $started).TotalSeconds -lt 180)) { Start-Sleep -Seconds $Poll; continue }
        break
    }
    $seen = $true
    $reasons = @()
    $mem = $p.PrivateMemorySize64 / 1GB
    $game = [WW]::Largest([uint32]$p.Id)
    $bmp = $null; $thumb = $null; $diff = 0.0; $bright = 0.0
    if ($game -ne [IntPtr]::Zero) {
        try {
            $bmp = [WW]::Grab($game); $thumb = [WW]::Thumb($bmp)
            $bright = ($thumb | Measure-Object -Average).Average
            if ($lastThumb) { $sum = 0; for ($i = 0; $i -lt $thumb.Length; $i++) { $sum += [Math]::Abs($thumb[$i] - $lastThumb[$i]) }; $diff = $sum / $thumb.Length }
            else { $reasons += 'first' }
        } catch {}
    }
    if ($diff -ge $Change) { $reasons += ('picture changed {0:N0}' -f $diff) }
    if ($mem - $lastMem -ge $MemStepGB) { $reasons += ('memory +{0:N1} GB' -f ($mem - $lastMem)) }
    if ($LogDir) {
        $log = Get-ChildItem $LogDir -File -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($log -and $log.Length -gt $logPos) {
            try {
                $fs = [IO.File]::Open($log.FullName, 'Open', 'Read', 'ReadWrite'); $fs.Seek($logPos, 'Begin') | Out-Null
                $new = (New-Object IO.StreamReader $fs).ReadToEnd(); $fs.Close(); $logPos = $log.Length
                $bad = $new -split "`n" | Where-Object { $_ -match ' (WARN|ERROR) |panick|crash|watchdog|device removed|TDR' } | Select-Object -First 3
                if ($bad) { $reasons += 'log: ' + (($bad | ForEach-Object { $_.Trim() }) -join ' | ').Substring(0, [Math]::Min(300, (($bad -join ' | ').Length))) }
            } catch {}
        }
    }
    if (((Get-Date) - $lastSave).TotalSeconds -ge $MaxGap) { $reasons += 'heartbeat' }
    if ($reasons -and $bmp) {
        $up = [int]((Get-Date) - $p.StartTime).TotalSeconds
        $tag = '{0:D4}s' -f $up
        Save-Bmp $bmp (Join-Path $Out "shots\$tag-game.png")
        try { $hb = [WW]::Grab($p.MainWindowHandle); Save-Bmp $hb (Join-Path $Out "shots\$tag-hud.png"); $hb.Dispose() } catch {}
        $free = [math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1MB, 1)
        '{0} up={1}s private={2:N1}GB freeRAM={3}GB bright={4:N0} | {5}' -f (Get-Date -Format HH:mm:ss), $up, $mem, $free, $bright, ($reasons -join '; ') | Add-Content $events
        $lastThumb = $thumb; $lastSave = Get-Date; $lastMem = $mem
    }
    if ($bmp) { $bmp.Dispose() }
    Start-Sleep -Seconds $Poll
}
if ($LogDir -and (Test-Path $LogDir)) {
    Get-ChildItem $LogDir -File | Sort-Object LastWriteTime -Descending | Select-Object -First 2 | Copy-Item -Destination $Out -Force
}
"done $(Get-Date -Format HH:mm:ss)" | Add-Content $events
