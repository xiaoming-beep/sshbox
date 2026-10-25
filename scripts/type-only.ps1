param(
    [string]$Text = 'uname -a{ENTER}',
    [string]$Out = 'C:\Users\Administrator\Documents\ssh-box\logs\shot-cmd.png',
    [int]$WaitSeconds = 3
)

# Activate the SSH Box window and type into the focused terminal, then screenshot.
# ASCII-only on purpose: PowerShell 5.1 misparses BOM-less UTF-8 scripts.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$proc = Get-Process -Name 'sshbox-desktop' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $proc) {
    Write-Host 'ERROR: sshbox-desktop not running'
    exit 1
}

$shell = New-Object -ComObject WScript.Shell
$activated = $shell.AppActivate($proc.Id)
Write-Host "AppActivate=$activated pid=$($proc.Id)"
Start-Sleep -Milliseconds 900

if ($Text) {
    Write-Host "type: $Text"
    [System.Windows.Forms.SendKeys]::SendWait($Text)
}

Start-Sleep -Seconds $WaitSeconds

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()

Write-Host "screenshot -> $Out"
