# 一键打包：编译 → 输出到 dist\豆包生图服务.exe
# 用法：powershell -ExecutionPolicy Bypass -File build.ps1
# 图标由 build.rs 在编译期用 windres 嵌进 exe（找不到 windres 也能编，只是没图标）
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

Write-Host "[1/2] cargo build --release" -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "编译失败" }

$exe = "target\release\doubao-img.exe"
if (-not (Test-Path $exe)) { throw "找不到产物 $exe" }

Write-Host "[2/2] 输出到 dist" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path "dist" | Out-Null
Copy-Item $exe "dist\豆包生图服务.exe" -Force

$size = [math]::Round((Get-Item "dist\豆包生图服务.exe").Length / 1KB)
Write-Host "`n完成：dist\豆包生图服务.exe ($size KB)" -ForegroundColor Green
