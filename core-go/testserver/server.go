// Package testserver 提供一个"可以随便摆布"的测试下载服务器，
// 用来验证引擎在各种恶劣情况下的行为（不支持分段、故意报错、限速、卡住、换文件）。
package testserver

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"time"
)

type Server struct {
	srv *httptest.Server

	mu          sync.Mutex
	data        []byte
	etag        string
	noRange     bool
	failRange   bool
	bytesPerSec int64 // 0 表示全速
	slowAtZero  int64 // 起始偏移为 0 的请求使用的速度（0 表示不特殊处理）
	stallAfter  int64 // 发这么多字节后卡住；0 表示不卡
}

func New(data []byte) *Server {
	s := &Server{data: data, etag: `"v1"`}
	s.srv = httptest.NewServer(http.HandlerFunc(s.handle))
	return s
}

func (s *Server) URL() string { return s.srv.URL }
func (s *Server) Close()      { s.srv.Close() }

func (s *Server) SetData(data []byte, etag string) {
	s.mu.Lock()
	s.data = data
	s.etag = etag
	s.mu.Unlock()
}

func (s *Server) SetNoRange(v bool) {
	s.mu.Lock()
	s.noRange = v
	s.mu.Unlock()
}

func (s *Server) SetFailRange(v bool) {
	s.mu.Lock()
	s.failRange = v
	s.mu.Unlock()
}

func (s *Server) SetSpeed(bps int64) {
	s.mu.Lock()
	s.bytesPerSec = bps
	s.mu.Unlock()
}

// SetSlowAtZero 让"从偏移 0 开始"的请求变慢，用来模拟"某条连接被限速"。
func (s *Server) SetSlowAtZero(bps int64) {
	s.mu.Lock()
	s.slowAtZero = bps
	s.mu.Unlock()
}

func (s *Server) SetStallAfter(n int64) {
	s.mu.Lock()
	s.stallAfter = n
	s.mu.Unlock()
}

func (s *Server) handle(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	data := s.data
	etag := s.etag
	noRange := s.noRange
	failRange := s.failRange
	bps := s.bytesPerSec
	slowAtZero := s.slowAtZero
	stallAfter := s.stallAfter
	s.mu.Unlock()

	w.Header().Set("ETag", etag)
	w.Header().Set("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT")

	rangeHeader := r.Header.Get("Range")
	if noRange || rangeHeader == "" {
		w.Header().Set("Accept-Ranges", "none")
		w.Header().Set("Content-Length", strconv.Itoa(len(data)))
		w.WriteHeader(http.StatusOK)
		s.writeBody(r.Context(), w, data, bps, stallAfter)
		return
	}

	start, end, ok := parseRange(rangeHeader, int64(len(data)))
	if !ok {
		w.WriteHeader(http.StatusRequestedRangeNotSatisfiable)
		return
	}
	w.Header().Set("Accept-Ranges", "bytes")
	// failRange：只让"多字节请求"故意把起点报错一位，放过 1 字节的探路请求，
	// 这样探路能成功、真正下分段时才暴露问题。
	reportStart := start
	if failRange && end-start+1 > 1 {
		reportStart = start + 1
	}
	w.Header().Set("Content-Range", fmt.Sprintf("bytes %d-%d/%d", reportStart, end, len(data)))
	w.Header().Set("Content-Length", strconv.FormatInt(end-start+1, 10))
	w.WriteHeader(http.StatusPartialContent)
	reqBps := bps
	if slowAtZero > 0 && start == 0 {
		reqBps = slowAtZero
	}
	s.writeBody(r.Context(), w, data[start:end+1], reqBps, stallAfter)
}

func (s *Server) writeBody(ctx context.Context, w http.ResponseWriter, data []byte, bps int64, stallAfter int64) {
	flusher, _ := w.(http.Flusher)
	const chunk = 32 << 10
	var written int64
	for off := 0; off < len(data); off += chunk {
		if stallAfter > 0 && written >= stallAfter {
			// 装死：直到客户端断开或超时
			select {
			case <-ctx.Done():
			case <-time.After(10 * time.Second):
			}
			return
		}
		end := off + chunk
		if end > len(data) {
			end = len(data)
		}
		n, err := w.Write(data[off:end])
		if err != nil {
			return
		}
		written += int64(n)
		if flusher != nil {
			flusher.Flush()
		}
		if bps > 0 {
			time.Sleep(time.Duration(float64(n) / float64(bps) * float64(time.Second)))
		}
	}
}

func parseRange(h string, size int64) (int64, int64, bool) {
	if !strings.HasPrefix(h, "bytes=") {
		return 0, 0, false
	}
	spec := strings.TrimPrefix(h, "bytes=")
	dash := strings.IndexByte(spec, '-')
	if dash < 0 {
		return 0, 0, false
	}
	left := strings.TrimSpace(spec[:dash])
	right := strings.TrimSpace(spec[dash+1:])

	var start, end int64
	if left == "" {
		n, err := strconv.ParseInt(right, 10, 64)
		if err != nil {
			return 0, 0, false
		}
		start = size - n
		if start < 0 {
			start = 0
		}
		end = size - 1
	} else {
		v, err := strconv.ParseInt(left, 10, 64)
		if err != nil {
			return 0, 0, false
		}
		start = v
		if right == "" {
			end = size - 1
		} else {
			w, err := strconv.ParseInt(right, 10, 64)
			if err != nil {
				return 0, 0, false
			}
			end = w
		}
	}
	if start < 0 {
		start = 0
	}
	if end >= size {
		end = size - 1
	}
	if start > end {
		return 0, 0, false
	}
	return start, end, true
}
