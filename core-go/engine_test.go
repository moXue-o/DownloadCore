package downloadcore

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"

	"downloadcore/testserver"
)

func testConfig(t *testing.T) Config {
	t.Helper()
	c := DefaultConfig()
	c.InitialThreads = 1
	c.MaxThreads = 4
	c.MinPartSize = 256 << 10
	c.BufferSize = 64 << 10
	c.IdleTimeout = 2 * time.Second
	c.MaxRetries = 2
	c.RetryDelay = 20 * time.Millisecond
	c.TempDir = t.TempDir()
	return c
}

func makeData(n int, seed byte) []byte {
	b := make([]byte, n)
	for i := range b {
		b[i] = byte((i*31 + int(seed)*7) & 0xff)
	}
	return b
}

func requireFileEquals(t *testing.T, path string, want []byte) {
	t.Helper()
	got, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("读取文件失败: %v", err)
	}
	if !bytes.Equal(got, want) {
		t.Fatalf("文件内容不一致: got %d 字节, want %d 字节", len(got), len(want))
	}
}

// --- 单元测试：切分与分裂 ---

func TestSplitToRange(t *testing.T) {
	r := splitToRange(10, 3, 4)
	if len(r) != 4 {
		t.Fatalf("期望 4 段，实际 %d", len(r))
	}
	if r[0][0] != 0 || r[len(r)-1][1] != 9 {
		t.Fatalf("段未覆盖整个文件: %v", r)
	}
	var sum int64
	for i, s := range r {
		sum += s[1] - s[0] + 1
		if i > 0 && s[0] != r[i-1][1]+1 {
			t.Fatalf("段之间不连续: %v", r)
		}
	}
	if sum != 10 {
		t.Fatalf("总长度应为 10，实际 %d", sum)
	}
}

func TestPartSplitKeepsPrefixAndSafeZone(t *testing.T) {
	origTo := int64(10)*safetyStep - 1
	p := newPart(0, origTo)
	sz := p.snapshot
	_, _, _, safeBefore := sz()
	np := p.split()
	if np == nil {
		t.Fatal("应当可以分裂")
	}
	if np.from != p.to+1 {
		t.Fatalf("两段必须首尾相接: old.to=%d new.from=%d", p.to, np.from)
	}
	if np.to != origTo {
		t.Fatalf("后半段应当接到原末尾: %d != %d", np.to, origTo)
	}
	if p.to < safeBefore {
		t.Fatalf("分裂后前半段不能小于安全区: to=%d safe=%d", p.to, safeBefore)
	}
}

// 分裂必须优先切"剩下活最多"的那段，否则负载会严重不均。
func TestSplitPicksLargestRemaining(t *testing.T) {
	j := &job{cfg: Config{MinPartSize: safetyStep}}
	a := newPart(0, 1<<20-1)     // 1 MiB，太小，不该被切
	b := newPart(1<<20, 9<<20-1) // 8 MiB，应当被切
	j.parts = []*part{a, b}

	np := j.splitOneLocked()
	if np == nil {
		t.Fatal("应当能分裂")
	}
	if np.from <= 1<<20 {
		t.Fatalf("应当拆分更大的那段，实际新增 from=%d", np.from)
	}
	if a.to != 1<<20-1 {
		t.Fatalf("小段不该被动：to=%d", a.to)
	}
}

// --- 端到端测试 ---

func TestDownloadDynamicSegmentation(t *testing.T) {
	data := makeData(4<<20, 1)
	srv := testserver.New(data)
	defer srv.Close()

	e := New(testConfig(t))
	target := filepath.Join(t.TempDir(), "out.bin")

	res, err := e.Download(context.Background(), Request{URL: srv.URL(), TargetFile: target}, Callbacks{})
	if err != nil {
		t.Fatalf("下载失败: %v", err)
	}
	if !res.RangeOK {
		t.Fatal("应当支持分段")
	}
	if res.Parts < 2 {
		t.Fatalf("期望动态分段产生多于 1 段，实际 %d", res.Parts)
	}
	requireFileEquals(t, target, data)
}

func TestNoRangeFallsBackToSingleStream(t *testing.T) {
	data := makeData(1<<20, 2)
	srv := testserver.New(data)
	defer srv.Close()
	srv.SetNoRange(true)

	e := New(testConfig(t))
	target := filepath.Join(t.TempDir(), "out.bin")

	res, err := e.Download(context.Background(), Request{URL: srv.URL(), TargetFile: target}, Callbacks{})
	if err != nil {
		t.Fatalf("下载失败: %v", err)
	}
	if res.RangeOK {
		t.Fatal("服务器不支持分段，RangeOK 应为 false")
	}
	requireFileEquals(t, target, data)
}

func TestRangeMismatchDetected(t *testing.T) {
	data := makeData(1<<20, 3)
	srv := testserver.New(data)
	defer srv.Close()
	srv.SetFailRange(true)

	cfg := testConfig(t)
	cfg.InitialThreads = 1
	cfg.MaxThreads = 1
	cfg.MaxRetries = 1
	cfg.RetryDelay = 10 * time.Millisecond
	e := New(cfg)
	target := filepath.Join(t.TempDir(), "out.bin")

	_, err := e.Download(context.Background(), Request{URL: srv.URL(), TargetFile: target}, Callbacks{})
	if err == nil {
		t.Fatal("服务器返回错误分段，应当报错")
	}
	if !errors.Is(err, ErrTooManyFailures) {
		t.Fatalf("期望重试耗尽错误，实际: %v", err)
	}
}

func TestIdleTimeoutRetries(t *testing.T) {
	data := makeData(2<<20, 4)
	srv := testserver.New(data)
	defer srv.Close()
	srv.SetStallAfter(64 << 10)

	cfg := testConfig(t)
	cfg.InitialThreads = 1
	cfg.MaxThreads = 1
	cfg.IdleTimeout = 300 * time.Millisecond
	cfg.MaxRetries = 1
	cfg.RetryDelay = 10 * time.Millisecond
	e := New(cfg)
	target := filepath.Join(t.TempDir(), "out.bin")

	_, err := e.Download(context.Background(), Request{URL: srv.URL(), TargetFile: target}, Callbacks{})
	if err == nil {
		t.Fatal("连接卡住，最终应当报错")
	}
	if !errors.Is(err, ErrTooManyFailures) {
		t.Fatalf("期望重试耗尽错误，实际: %v", err)
	}
}

func TestCancelThenResume(t *testing.T) {
	data := makeData(4<<20, 5)
	srv := testserver.New(data)
	defer srv.Close()
	srv.SetSpeed(2 << 20) // 2 MiB/s，保证能中途取消

	cfg := testConfig(t)
	cfg.MaxRetries = 0
	cfg.IdleTimeout = 5 * time.Second
	e := New(cfg)
	target := filepath.Join(t.TempDir(), "out.bin")

	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(400 * time.Millisecond)
		cancel()
	}()

	_, err := e.Download(ctx, Request{URL: srv.URL(), TargetFile: target}, Callbacks{})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("期望取消错误，实际: %v", err)
	}
	if _, statErr := os.Stat(target); !os.IsNotExist(statErr) {
		t.Fatal("取消后不应存在最终文件")
	}

	// 恢复：全速重下剩余部分
	srv.SetSpeed(0)
	res, err := e.Download(context.Background(), Request{URL: srv.URL(), TargetFile: target}, Callbacks{})
	if err != nil {
		t.Fatalf("续传失败: %v", err)
	}
	requireFileEquals(t, target, data)
	_ = res
}

func TestFileChangedOnResumeStartsFresh(t *testing.T) {
	data1 := makeData(2<<20, 6)
	data2 := makeData(2<<20, 7)
	srv := testserver.New(data1)
	defer srv.Close()
	srv.SetSpeed(1 << 20)

	cfg := testConfig(t)
	cfg.MaxRetries = 0
	cfg.IdleTimeout = 5 * time.Second
	e := New(cfg)
	target := filepath.Join(t.TempDir(), "out.bin")

	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(300 * time.Millisecond)
		cancel()
	}()
	if _, err := e.Download(ctx, Request{URL: srv.URL(), TargetFile: target}, Callbacks{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("期望取消错误，实际: %v", err)
	}

	// 服务器上的文件变了
	srv.SetData(data2, `"v2"`)
	srv.SetSpeed(0)

	if _, err := e.Download(context.Background(), Request{URL: srv.URL(), TargetFile: target}, Callbacks{}); err != nil {
		t.Fatalf("重新下载失败: %v", err)
	}
	// 必须是"新文件"，绝不能新旧内容拼在一起
	requireFileEquals(t, target, data2)
}

// 某条连接被限速到爬行时，看门狗应当把它重开，让下载恢复正常速度。
func TestSlowConnectionGetsReconnected(t *testing.T) {
	data := makeData(1<<20, 9)
	srv := testserver.New(data)
	defer srv.Close()
	// 从偏移 0 开始的请求只有 64 KB/s（模拟"某条连接被限速"）；
	// 从别的偏移开始是全速 —— 所以重开连接后应当立刻变快。
	srv.SetSlowAtZero(64 << 10)

	oldW, oldB, oldR := slowWindow, slowMinBytes, slowRemainingMin
	slowWindow, slowMinBytes, slowRemainingMin = 200*time.Millisecond, 64<<10, 0
	defer func() { slowWindow, slowMinBytes, slowRemainingMin = oldW, oldB, oldR }()

	cfg := testConfig(t)
	cfg.InitialThreads = 1
	cfg.MaxThreads = 1
	e := New(cfg)
	target := filepath.Join(t.TempDir(), "out.bin")

	start := time.Now()
	if _, err := e.Download(context.Background(), Request{URL: srv.URL(), TargetFile: target}, Callbacks{}); err != nil {
		t.Fatalf("下载失败: %v", err)
	}
	elapsed := time.Since(start)
	requireFileEquals(t, target, data)
	// 没有看门狗的话，1MB 以 64KB/s 下要 ~16 秒；这里应当在几秒内完成。
	if elapsed > 5*time.Second {
		t.Fatalf("慢连接没有被重开，用时 %v", elapsed)
	}
}

func TestAutoFileNameWhenTargetMissing(t *testing.T) {
	data := makeData(512<<10, 8)
	srv := testserver.New(data)
	defer srv.Close()

	e := New(testConfig(t))
	dir := t.TempDir()

	// 只给网址，不给文件名 —— 引擎应自己取名
	res, err := e.Download(context.Background(), Request{URL: srv.URL(), TargetDir: dir}, Callbacks{})
	if err != nil {
		t.Fatalf("下载失败: %v", err)
	}
	if filepath.Dir(res.Path) != dir {
		t.Fatalf("文件没有落到指定目录: %s", res.Path)
	}
	requireFileEquals(t, res.Path, data)
}
