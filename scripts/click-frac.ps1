param(
    [double]$Fx = 0.44,
    [double]$Fy = 0.77,
    [string]$Out = '',
    [int]$WaitSec = 2
)

# Default output goes to <repo>\logs\ and the folder is created on demand,
# so this script works on any machine (ASCII-only on purpose).
if (-not $Out) {
    $Out = Join-Path (Split-Path $PSScriptRoot -Parent) 'logs\shot-click-frac.png'
}
$logDir = Split-Path $Out -Parent
if ($logDir -and -not (Test-Path $logDir)) {
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
}

# ASCII-only on purpose: PowerShell 5.1 misparses BOM-less UTF-8 scripts.
# Click at a FRACTION of the app window's physical rect (DPI-aware).
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Frac {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, int e);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    public static void ClickAt(int x, int y) {
        SetCursorPos(x, y);
        System.Threading.Thread.Sleep(150);
        mouse_event(0x0002, 0, 0, 0, 0);
        System.Threading.Thread.Sleep(70);
        mouse_event(0x0004, 0, 0, 0, 0);
    }
}
'@
Add-Type -TypeDefinition $sig -ErrorAction Stop
[void][Frac]::SetProcessDPIAware()

$p = Get-Process sshbox-desktop -ErrorAction SilentlyContinue
if (-not $p) { Write-Host 'ERROR: sshbox-desktop not running'; exit 1 }
$h = $p.MainWindowHandle
[void][Frac]::ShowWindow($h, 9)
[void][Frac]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 700

$r = [Frac+RECT]::new()
[void][Frac]::GetWindowRect($h, [ref]$r)
$w = $r.Right - $r.Left
$hh = $r.Bottom - $r.Top
Write-Host "rect: ($($r.Left),$($r.Top)) ${w}x${hh}"

$x = [int]($r.Left + $w * $Fx)
$y = [int]($r.Top + $hh * $Fy)
Write-Host "click physical ($x,$y) = fraction ($Fx,$Fy)"
[Frac]::ClickAt($x, $y)
Start-Sleep -Seconds $WaitSec

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()
Write-Host "screenshot -> $Out"
