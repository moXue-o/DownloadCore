package downloadcore

import "time"

// Config 是下载核心的全部可调参数。
// 将来做成 C ABI 时，它会对应一个配置结构体。
type Config struct {
	// InitialThreads 开局铺开的工人数（连接数）
	InitialThreads int
	// MaxThreads 动态分段最多能加到几个工人
	MaxThreads int
	// MinPartSize 最小分段大小；小于它就不再切
	MinPartSize int64
	// BufferSize 每个工人的读写缓冲大小
	BufferSize int
	// IdleTimeout 多久没数据进来就判定为卡住，重连
	IdleTimeout time.Duration
	// MaxRetries 每一段最多重试次数
	MaxRetries int
	// RetryDelay 每次重试前的等待
	RetryDelay time.Duration
	// TempDir 临时文件根目录
	TempDir string
	// IncompleteSuffix 未完成文件的标记后缀
	IncompleteSuffix string
	// UserAgent 默认 User-Agent
	UserAgent string
	// MaxSpeed 全局限速（字节/秒），0 表示不限
	MaxSpeed int64
}

// DefaultUserAgent 用中性的客户端身份。
//
// 实测：把 User-Agent 改成 Chrome 浏览器身份后，部分 CDN（如 dl.hdslb.com）
// 反而返回 403 —— 因为 Go 的 TLS 指纹和 Chrome 不一致，伪装浏览器会被反爬识别。
// 所以这里保持一个坦诚的、非浏览器的身份。
const DefaultUserAgent = "downloadcore/0.1"

// DefaultConfig 给出一套保守可用的默认参数。
func DefaultConfig() Config {
	return Config{
		InitialThreads:   32,
		MaxThreads:       32,
		MinPartSize:      1 << 20, // 1 MiB
		BufferSize:       256 << 10,
		IdleTimeout:      15 * time.Second,
		MaxRetries:       10,
		RetryDelay:       time.Second,
		TempDir:          ".download-temp",
		IncompleteSuffix: ".part",
		UserAgent:        DefaultUserAgent,
		MaxSpeed:         0,
	}
}
