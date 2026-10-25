#Requires -RunAsAdministrator
<#
    SSH Box dev environment bootstrap (Windows 10/11 x64).
    Installs: Node.js LTS -> Rust (stable-x86_64-pc-windows-msvc) -> VS Build Tools (VCTools) -> pnpm
    Idempotent: already-installed components are skipped.

    NOTE: this file is intentionally ASCII-only. Windows PowerShell 5.1 decodes
    BOM-less UTF-8 scripts as ANSI and mangles non-ASCII text.
#>
[CmdletBinding()]
param(
    [switch]$SkipBuildTools,
    [switch]$SkipNode,
    [switch]$SkipRust,
    [switch]$SkipPnpm
)

# 'Continue' on purpose: native tools (rustup-init, msiexec, vs_BuildTools) print warnings to stderr,
# and with 'Stop' those become terminating NativeCommandError. We check exit codes explicitly instead.
$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$Tmp = Join-Path $env:TEMP 'sshbox-setup'
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null

function Step([string]$title) {
    Write-Host ""
    Write-Host "=== $title ==="
}
function Have([string]$cmd) {
    return [bool](Get-Command $cmd -ErrorAction SilentlyContinue)
}
function Sync-Path {
    $env:Path = (@(
            (Join-Path $env:USERPROFILE '.cargo\bin'),
            [Environment]::GetEnvironmentVariable('Path', 'Machine'),
            [Environment]::GetEnvironmentVariable('Path', 'User')
        ) | Where-Object { $_ }) -join ';'
}

Step 'Node.js LTS'
if ($SkipNode) {
    Write-Host 'skip (-SkipNode)'
}
elseif (Have 'node') {
    Write-Host "installed: node $(node -v)"
}
else {
    $idx = Invoke-RestMethod 'https://nodejs.org/dist/index.json' -TimeoutSec 60
    $ver = ($idx | Where-Object { $_.lts } | Select-Object -First 1).version
    $msi = Join-Path $Tmp "node-$ver-x64.msi"
    if (-not (Test-Path $msi) -or (Get-Item $msi).Length -lt 10MB) {
        Write-Host "downloading node $ver ..."
        Invoke-WebRequest "https://nodejs.org/dist/$ver/node-$ver-x64.msi" -OutFile $msi -TimeoutSec 1800
    }
    Write-Host 'installing ...'
    Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart" -Wait
    Sync-Path
    if (-not (Have 'node')) { throw 'node not found after install' }
    Write-Host "done: node $(node -v) / npm $(npm -v)"
}

Step 'Rust (stable-x86_64-pc-windows-msvc)'
if ($SkipRust) {
    Write-Host 'skip (-SkipRust)'
}
elseif ((Have 'cargo') -and (Have 'rustc')) {
    Write-Host "installed: $(rustc --version)"
}
else {
    $init = Join-Path $Tmp 'rustup-init.exe'
    if (-not (Test-Path $init) -or (Get-Item $init).Length -lt 1MB) {
        Write-Host 'downloading rustup-init ...'
        Invoke-WebRequest 'https://win.rustup.rs/x86_64' -OutFile $init -TimeoutSec 1800
    }
    & $init -y --default-host x86_64-pc-windows-msvc --default-toolchain stable --profile default
    if ($LASTEXITCODE -ne 0) { throw "rustup-init exited with $LASTEXITCODE" }
    Sync-Path
    if (-not (Have 'cargo')) { throw 'cargo not found after install' }
    Write-Host "done: $(rustc --version) / $(cargo --version)"
}

Step 'Visual Studio Build Tools (MSVC + Windows SDK)'
if ($SkipBuildTools) {
    Write-Host 'skip (-SkipBuildTools)'
}
else {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $installed = $null
    if (Test-Path $vswhere) {
        $installed = & $vswhere -latest -products * `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -property installationPath
    }
    if ($installed) {
        Write-Host "installed: $installed"
    }
    else {
        $exe = Join-Path $Tmp 'vs_BuildTools.exe'
        if (-not (Test-Path $exe) -or (Get-Item $exe).Length -lt 1MB) {
            Write-Host 'downloading vs_BuildTools bootstrapper ...'
            Invoke-WebRequest 'https://aka.ms/vs/17/release/vs_BuildTools.exe' -OutFile $exe -TimeoutSec 1800
        }
        Write-Host 'installing VCTools workload (2-3 GB, 10-25 min) ...'
        # Standard documented form. An explicit SDK component id (e.g. Windows11SDK.22621) is NOT
        # portable across channels and makes the installer exit with 87.
        $p = Start-Process -FilePath $exe -ArgumentList @(
            '--quiet', '--wait', '--norestart',
            '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended'
        ) -Wait -PassThru
        Write-Host "installer exit code: $($p.ExitCode)"
        if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) {
            throw "Build Tools install failed ($($p.ExitCode))"
        }
    }
}

Step 'pnpm'
if ($SkipPnpm) {
    Write-Host 'skip (-SkipPnpm)'
}
elseif (Have 'pnpm') {
    Write-Host "installed: pnpm $(pnpm -v)"
}
else {
    npm i -g --allow-scripts=pnpm pnpm
    Write-Host "done: pnpm $(pnpm -v)"
}

Step 'verify'
Sync-Path
$rows = [ordered]@{
    'node'     = if (Have 'node') { (node -v) } else { 'MISSING' }
    'npm'      = if (Have 'npm') { (npm -v) } else { 'MISSING' }
    'pnpm'     = if (Have 'pnpm') { (pnpm -v) } else { 'MISSING' }
    'rustc'    = if (Have 'rustc') { (rustc --version) } else { 'MISSING' }
    'cargo'    = if (Have 'cargo') { (cargo --version) } else { 'MISSING' }
    'link.exe' = if (Have 'link.exe') { 'OK' } else { 'NOT-IN-PATH (build uses vcvars)' }
}
$rows.GetEnumerator() | ForEach-Object { Write-Host ("  {0,-9} {1}" -f $_.Key, $_.Value) }

Write-Host ""
Write-Host "=== done ==="
Write-Host 'New PATH entries require a new shell. Run: . .\scripts\env.ps1'
