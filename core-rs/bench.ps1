param(
    [int]$Times = 5
)
# 批量测试：对若干 URL 各下载 N 次，汇总用时/速度/分段，并统计异常行数。
$ErrorActionPreference = "Continue"
Set-Location $PSScriptRoot

$exe = Join-Path $PSScriptRoot "target\release\get.exe"
if (-not (Test-Path $exe)) { Write-Output "找不到 $exe，先编：cargo build --release --bin get"; exit 1 }

$scratch = Join-Path $env:TEMP "dc-bench"
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

$urls = @(
    "https://dldir1v6.qq.com/weixin/Universal/Windows/WeChatWin_4.1.15.exe",
    "https://dl.testfile.cc/100mb.dat"
)

foreach ($u in $urls) {
    Write-Output ""
    Write-Output ("==================== {0} ====================" -f $u)
    $okTimes = 0
    for ($i = 1; $i -le $Times; $i++) {
        $log = Join-Path $scratch "download.log"
        Remove-Item -Force -ErrorAction SilentlyContinue $log
        Push-Location $scratch
        $sw = [Diagnostics.Stopwatch]::StartNew()
        & $exe $u | Out-Null
        $sw.Stop()
        Pop-Location

        $lines = if (Test-Path $log) { Get-Content $log -Encoding UTF8 } else { @() }
        $done = $lines | Select-String "下载完成" | Select-Object -Last 1
        $warn = ($lines | Select-String "\[WARN").Count
        $slow = ($lines | Select-String "过慢").Count
        if ($done) {
            $m = [regex]::Match($done.Line, "用时 ([\d\.]+) 秒，平均 ([\d\.]+) MB/s，分段 (\d+)")
            if ($m.Success) {
                Write-Output ("  第{0}次  OK   用时 {1,6}s   平均 {2,5} MB/s   分段 {3,3}   耗时(墙钟) {4:N1}s   WARN={5} 过慢={6}" -f `
                    $i, $m.Groups[1].Value, $m.Groups[2].Value, $m.Groups[3].Value, $sw.Elapsed.TotalSeconds, $warn, $slow)
                $okTimes++
            } else {
                Write-Output ("  第{0}次  ?    {1}" -f $i, $done.Line)
            }
        } else {
            Write-Output ("  第{0}次  FAIL  未完成（WARN={1} 过慢={2}）" -f $i, $warn, $slow)
        }

        # 清掉下载的文件与临时目录，避免占盘
        Get-ChildItem $scratch -File -ErrorAction SilentlyContinue | Where-Object { $_.Name -ne "download.log" } | Remove-Item -Force -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $scratch ".download-temp")
    }
    Write-Output ("  小结：成功 {0}/{1}" -f $okTimes, $Times)
}
