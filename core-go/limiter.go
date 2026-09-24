package downloadcore

import (
	"context"
	"sync"
	"time"
)

// limiter 是一个简单的令牌桶全局限速器，0 表示不限速。
type limiter struct {
	mu       sync.Mutex
	maxSpeed int64
	burst    float64
	tokens   float64
	last     time.Time
}

func newLimiter(maxSpeed int64) *limiter {
	const burst = 1 << 20 // 1 MiB
	return &limiter{
		maxSpeed: maxSpeed,
		burst:    burst,
		tokens:   burst,
		last:     time.Now(),
	}
}

// wait 阻塞直到允许读取 n 字节，或被 ctx 取消。
func (l *limiter) wait(ctx context.Context, n int64) error {
	if l == nil || l.maxSpeed <= 0 {
		return nil
	}
	for {
		l.mu.Lock()
		now := time.Now()
		elapsed := now.Sub(l.last).Seconds()
		l.last = now
		l.tokens += float64(l.maxSpeed) * elapsed
		if l.tokens > l.burst {
			l.tokens = l.burst
		}
		if l.tokens >= float64(n) {
			l.tokens -= float64(n)
			l.mu.Unlock()
			return nil
		}
		need := (float64(n) - l.tokens) / float64(l.maxSpeed)
		l.mu.Unlock()

		d := time.Duration(need * float64(time.Second))
		if d < time.Millisecond {
			d = time.Millisecond
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(d):
		}
	}
}
