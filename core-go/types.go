package downloadcore

// Status 是下载任务的状态。
type Status int

const (
	StatusPending Status = iota
	StatusProbing
	StatusDownloading
	StatusAssembling
	StatusCompleted
	StatusFailed
	StatusCanceled
)

func (s Status) String() string {
	switch s {
	case StatusPending:
		return "pending"
	case StatusProbing:
		return "probing"
	case StatusDownloading:
		return "downloading"
	case StatusAssembling:
		return "assembling"
	case StatusCompleted:
		return "completed"
	case StatusFailed:
		return "failed"
	case StatusCanceled:
		return "canceled"
	default:
		return "unknown"
	}
}

// Progress 是一次进度回调的快照。
type Progress struct {
	Downloaded int64 // 已下载字节
	Total      int64 // 总大小；未知时为 0
	Speed      int64 // 瞬时速度（字节/秒）
	Parts      int   // 当前分段数
}

// Request 描述一次下载请求。
type Request struct {
	URL        string            // 要下载的地址
	TargetFile string            // 保存到哪个文件；留空则自动取名（见 TargetDir）
	TargetDir  string            // 自动取名时存到哪个目录；留空表示当前目录
	Headers    map[string]string // 额外的请求头（可选）
}

// Result 是下载成功后的结果。
type Result struct {
	Path    string // 最终文件路径
	Size    int64  // 文件大小
	Speed   int64  // 平均速度（字节/秒）
	Parts   int    // 实际用到的分段数（用于验证"动态分段"）
	RangeOK bool   // 服务器是否支持分段
}

// LogEntry 是一条日志。Level 取值为 DEBUG / INFO / WARN / ERROR。
type LogEntry struct {
	Level   string
	Message string
}

// Callbacks 是宿主传入的回调，对应将来 C ABI 里的函数指针 + userdata。
type Callbacks struct {
	OnProgress func(Progress)
	OnStatus   func(Status)
	OnLog      func(LogEntry)
}
