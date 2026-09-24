package main

import (
	"bufio"
	"bytes"
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"downloadcore"
	"downloadcore/testserver"
)

// 这是唯一的一个测试程序：
//   - 双击运行会弹出 CMD 窗口
//   - 提示输入网址，回车后下载到"当前文件夹"
//   - 全程写详细日志到 download.log，出错好查
//   - 输入 selftest 可不联网自检
//   - 运行 `下载.exe -v` 只打印版本号
const logFileName = "download.log"

// appVersion 是版本号。每次改动后请递增，方便核对"测的是哪一版"。
const appVersion = "0.3.4"

// buildStamp 是编译时间戳，由编译命令注入（-ldflags），默认 dev。
var buildStamp = "dev"

func main() {
	// `下载.exe -v` 只打印版本，便于核对
	if len(os.Args) > 1 {
		switch os.Args[1] {
		case "-v", "-version", "--version", "version":
			fmt.Printf("简单下载器 v%s (build %s)\n", appVersion, buildStamp)
			return
		}
	}

	dir, _ := os.Getwd()

	fmt.Println("==============================================")
	fmt.Printf("  简单下载器  v%s  (build %s)\n", appVersion, buildStamp)
	fmt.Println("==============================================")
	fmt.Printf("文件保存到：%s\n", dir)
	fmt.Printf("详细日志：  %s\n", filepath.Join(dir, logFileName))
	fmt.Println("提示：输入 selftest 可不联网自检；直接回车退出。")
	fmt.Println()

	lg, err := newLogger(filepath.Join(dir, logFileName))
	if err != nil {
		fmt.Println("无法创建日志文件：", err)
		os.Exit(1)
	}
	defer lg.Close()
	lg.log("INFO", "程序启动 v%s (build %s)，工作目录=%s", appVersion, buildStamp, dir)

	reader := bufio.NewReader(os.Stdin)
	for {
		fmt.Print("请输入下载网址（直接回车退出）：")
		line, _ := reader.ReadString('\n')
		link := strings.TrimSpace(line)
		if link == "" {
			lg.log("INFO", "用户选择退出")
			fmt.Println("已退出。")
			return
		}
		if strings.EqualFold(link, "selftest") {
			runSelfTest(lg, dir)
		} else {
			downloadOne(lg, link, dir)
		}
		fmt.Println()
	}
}

// makeCallbacks 把引擎的进度/状态/日志统一接到我们的日志器上。
func makeCallbacks(lg *logger) downloadcore.Callbacks {
	lastProgress := time.Now()
	return downloadcore.Callbacks{
		OnStatus: func(s downloadcore.Status) {
			lg.log("INFO", "状态：%s", s)
		},
		OnLog: func(entry downloadcore.LogEntry) {
			lg.log(entry.Level, "%s", entry.Message)
		},
		OnProgress: func(p downloadcore.Progress) {
			if time.Since(lastProgress) < time.Second {
				return
			}
			lastProgress = time.Now()
			pct := 0.0
			if p.Total > 0 {
				pct = float64(p.Downloaded) / float64(p.Total) * 100
			}
			lg.log("INFO", "进度：%.1f%%  已下 %.2f/%.2f MB  %.2f MB/s  分段 %d",
				pct,
				float64(p.Downloaded)/1024/1024,
				float64(p.Total)/1024/1024,
				float64(p.Speed)/1024/1024,
				p.Parts)
		},
	}
}

func downloadOne(lg *logger, link, dir string) {
	cfg := downloadcore.DefaultConfig()
	cfg.TempDir = filepath.Join(dir, ".download-temp")
	e := downloadcore.New(cfg)

	res, err := e.Download(
		context.Background(),
		downloadcore.Request{URL: link, TargetDir: dir},
		makeCallbacks(lg),
	)
	if err != nil {
		lg.log("ERROR", "下载失败：%v", err)
		fmt.Println(">>> 下载失败！详细原因见 download.log 里最后几行。")
		return
	}
	abs, _ := filepath.Abs(res.Path)
	lg.log("INFO", "下载成功：%s（%d 字节，平均 %.2f MB/s，分段 %d）",
		abs, res.Size, float64(res.Speed)/1024/1024, res.Parts)
	fmt.Printf(">>> 下载完成：%s\n", abs)
}

// runSelfTest 不联网：起一个本地测试服务器，完整下一遍并校验。
func runSelfTest(lg *logger, dir string) {
	lg.log("INFO", "开始离线自检（不联网）")

	const size = 8 << 20
	data := make([]byte, size)
	for i := range data {
		data[i] = byte((i*31 + 7) & 0xff)
	}

	srv := testserver.New(data)
	defer srv.Close()
	srv.SetSpeed(8 << 20)

	cfg := downloadcore.DefaultConfig()
	cfg.TempDir = filepath.Join(dir, ".download-temp")
	cfg.InitialThreads = 1
	cfg.MaxThreads = 4
	cfg.MinPartSize = 512 << 10
	e := downloadcore.New(cfg)

	res, err := e.Download(
		context.Background(),
		downloadcore.Request{URL: srv.URL(), TargetDir: dir},
		makeCallbacks(lg),
	)
	if err != nil {
		lg.log("ERROR", "自检下载失败：%v", err)
		fmt.Println(">>> 自检失败！详见 download.log")
		return
	}

	got, err := os.ReadFile(res.Path)
	if err != nil {
		lg.log("ERROR", "自检读取结果失败：%v", err)
		fmt.Println(">>> 自检失败！详见 download.log")
		return
	}
	if bytes.Equal(got, data) {
		lg.log("INFO", "自检通过：与原文件逐字节一致 ✔")
		fmt.Println(">>> 自检通过 ✔")
	} else {
		lg.log("ERROR", "自检失败：内容不一致")
		fmt.Println(">>> 自检失败 ✘")
	}

	_ = os.Remove(res.Path)
	_ = os.RemoveAll(cfg.TempDir)
}

// --- 详细日志器：同时写到控制台（INFO 以上）和日志文件（全部） ---

type logger struct {
	mu sync.Mutex
	f  *os.File
}

func newLogger(path string) (*logger, error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0644)
	if err != nil {
		return nil, err
	}
	// 新文件开头写 UTF-8 BOM，方便 Windows 记事本等工具正确识别中文
	if fi, statErr := f.Stat(); statErr == nil && fi.Size() == 0 {
		_, _ = f.WriteString("\ufeff")
	}
	return &logger{f: f}, nil
}

func (l *logger) Close() {
	if l != nil && l.f != nil {
		_ = l.f.Close()
	}
}

func (l *logger) log(level, format string, args ...any) {
	line := fmt.Sprintf("%s [%-5s] %s",
		time.Now().Format("2006-01-02 15:04:05.000"), level, fmt.Sprintf(format, args...))
	l.mu.Lock()
	defer l.mu.Unlock()
	if level != "DEBUG" {
		fmt.Fprintln(os.Stdout, line)
	}
	if l.f != nil {
		fmt.Fprintln(l.f, line)
		_ = l.f.Sync()
	}
}
