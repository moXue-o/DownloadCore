package downloadcore

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"
)

// probeInfo 是"探路"的结果：文件多大、能不能分段、以及文件身份证。
type probeInfo struct {
	size         int64
	rangeOK      bool
	etag         string
	lastModified string
	fileName     string
}

// applyHeaders 把宿主给的请求头套上，并强制禁用压缩。
// 分段下载必须禁用压缩，否则字节范围会错位。
func applyHeaders(h *http.Request, req Request, cfg Config) {
	for k, v := range req.Headers {
		h.Header.Set(k, v)
	}
	if h.Header.Get("User-Agent") == "" {
		h.Header.Set("User-Agent", cfg.UserAgent)
	}
	h.Header.Set("Accept-Encoding", "identity")
}

// probe 先问服务器一句"能分段吗、文件多大"。
func (e *Engine) probe(ctx context.Context, req Request) (*probeInfo, error) {
	h, err := http.NewRequestWithContext(ctx, http.MethodGet, req.URL, nil)
	if err != nil {
		return nil, fatal("probe", err)
	}
	applyHeaders(h, req, e.cfg)
	h.Header.Set("Range", "bytes=0-0")

	resp, err := e.client.Do(h)
	if err != nil {
		return nil, retryable("probe", err)
	}
	defer resp.Body.Close()
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 1<<20))

	info := &probeInfo{
		etag:         resp.Header.Get("ETag"),
		lastModified: resp.Header.Get("Last-Modified"),
		fileName:     parseFileName(resp.Header.Get("Content-Disposition")),
	}

	switch {
	case resp.StatusCode == http.StatusPartialContent:
		start, _, total, ok := parseContentRange(resp.Header.Get("Content-Range"))
		if ok && start == 0 && total > 0 {
			info.rangeOK = true
			info.size = total
		} else if resp.ContentLength > 0 {
			info.rangeOK = false
			info.size = resp.ContentLength
		}
	case resp.StatusCode >= 200 && resp.StatusCode < 300:
		// 服务器没理我们的 Range，直接给了整文件 —— 不支持分段
		info.rangeOK = false
		info.size = resp.ContentLength
	default:
		return nil, fatal("probe", fmt.Errorf("服务器返回状态 %d", resp.StatusCode))
	}

	if strings.EqualFold(strings.TrimSpace(resp.Header.Get("Accept-Ranges")), "none") {
		info.rangeOK = false
	}
	return info, nil
}

// openRange 打开某一段 [from, to] 的连接，并在这里"对暗号"：
// 状态必须是 206，且返回的起点必须正好是我们要的 from。
func (e *Engine) openRange(ctx context.Context, req Request, from, to int64) (io.ReadCloser, error) {
	h, err := http.NewRequestWithContext(ctx, http.MethodGet, req.URL, nil)
	if err != nil {
		return nil, fatal("request", err)
	}
	applyHeaders(h, req, e.cfg)
	h.Header.Set("Range", fmt.Sprintf("bytes=%d-%d", from, to))

	resp, err := e.client.Do(h)
	if err != nil {
		return nil, retryable("request", err)
	}
	if resp.StatusCode != http.StatusPartialContent {
		resp.Body.Close()
		return nil, retryable("range", fmt.Errorf("%w: 期望 206，实际 %d", ErrRangeMismatch, resp.StatusCode))
	}
	start, _, _, ok := parseContentRange(resp.Header.Get("Content-Range"))
	if !ok {
		resp.Body.Close()
		return nil, retryable("range", fmt.Errorf("%w: Content-Range 缺失", ErrRangeMismatch))
	}
	if start != from {
		resp.Body.Close()
		return nil, retryable("range", fmt.Errorf("%w: 期望起点 %d，服务器给了 %d", ErrRangeMismatch, from, start))
	}
	return newIdleBody(resp.Body, e.cfg.IdleTimeout), nil
}

// parseContentRange 解析 "bytes start-end/total"。
func parseContentRange(v string) (start, end, total int64, ok bool) {
	v = strings.TrimSpace(v)
	if len(v) < 5 || !strings.EqualFold(v[:5], "bytes") {
		return 0, 0, 0, false
	}
	v = strings.TrimSpace(v[5:])
	slash := strings.IndexByte(v, '/')
	if slash < 0 {
		return 0, 0, 0, false
	}
	rangePart := strings.TrimSpace(v[:slash])
	totalPart := strings.TrimSpace(v[slash+1:])
	dash := strings.IndexByte(rangePart, '-')
	if dash < 0 {
		return 0, 0, 0, false
	}
	s, err1 := strconv.ParseInt(strings.TrimSpace(rangePart[:dash]), 10, 64)
	e, err2 := strconv.ParseInt(strings.TrimSpace(rangePart[dash+1:]), 10, 64)
	if err1 != nil || err2 != nil {
		return 0, 0, 0, false
	}
	if totalPart == "*" {
		total = -1
	} else {
		t, err3 := strconv.ParseInt(totalPart, 10, 64)
		if err3 != nil {
			return 0, 0, 0, false
		}
		total = t
	}
	return s, e, total, true
}

// parseFileName 从 Content-Disposition 里挖文件名（支持 filename* 和 filename）。
func parseFileName(cd string) string {
	if cd == "" {
		return ""
	}
	lower := strings.ToLower(cd)
	// 优先 filename*=UTF-8''xxx
	if idx := strings.Index(lower, "filename*="); idx >= 0 {
		rest := cd[idx+len("filename*="):]
		if semi := strings.IndexByte(rest, ';'); semi >= 0 {
			rest = rest[:semi]
		}
		rest = strings.TrimSpace(rest)
		// 形如 UTF-8''%E4%B8%AD
		if parts := strings.SplitN(rest, "''", 2); len(parts) == 2 {
			return urlUnescape(parts[1])
		}
	}
	if idx := strings.Index(lower, "filename="); idx >= 0 {
		rest := cd[idx+len("filename="):]
		if semi := strings.IndexByte(rest, ';'); semi >= 0 {
			rest = rest[:semi]
		}
		rest = strings.TrimSpace(rest)
		rest = strings.Trim(rest, `"`)
		return rest
	}
	return ""
}

func urlUnescape(s string) string {
	// 用一个极小的百分号解码，避免引入额外依赖
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		if s[i] == '%' && i+2 < len(s) {
			hi := unhex(s[i+1])
			lo := unhex(s[i+2])
			if hi >= 0 && lo >= 0 {
				b.WriteByte(byte(hi<<4 | lo))
				i += 2
				continue
			}
		}
		b.WriteByte(s[i])
	}
	return b.String()
}

func unhex(c byte) int {
	switch {
	case c >= '0' && c <= '9':
		return int(c - '0')
	case c >= 'a' && c <= 'f':
		return int(c-'a') + 10
	case c >= 'A' && c <= 'F':
		return int(c-'A') + 10
	}
	return -1
}

// idleBody 在"一段时间没有数据进来"时主动掐断连接，避免无限期挂着。
type idleBody struct {
	rc      io.ReadCloser
	timeout time.Duration
	timer   *time.Timer
	mu      sync.Mutex
	fired   bool
}

func newIdleBody(rc io.ReadCloser, timeout time.Duration) *idleBody {
	b := &idleBody{rc: rc, timeout: timeout}
	if timeout > 0 {
		b.timer = time.AfterFunc(timeout, b.onIdle)
	}
	return b
}

func (b *idleBody) onIdle() {
	b.mu.Lock()
	b.fired = true
	b.mu.Unlock()
	_ = b.rc.Close()
}

func (b *idleBody) Read(p []byte) (int, error) {
	n, err := b.rc.Read(p)
	b.mu.Lock()
	fired := b.fired
	if n > 0 && !fired && b.timer != nil {
		b.timer.Reset(b.timeout)
	}
	b.mu.Unlock()
	if err != nil && fired {
		return n, ErrIdle
	}
	return n, err
}

func (b *idleBody) Close() error {
	b.mu.Lock()
	b.fired = true
	b.mu.Unlock()
	if b.timer != nil {
		b.timer.Stop()
	}
	return b.rc.Close()
}
