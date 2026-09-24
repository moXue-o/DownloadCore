package downloadcore

import (
	"context"
	"errors"
	"fmt"
)

// ErrorKind 告诉调用方："这个错能不能自动重试"。
type ErrorKind int

const (
	// KindRetryable 可以自动重试（网络抖动、超时等）
	KindRetryable ErrorKind = iota
	// KindFatal 不可重试，需要停下来告诉宿主（磁盘没空间、服务器文件变了等）
	KindFatal
)

// Error 给底层错误贴上"可重试/致命"的标签。
type Error struct {
	Kind ErrorKind
	Op   string
	Err  error
}

func (e *Error) Error() string { return fmt.Sprintf("%s: %v", e.Op, e.Err) }
func (e *Error) Unwrap() error { return e.Err }

func retryable(op string, err error) error { return &Error{Kind: KindRetryable, Op: op, Err: err} }
func fatal(op string, err error) error     { return &Error{Kind: KindFatal, Op: op, Err: err} }

func kindOf(err error) ErrorKind {
	var e *Error
	if errors.As(err, &e) {
		return e.Kind
	}
	return KindFatal // 未知错误按致命处理，交给上层决定
}

// isRetryable 判断一个错误是否值得重试。用户取消/超时永不重试。
func isRetryable(err error) bool {
	if err == nil {
		return false
	}
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return false
	}
	return kindOf(err) == KindRetryable
}

// 一系列可识别的错误。
var (
	ErrRangeUnsupported = errors.New("服务器不支持分段下载")
	ErrRangeMismatch    = errors.New("服务器返回的分段与请求不一致")
	ErrFileChanged      = errors.New("服务器上的文件已改变")
	ErrNoSpace          = errors.New("磁盘空间不足")
	ErrTooManyFailures  = errors.New("重试次数用尽")
	ErrIdle             = errors.New("连接卡住（空闲超时）")
)
