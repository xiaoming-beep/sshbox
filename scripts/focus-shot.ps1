param(
    [string]$ProcessName = 'sshbox-desktop',
    [string]$Out = ''
)

# Default output goes to <repo>\logs\ and the folder is created on demand,
# so this script works on any machine (ASCII-only on purpose).
if (-not $Out) {
    $Out = Join-Path (Split-Path $PSScriptRoot -Parent) 'logs\shot.png'
}
$logDir = Split-Path $Out -Parent
if ($logDir -and -not (Test-Path $logDir)) {
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
}

# ASCII-only on purpose: PowerShell 5.1 misparses BOM-less UTF-8 scripts.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$sig = @'
using System;
using System.Runtime.InteropServices;
public static class Focus {
    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")]
    public static extern bool BringWindowToTop(IntPtr h);
}
'@

Add-Type -TypeDefinition $sig -ErrorAction Stop

$proc = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $proc) {
    Write-Host "ERROR: process $ProcessName not running"
    exit 1
}

$hwnd = $proc.MainWindowHandle
Write-Host "pid=$($proc.Id) title='$($proc.MainWindowTitle)' hwnd=$hwnd"

[void][Focus]::ShowWindow($hwnd, 9)
[void][Focus]::BringWindowToTop($hwnd)
[void][Focus]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 900

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()

Write-Host "screenshot -> $Out"
