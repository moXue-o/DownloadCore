/// 下载任务的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Probing,
    Downloading,
    Assembling,
    Completed,
    Failed,
    Canceled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Probing => "probing",
            Status::Downloading => "downloading",
            Status::Assembling => "assembling",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Canceled => "canceled",
        }
    }
}

/// 一次进度回调的快照。引擎只给"原始累计字节 + 一个简单瞬时速度"，
/// 如何平滑、怎么显示，交给宿主。
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub downloaded: i64,
    pub total: i64,
    pub speed: i64,
    pub parts: usize,
}

/// 一条日志。level 取值为 DEBUG / INFO / WARN / ERROR。
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: &'static str,
    pub message: String,
}

/// 下载请求。
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub url: String,
    /// 保存到哪个文件；留空则自动取名（见 target_dir）
    pub target_file: Option<String>,
    /// 自动取名时存到哪个目录；留空表示当前目录
    pub target_dir: Option<String>,
    /// 额外的请求头
    pub headers: Vec<(String, String)>,
    /// 外部取消标志：置为 true 即中止下载（已下进度保留，供续传）
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 外部暂停标志：置为 true 即暂停（连接保持，恢复后继续），可从别的线程调用
    pub pause: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// 下载成功后的结果。
#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub path: String,
    pub size: i64,
    pub speed: i64,
    pub parts: usize,
    pub range_ok: bool,
}

/// 宿主传入的回调。多个工人会并发调用，回调实现需自行保证线程安全。
#[derive(Default)]
pub struct Callbacks {
    pub on_progress: Option<Box<dyn Fn(Progress) + Send + Sync>>,
    pub on_status: Option<Box<dyn Fn(Status) + Send + Sync>>,
    pub on_log: Option<Box<dyn Fn(LogEntry) + Send + Sync>>,
}

impl std::fmt::Debug for Callbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Callbacks { .. }")
    }
}
