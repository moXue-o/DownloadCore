package downloadcore

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"time"
)

// Engine 是下载核心。
// 它不含任何界面、队列、通知逻辑，只负责把一个 URL 下成一个文件。
type Engine struct {
	cfg     Config
	client  *http.Client
	limiter *limiter
}

// New 创建引擎，并把明显不合理的参数拉回可用范围。
func New(cfg Config) *Engine {
	if cfg.InitialThreads <= 0 {
		cfg.InitialThreads = 1
	}
	if cfg.MaxThreads < cfg.InitialThreads {
		cfg.MaxThreads = cfg.InitialThreads
	}
	if cfg.BufferSize <= 0 {
		cfg.BufferSize = 256 << 10
	}
	if cfg.MinPartSize <= 0 {
		cfg.MinPartSize = 1 << 20
	}
	if cfg.IdleTimeout <= 0 {
		cfg.IdleTimeout = 15 * time.Second
	}
	if cfg.MaxRetries < 0 {
		cfg.MaxRetries = 0
	}
	if cfg.RetryDelay <= 0 {
		cfg.RetryDelay = time.Second
	}
	if cfg.TempDir == "" {
		cfg.TempDir = ".download-temp"
	}
	if cfg.IncompleteSuffix == "" {
		cfg.IncompleteSuffix = ".part"
	}
	if cfg.UserAgent == "" {
		cfg.UserAgent = DefaultUserAgent
	}
	tr := &http.Transport{
		Proxy:               http.ProxyFromEnvironment,
		DialContext:         (&net.Dialer{Timeout: 30 * time.Second, KeepAlive: 30 * time.Second}).DialContext,
		ForceAttemptHTTP2:   false, // 用 HTTP/1.1 多连接，范围下载更可控
		MaxIdleConns:        100,
		MaxIdleConnsPerHost: 32,
		IdleConnTimeout:     90 * time.Second,
		TLSHandshakeTimeout: 15 * time.Second,
		DisableCompression:  true,
	}
	return &Engine{
		cfg: cfg,
		client: &http.Client{
			Transport: tr,
		},
		limiter: newLimiter(cfg.MaxSpeed),
	}
}

// Download 下载一个文件。
// ctx 取消即中止（已下部分保留在临时目录，供下次续传）。
func (e *Engine) Download(ctx context.Context, req Request, cbs Callbacks) (*Result, error) {
	if req.URL == "" {
		return nil, errors.New("URL 为空")
	}
	return newJob(e, req, cbs).run(ctx)
}

type job struct {
	eng *Engine
	cfg Config
	req Request
	cbs Callbacks

	key     string
	tempDir string
	final   string
	marker  string

	mu       sync.Mutex
	parts    []*part
	queue    []*part
	active   int
	firstErr error

	downloaded atomic.Int64

	total   int64
	rangeOK bool
	etag    string
	lastMod string

	progMu        sync.Mutex
	lastEmit      time.Time
	lastEmitBytes int64

	startTime time.Time
	cancel    context.CancelFunc
	done      chan struct{}

	logMu sync.Mutex
}

func newJob(e *Engine, req Request, cbs Callbacks) *job {
	return &job{
		eng:  e,
		cfg:  e.cfg,
		req:  req,
		cbs:  cbs,
		done: make(chan struct{}, 1),
	}
}

// setupPaths 在探路之后确定最终路径。
// 如果宿主没给文件名，就从服务器响应（Content-Disposition）或 URL 里推一个。
func (j *job) setupPaths(pi *probeInfo) error {
	final := j.req.TargetFile
	if final == "" {
		name := pi.fileName
		if name == "" {
			name = fileNameFromURL(j.req.URL)
		}
		if name == "" {
			name = "download.bin"
		}
		dir := j.req.TargetDir
		if dir == "" {
			dir = "."
		}
		final = filepath.Join(dir, sanitizeName(name))
	}

	abs := final
	if a, err := filepath.Abs(final); err == nil {
		abs = a
	}
	j.final = final
	j.key = jobKey(abs)
	j.tempDir = filepath.Join(j.cfg.TempDir, j.key)
	j.marker = final + j.cfg.IncompleteSuffix

	if err := os.MkdirAll(filepath.Dir(final), 0755); err != nil {
		return fatal("mkdir", err)
	}
	return nil
}

func (j *job) emitStatus(s Status) {
	if j.cbs.OnStatus != nil {
		j.cbs.OnStatus(s)
	}
}

// logf 输出一条日志。多个工人会并发调用，所以加了锁。
func (j *job) logf(level, format string, args ...any) {
	if j.cbs.OnLog == nil {
		return
	}
	entry := LogEntry{Level: level, Message: fmt.Sprintf(format, args...)}
	j.logMu.Lock()
	defer j.logMu.Unlock()
	j.cbs.OnLog(entry)
}

func (j *job) addDownloaded(n int64) {
	total := j.downloaded.Add(n)
	if j.cbs.OnProgress == nil {
		return
	}
	now := time.Now()
	j.progMu.Lock()
	if now.Sub(j.lastEmit) < 200*time.Millisecond {
		j.progMu.Unlock()
		return
	}
	dt := now.Sub(j.lastEmit).Seconds()
	delta := total - j.lastEmitBytes
	j.lastEmit = now
	j.lastEmitBytes = total
	j.progMu.Unlock()

	// 引擎只提供"原始累计字节 + 一个简单的瞬时速度"。
	// 如何平滑、怎么显示，交给宿主程序决定。
	speed := int64(0)
	if dt > 0 {
		speed = int64(float64(delta) / dt)
	}

	j.mu.Lock()
	parts := len(j.parts)
	j.mu.Unlock()
	j.cbs.OnProgress(Progress{Downloaded: total, Total: j.total, Speed: speed, Parts: parts})
}

func (j *job) run(ctx context.Context) (*Result, error) {
	ctx, cancel := context.WithCancel(ctx)
	j.cancel = cancel
	defer cancel()

	j.startTime = time.Now()
	j.progMu.Lock()
	j.lastEmit = time.Now()
	j.progMu.Unlock()

	j.logf("INFO", "开始任务：%s", j.req.URL)

	j.emitStatus(StatusProbing)
	pi, err := j.eng.probe(ctx, j.req)
	if err != nil {
		j.logf("ERROR", "探路失败：%v", err)
		j.emitStatus(StatusFailed)
		return nil, err
	}
	if pi.size < 0 {
		pi.size = 0
	}
	if err := j.setupPaths(pi); err != nil {
		j.logf("ERROR", "确定保存路径失败：%v", err)
		j.emitStatus(StatusFailed)
		return nil, err
	}
	j.total = pi.size
	j.rangeOK = pi.rangeOK
	j.etag = pi.etag
	j.lastMod = pi.lastModified

	j.logf("INFO", "探路完成：大小=%d 字节，支持分段=%v，ETag=%q，最后修改=%q",
		pi.size, pi.rangeOK, pi.etag, pi.lastModified)
	j.logf("INFO", "最终文件：%s", j.final)
	j.logf("DEBUG", "临时目录：%s", j.tempDir)

	// 服务器不支持分段（或大小未知）：退化为单线程整文件下载
	if !pi.rangeOK || pi.size <= 0 {
		j.logf("INFO", "模式：单线程（服务器不支持分段或大小未知）")
		j.emitStatus(StatusDownloading)
		if err := j.downloadWhole(ctx); err != nil {
			j.logf("ERROR", "下载失败：%v", err)
			j.emitStatus(StatusFailed)
			return nil, err
		}
		if err := j.moveIntoPlace(j.marker); err != nil {
			j.logf("ERROR", "改名失败：%v", err)
			j.emitStatus(StatusFailed)
			return nil, err
		}
		j.logf("INFO", "下载完成：%s，共 %d 字节", j.final, j.downloaded.Load())
		j.emitStatus(StatusCompleted)
		return j.result(0), nil
	}

	j.logf("INFO", "模式：分段下载（开局 %d 路，最多 %d 路，最小段 %d 字节）",
		j.cfg.InitialThreads, j.cfg.MaxThreads, j.cfg.MinPartSize)

	// 分段模式
	if err := j.prepareParts(pi); err != nil {
		j.logf("ERROR", "准备分段失败：%v", err)
		j.emitStatus(StatusFailed)
		return nil, err
	}

	stopTicker := j.startStateTicker(ctx)

	j.emitStatus(StatusDownloading)

	j.mu.Lock()
	j.pumpLocked(ctx)
	done := j.active == 0 && len(j.queue) == 0
	j.mu.Unlock()

	for !done {
		select {
		case <-ctx.Done():
		case <-j.done:
		}
		j.mu.Lock()
		if j.firstErr != nil {
			err := j.firstErr
			j.mu.Unlock()
			cancel()
			stopTicker()
			j.saveState()
			j.logf("ERROR", "任务失败：%v", err)
			j.emitStatus(StatusFailed)
			return nil, err
		}
		j.pumpLocked(ctx)
		done = j.active == 0 && len(j.queue) == 0
		j.mu.Unlock()
	}

	if err := ctx.Err(); err != nil {
		stopTicker()
		j.saveState() // 保留进度，供下次续传
		j.logf("WARN", "任务被取消，已下进度已保留，可稍后续传")
		return nil, err
	}
	j.mu.Lock()
	fe := j.firstErr
	j.mu.Unlock()
	if fe != nil {
		stopTicker()
		j.saveState()
		j.logf("ERROR", "任务失败：%v", fe)
		return nil, fe
	}

	// 全部下完 → 拼装
	stopTicker()
	j.mu.Lock()
	nSegments := len(j.parts)
	j.mu.Unlock()
	j.logf("INFO", "所有分段下载完毕，开始拼装 %d 段 → %s", nSegments, j.final)
	j.emitStatus(StatusAssembling)
	if err := j.assemble(); err != nil {
		j.logf("ERROR", "拼装失败：%v", err)
		j.emitStatus(StatusFailed)
		return nil, err
	}
	_ = os.RemoveAll(j.tempDir)
	j.emitStatus(StatusCompleted)

	elapsed := time.Since(j.startTime).Seconds()
	avg := int64(0)
	if elapsed > 0 {
		avg = int64(float64(j.downloaded.Load()) / elapsed)
	}
	j.logf("INFO", "下载完成：%s，共 %d 字节，用时 %.2f 秒，平均 %.2f MB/s，分段 %d",
		j.final, j.downloaded.Load(), elapsed, float64(avg)/1024/1024, nSegments)

	if j.cbs.OnProgress != nil {
		j.cbs.OnProgress(Progress{Downloaded: j.total, Total: j.total, Parts: nSegments})
	}
	return j.result(nSegments), nil
}

// prepareParts 决定这一轮要下哪些段：能续传就续传，否则全新切分。
func (j *job) prepareParts(pi *probeInfo) error {
	if err := os.MkdirAll(j.tempDir, 0755); err != nil {
		return fatal("mkdir", err)
	}

	if st, err := loadStateFile(filepath.Join(j.tempDir, stateFileName)); err == nil && st != nil {
		if st.Version == 1 && st.URL == j.req.URL && st.Total == pi.size &&
			st.ETag == pi.etag && st.LastModified == pi.lastModified && len(st.Parts) > 0 &&
			j.partFilesUsable(st.Parts) {
			for _, ps := range st.Parts {
				if ps.From < 0 || ps.To >= pi.size || ps.Current < ps.From {
					continue
				}
				p := newPartFrom(ps.From, ps.To, min64(ps.Current, ps.To+1))
				j.parts = append(j.parts, p)
				if !p.done() {
					j.queue = append(j.queue, p)
				}
			}
			if len(j.parts) > 0 {
				j.logf("INFO", "发现可续传记录：共 %d 段，继续下载未完成的部分", len(j.parts))
				return nil
			}
		} else if st.Total != pi.size || st.ETag != pi.etag || st.LastModified != pi.lastModified {
			j.logf("WARN", "续传记录与服务器对不上（文件可能已变化），改为重新下载")
		}
	}

	// 续传信息不存在或不匹配（例如服务器上的文件变了）→ 全新开始
	_ = os.RemoveAll(j.tempDir)
	if err := os.MkdirAll(j.tempDir, 0755); err != nil {
		return fatal("mkdir", err)
	}

	var ranges [][2]int64
	if pi.size > j.cfg.MinPartSize {
		ranges = splitToRange(pi.size, j.cfg.MinPartSize, j.cfg.InitialThreads)
	} else {
		ranges = [][2]int64{{0, pi.size - 1}}
	}
	for _, r := range ranges {
		p := newPart(r[0], r[1])
		j.parts = append(j.parts, p)
		j.queue = append(j.queue, p)
	}
	j.logf("INFO", "全新开始：切成 %d 段", len(ranges))
	return nil
}

// partFilesUsable 校验续传记录里的临时文件是否还在、是否够长。
// 防止"记录还在、数据文件已被删"时盲目续传，产出坏文件。
func (j *job) partFilesUsable(states []partState) bool {
	for _, ps := range states {
		need := ps.Current - ps.From
		if ps.Current > ps.To {
			need = ps.To - ps.From + 1
		}
		if need <= 0 {
			continue
		}
		fi, err := os.Stat(partFileName(j.tempDir, ps.From))
		if err != nil || fi.Size() < need {
			return false
		}
	}
	return true
}

// pumpLocked 用空闲名额补工人：先派排队中的段，没有排队段就尝试分裂出一个新段。
func (j *job) pumpLocked(ctx context.Context) {
	for j.active < j.cfg.MaxThreads {
		if len(j.queue) > 0 {
			p := j.queue[0]
			j.queue = j.queue[1:]
			j.startLocked(ctx, p)
			continue
		}
		np := j.splitOneLocked()
		if np == nil {
			return
		}
		j.startLocked(ctx, np)
	}
}

// splitOneLocked 从所有段里挑"剩下活最多"的那段来分裂，保证负载均衡。
// 旧做法是"切第一段"，会把最左边那段越切越碎，而大段无人分担 —— 这正是速度上不去的主因。
func (j *job) splitOneLocked() *part {
	minDelta := j.splitMinDelta()
	var best *part
	var bestDelta int64
	for _, p := range j.parts {
		d := p.splittableDelta()
		if d >= minDelta && d > bestDelta {
			bestDelta = d
			best = p
		}
	}
	if best == nil {
		return nil
	}
	np := best.splitAtLeast(minDelta)
	if np == nil {
		return nil
	}
	j.parts = append(j.parts, np)
	j.logf("INFO", "动态分段：拆分最大段，新增 [%d, %d]（原段剩到 %d）", np.from, np.to, np.from-1)
	return np
}

// splitMinDelta 是允许动态分裂的最小"未认领区域"。
// 允许切到约 safetyStep（默认 1 MiB），这样尾段也能被切成多份、
// 保持较多连接同时收尾 —— 服务器按连接限速时，连接越多收得越快。
func (j *job) splitMinDelta() int64 {
	m := j.cfg.MinPartSize
	if m < safetyStep {
		m = safetyStep
	}
	return m
}

func (j *job) startLocked(ctx context.Context, p *part) {
	j.active++
	from, to, _, _ := p.snapshot()
	j.logf("DEBUG", "工人启动：段 [%d, %d]", from, to)
	go func() {
		err := j.runPart(ctx, p)
		j.mu.Lock()
		if err != nil && !errors.Is(err, context.Canceled) && !errors.Is(err, context.DeadlineExceeded) {
			if j.firstErr == nil {
				j.firstErr = err
			}
		}
		j.active--
		j.mu.Unlock()
		if err != nil {
			j.logf("WARN", "工人结束（出错）：段 [%d, %d]，错误=%v", from, to, err)
			j.cancel()
		} else {
			j.logf("DEBUG", "工人结束（完成）：段 [%d, %d]", from, to)
		}
		select {
		case j.done <- struct{}{}:
		default:
		}
	}()
}

// runPart 负责一个段的"带重试下载"。
func (j *job) runPart(ctx context.Context, p *part) error {
	var lastErr error
	for attempt := 0; attempt <= j.cfg.MaxRetries; attempt++ {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if attempt > 0 {
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(j.cfg.RetryDelay):
			}
		}
		err := j.downloadPartOnce(ctx, p)
		if err == nil {
			if p.done() {
				return nil
			}
			lastErr = retryable("part", io.ErrUnexpectedEOF)
			continue
		}
		if !isRetryable(err) {
			return err
		}
		from, to, cur, _ := p.snapshot()
		j.logf("WARN", "本段下载出错，准备重试（第 %d 次）：段 [%d, %d]，已到 %d，错误=%v",
			attempt+1, from, to, cur, err)
		lastErr = err
	}
	return fmt.Errorf("%w: %v", ErrTooManyFailures, lastErr)
}

// 慢连接看门狗：连接在 slowWindow 内下的数据少于 slowMinBytes，
// 且该段还剩超过 slowRemainingMin 没下时，判定为"过慢"，重开连接。
var (
	slowWindow        = 5 * time.Second
	slowMinBytes      = int64(512 << 10) // 5 秒不足 512 KiB ≈ 低于 100 KiB/s
	slowRemainingMin  = int64(256 << 10) // 还剩这么多就有必要折腾（尾巴很小也管）
	maxSlowReconnects = 8                // 最多主动重开这么多次，避免整体网络慢时来回折腾
)

var errSlowConn = errors.New("连接过慢")

// downloadPartOnce 把这一段下完；遇到"过慢"的连接会自动重开（不动用重试预算）。
func (j *job) downloadPartOnce(ctx context.Context, p *part) error {
	from, to, current, _ := p.snapshot()
	if current > to {
		return nil
	}

	f, err := j.openPartFile(p)
	if err != nil {
		return err
	}
	defer f.Close()

	buf := make([]byte, j.cfg.BufferSize)
	for reconnects := 0; ; {
		_, to, current, _ = p.snapshot()
		if current > to {
			return nil
		}
		body, err := j.eng.openRange(ctx, j.req, current, to)
		if err != nil {
			j.logf("WARN", "打开分段连接失败：段 [%d, %d]，从 %d 开始，错误=%v", from, to, current, err)
			return err
		}
		watch := reconnects < maxSlowReconnects
		err = j.pumpBody(ctx, p, f, body, from, buf, watch)
		body.Close()
		if err == nil {
			return nil
		}
		if errors.Is(err, errSlowConn) {
			reconnects++
			continue
		}
		return err
	}
}

// pumpBody 从连接里读数据写进临时文件，直到这段下完、出错、或被判定为过慢。
func (j *job) pumpBody(ctx context.Context, p *part, f *os.File, body io.ReadCloser, from int64, buf []byte, watchSlow bool) error {
	windowStart := time.Now()
	var windowBytes int64
	for {
		if err := ctx.Err(); err != nil {
			return err
		}
		_, end, cur, sz := p.snapshot()
		if cur > end {
			return nil
		}
		allowed := sz - cur + 1
		if allowed <= 0 {
			if !p.extendSafeZone() {
				if cur > end {
					return nil
				}
				return retryable("part", errors.New("无法推进安全区"))
			}
			continue
		}
		want := allowed
		if want > int64(len(buf)) {
			want = int64(len(buf))
		}
		n, rerr := body.Read(buf[:want])
		if n > 0 {
			if _, werr := f.WriteAt(buf[:n], cur-from); werr != nil {
				return fatal("write", werr)
			}
			p.advance(int64(n))
			j.addDownloaded(int64(n))
			windowBytes += int64(n)
			if err := j.eng.limiter.wait(ctx, int64(n)); err != nil {
				return err
			}
		}
		if rerr != nil {
			if rerr == io.EOF {
				if p.done() {
					return nil
				}
				j.logf("WARN", "连接提前结束：段 [%d, %d] 尚未下完", from, end)
				return retryable("read", io.ErrUnexpectedEOF)
			}
			if errors.Is(rerr, ErrIdle) {
				j.logf("WARN", "连接卡住（空闲超时）：段 [%d, %d]", from, end)
			}
			return retryable("read", rerr)
		}
		if watchSlow {
			elapsed := time.Since(windowStart)
			if elapsed >= slowWindow {
				_, end2, cur2, _ := p.snapshot()
				remaining := end2 - cur2 + 1
				if remaining > slowRemainingMin && windowBytes < slowMinBytes {
					j.logf("WARN", "连接过慢（%.0f 秒仅下 %d KB，还剩 %d KB），重开连接：段 [%d, %d]",
						elapsed.Seconds(), windowBytes/1024, remaining/1024, from, end2)
					return errSlowConn
				}
				windowStart = time.Now()
				windowBytes = 0
			}
		}
	}
}

// openPartFile 打开这一段的临时文件；首次创建时按段长预分配，尽早暴露"磁盘没空间"。
func (j *job) openPartFile(p *part) (*os.File, error) {
	from, to, _, _ := p.snapshot()
	path := partFileName(j.tempDir, from)
	fresh := false
	if _, err := os.Stat(path); os.IsNotExist(err) {
		fresh = true
	}
	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0644)
	if err != nil {
		return nil, fatal("open", err)
	}
	if fresh {
		if length := to - from + 1; length > 0 {
			if err := f.Truncate(length); err != nil {
				f.Close()
				return nil, fatal("preallocate", err)
			}
		}
	}
	return f, nil
}

// downloadWhole 是"服务器不支持分段"时的兜底：整文件顺序下载，失败从头再来。
func (j *job) downloadWhole(ctx context.Context) error {
	var lastErr error
	for attempt := 0; attempt <= j.cfg.MaxRetries; attempt++ {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if attempt > 0 {
			j.logf("WARN", "单线程重试（第 %d 次）：%v", attempt, lastErr)
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(j.cfg.RetryDelay):
			}
		}
		err := j.downloadWholeOnce(ctx)
		if err == nil {
			return nil
		}
		if !isRetryable(err) {
			return err
		}
		lastErr = err
	}
	return fmt.Errorf("%w: %v", ErrTooManyFailures, lastErr)
}

func (j *job) downloadWholeOnce(ctx context.Context) error {
	h, err := http.NewRequestWithContext(ctx, http.MethodGet, j.req.URL, nil)
	if err != nil {
		return fatal("request", err)
	}
	applyHeaders(h, j.req, j.cfg)

	resp, err := j.eng.client.Do(h)
	if err != nil {
		return retryable("request", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return retryable("whole", fmt.Errorf("服务器返回状态 %d", resp.StatusCode))
	}
	if j.total <= 0 && resp.ContentLength > 0 {
		j.total = resp.ContentLength
	}

	f, err := os.Create(j.marker)
	if err != nil {
		return fatal("create", err)
	}
	defer f.Close()

	j.downloaded.Store(0)

	buf := make([]byte, j.cfg.BufferSize)
	body := newIdleBody(resp.Body, j.cfg.IdleTimeout)
	defer body.Close()
	for {
		n, rerr := body.Read(buf)
		if n > 0 {
			if _, werr := f.Write(buf[:n]); werr != nil {
				return fatal("write", werr)
			}
			j.addDownloaded(int64(n))
			if err := j.eng.limiter.wait(ctx, int64(n)); err != nil {
				return err
			}
		}
		if rerr != nil {
			if rerr == io.EOF {
				return nil
			}
			if errors.Is(rerr, ErrIdle) {
				j.logf("WARN", "连接卡住（空闲超时），将重试")
			}
			return retryable("read", rerr)
		}
	}
}

// startStateTicker 周期性保存续传记录，供下次续传使用。
func (j *job) startStateTicker(ctx context.Context) func() {
	t := time.NewTicker(time.Second)
	stop := make(chan struct{})
	go func() {
		for {
			select {
			case <-ctx.Done():
				return
			case <-stop:
				return
			case <-t.C:
				j.saveState()
			}
		}
	}()
	var once sync.Once
	return func() {
		once.Do(func() {
			t.Stop()
			close(stop)
		})
	}
}

// saveState 原子地保存当前所有分段进度。
func (j *job) saveState() {
	j.mu.Lock()
	st := &resumeState{
		Version:      1,
		URL:          j.req.URL,
		Total:        j.total,
		ETag:         j.etag,
		LastModified: j.lastMod,
	}
	for _, p := range j.parts {
		from, to, cur, _ := p.snapshot()
		if cur > to {
			cur = to + 1
		}
		st.Parts = append(st.Parts, partState{From: from, To: to, Current: cur})
	}
	j.mu.Unlock()
	if err := saveStateFile(filepath.Join(j.tempDir, stateFileName), st); err != nil {
		j.logf("WARN", "保存续传记录失败：%v", err)
	}
}

func (j *job) result(parts int) *Result {
	elapsed := time.Since(j.startTime).Seconds()
	size := j.downloaded.Load()
	if j.rangeOK && j.total > 0 {
		size = j.total
	}
	speed := int64(0)
	if elapsed > 0 {
		speed = int64(float64(size) / elapsed)
	}
	return &Result{
		Path:    j.final,
		Size:    size,
		Speed:   speed,
		Parts:   parts,
		RangeOK: j.rangeOK,
	}
}
