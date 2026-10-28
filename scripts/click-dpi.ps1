param(
    [int]$X = 605,
    [int]$Y = 714,
    [string]$Out = 'C:\Users\Administrator\Documents\ssh-box\logs\shot-click.png',
    [int]$WaitSec = 2
)

# ASCII-only on purpose: PowerShell 5.1 misparses BOM-less UTF-8 scripts.
# Coordinates are PHYSICAL screen pixels (SetProcessDPIAware is called first).
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Dpi {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
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
[void][Dpi]::SetProcessDPIAware()

$p = Get-Process sshbox-desktop -ErrorAction SilentlyContinue
if (-not $p) {
    Write-Host 'ERROR: sshbox-desktop not running'
    exit 1
}
[void][Dpi]::ShowWindow($p.MainWindowHandle, 9)
[void][Dpi]::SetForegroundWindow($p.MainWindowHandle)
Start-Sleep -Milliseconds 600

Write-Host "click physical ($X,$Y)"
[Dpi]::ClickAt($X, $Y)
Start-Sleep -Seconds $WaitSec

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()
Write-Host "screenshot -> $Out"
