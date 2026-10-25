# Install only Visual Studio Build Tools (MSVC + Windows SDK) with fallbacks.
# ASCII-only (PowerShell 5.1 + BOM-less UTF-8 = mojibake).
$ErrorActionPreference = 'Continue'

$Tmp = Join-Path $env:TEMP 'sshbox-setup'
$Repo = Split-Path $PSScriptRoot -Parent
$Log = Join-Path $Repo 'logs\buildtools.log'
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
New-Item -ItemType Directory -Force -Path (Split-Path $Log) | Out-Null

function Say([string]$message) {
    $line = "$(Get-Date -Format 'HH:mm:ss')  $message"
    Write-Host $line
    Add-Content -Path $Log -Value $line -Encoding UTF8
}

Say 'start'
$exe = Join-Path $Tmp 'vs_BuildTools.exe'
if (-not (Test-Path $exe) -or (Get-Item $exe).Length -lt 1MB) {
    Say 'downloading bootstrapper'
    Invoke-WebRequest 'https://aka.ms/vs/17/release/vs_BuildTools.exe' -OutFile $exe -TimeoutSec 1800
}
Say "bootstrapper: $([math]::Round((Get-Item $exe).Length / 1MB, 1)) MB"

$attempts = @(
    @('--quiet', '--wait', '--norestart',
        '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended'),
    @('--quiet', '--wait', '--norestart',
        '--add', 'Microsoft.VisualStudio.Workload.VCTools'),
    @('--quiet', '--wait', '--norestart')
)

foreach ($argv in $attempts) {
    Say ('try: ' + ($argv -join ' '))
    $proc = Start-Process -FilePath $exe -ArgumentList $argv -Wait -PassThru
    Say "exit=$($proc.ExitCode)"
    if ($proc.ExitCode -eq 0 -or $proc.ExitCode -eq 3010) { break }
}

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) {
    $path = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    Say "VCTools path: $path"
}
else {
    Say 'vswhere missing - nothing installed'
}
Say 'done'
