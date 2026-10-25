param(
    [int]$Dx = 100,
    [int]$Dy = 190,
    [string]$Text = 'uname -a{ENTER}',
    [string]$Out = 'C:\Users\Administrator\Documents\ssh-box\logs\shot-type.png'
)

# Focus the SSH Box window, click inside it, type text, then screenshot.
# ASCII-only on purpose: PowerShell 5.1 misparses BOM-less UTF-8 scripts.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Ui {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern IntPtr FindWindow(string cls, string title);
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")]
    public static extern void mouse_event(uint f, uint dx, uint dy, uint d, int e);
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

$proc = Get-Process -Name 'sshbox-desktop' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $proc) {
    Write-Host 'ERROR: process sshbox-desktop not running'
    exit 1
}

$hwnd = $proc.MainWindowHandle
if ($hwnd -eq [IntPtr]::Zero) {
    Write-Host 'ERROR: main window handle not ready'
    exit 1
}
Write-Host "pid=$($proc.Id) hwnd=$hwnd"

[void][Ui]::ShowWindow($hwnd, 9)
[void][Ui]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 600

$rect = [Ui+RECT]::new()
[void][Ui]::GetWindowRect($hwnd, [ref]$rect)
Write-Host "window: ($($rect.Left),$($rect.Top)) - ($($rect.Right),$($rect.Bottom))"

$cx = $rect.Left + $Dx
$cy = $rect.Top + $Dy
Write-Host "click ($cx,$cy)"
[Ui]::ClickAt($cx, $cy)
Start-Sleep -Milliseconds 900

if ($Text) {
    Write-Host "type: $Text"
    [System.Windows.Forms.SendKeys]::SendWait($Text)
}

Start-Sleep -Seconds 3

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()

Write-Host "screenshot -> $Out"
