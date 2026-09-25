# 编译 Rust 示例程序：注入 build 时间戳，并打印版本号，便于校对。
# 用法：右键"使用 PowerShell 运行"，或命令行执行 .\build.ps1
$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

$cargo = $null
$c = Get-Command cargo -ErrorAction SilentlyContinue
if ($c) { $cargo = $c.Source } else { $cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe" }

$env:BUILD_STAMP = Get-Date -Format 'yyyyMMdd-HHmmss'
& $cargo build --bin get
& (Join-Path $PSScriptRoot "target\debug\get.exe") -v
