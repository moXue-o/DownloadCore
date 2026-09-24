# 编译测试程序：注入编译时间戳，并打印版本号，便于校对。
# 用法：右键"使用 PowerShell 运行"，或命令行执行 .\build.ps1
$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
go build -ldflags "-X main.buildStamp=$stamp" -o 下载.exe ./cmd/get

# 打印版本号，和程序启动时显示的一致，用于校对
& .\下载.exe -v
