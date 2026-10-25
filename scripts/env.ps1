# 在当前 PowerShell 会话里刷新 PATH（安装完工具链后用）
# 用法： . .\scripts\env.ps1
$machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
$user = [Environment]::GetEnvironmentVariable('Path', 'User')
$extra = Join-Path $env:USERPROFILE '.cargo\bin'
$env:Path = (@($extra, $machine, $user) | Where-Object { $_ }) -join ';'
Write-Host "PATH 已刷新：node=$(node -v 2>$null) cargo=$(cargo --version 2>$null)"
