param(
    [int]$Dx = 93,
    [int]$Dy = 145,
    [string]$Out = 'C:\Users\Administrator\Documents\ssh-box\logs\screen3.png'
)

# ASCII-only on purpose: PowerShell 5.1 misparses BOM-less UTF-8 scripts.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Win {
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

$hwnd = [Win]::FindWindow($null, 'SSH Box')
if ($hwnd -eq [IntPtr]::Zero) {
    Write-Host 'ERROR: SSH Box window not found'
    exit 1
}

[void][Win]::ShowWindow($hwnd, 9)
[void][Win]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 500

$rect = [Win+RECT]::new()
[void][Win]::GetWindowRect($hwnd, [ref]$rect)
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top
Write-Host "window rect: ($($rect.Left),$($rect.Top)) size ${w}x${h}"

$x = $rect.Left + $Dx
$y = $rect.Top + $Dy
Write-Host "click at ($x,$y)"
[Win]::ClickAt($x, $y)

Start-Sleep -Seconds 6

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()

Write-Host "screenshot -> $Out"
