# 编译 Rust 示例程序：写入新的 build 时间戳，编译后打印版本号，便于校对。
# 用法：右键"使用 PowerShell 运行"，或命令行执行 .\build.ps1
$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

$cargo = $null
$c = Get-Command cargo -ErrorAction SilentlyContinue
if ($c) { $cargo = $c.Source } else { $cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe" }

# 写入时间戳文件；build.rs 监听它，从而每次编译都刷新版本
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
[System.IO.File]::WriteAllText((Join-Path $PSScriptRoot "build-stamp.txt"), $stamp)

& $cargo build --bin get
& (Join-Path $PSScriptRoot "target\debug\get.exe") -v
