# Launcher: run setup-dev.ps1 and write ALL output (including Write-Host) to logs\setup.log
# ASCII-only on purpose (PowerShell 5.1 + BOM-less UTF-8 = mojibake).
$repo = Split-Path $PSScriptRoot -Parent
$log = Join-Path $repo 'logs\setup.log'
New-Item -ItemType Directory -Force -Path (Split-Path $log) | Out-Null

"$(Get-Date -Format o)  start" | Set-Content -Path $log -Encoding UTF8

$code = 0
try {
    & (Join-Path $PSScriptRoot 'setup-dev.ps1') *>&1 | Out-File -FilePath $log -Append -Encoding utf8
}
catch {
    "ERROR: $_" | Out-File -FilePath $log -Append -Encoding utf8
    $code = 1
}

"### SETUP_DONE code=$code  $(Get-Date -Format o)" | Out-File -FilePath $log -Append -Encoding utf8
exit $code
