# 一键打包：编译 → 补图标 → 输出到 dist\豆包生图服务.exe
# 用法：powershell -ExecutionPolicy Bypass -File build.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

Write-Host "[1/3] cargo build --release" -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "编译失败" }

$exe = "target\release\doubao-img.exe"
if (-not (Test-Path $exe)) { throw "找不到产物 $exe" }

Write-Host "[2/3] 输出到 dist" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path "dist" | Out-Null
Copy-Item $exe "dist\豆包生图服务.exe" -Force

Write-Host "[3/3] 嵌入图标" -ForegroundColor Cyan
if (Test-Path "tools\rcedit-x64.exe") {
    & "tools\rcedit-x64.exe" "dist\豆包生图服务.exe" --set-icon "icon.ico" | Out-Null
} else {
    Write-Host "  跳过：tools\rcedit-x64.exe 不存在" -ForegroundColor Yellow
}

$size = [math]::Round((Get-Item "dist\豆包生图服务.exe").Length / 1KB)
Write-Host "`n完成：dist\豆包生图服务.exe ($size KB)" -ForegroundColor Green
