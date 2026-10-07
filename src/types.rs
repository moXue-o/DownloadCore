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

/// 单个分段的进度快照。引擎内部的每一段用一个闭区间 [from, to] 表示，
/// `current` 是"下一个要写的字节位置"（因此 current == to+1 表示该段已下完）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartProgress {
    pub from: i64,
    pub to: i64,
    pub current: i64,
}

impl PartProgress {
    /// 该分段的总字节数。
    pub fn len(&self) -> i64 {
        (self.to - self.from + 1).max(0)
    }

    /// 该分段是否为空（长度为 0）。配合 clippy 的 len-without-is-empty 规则。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 该分段已下载的字节数（裁剪到 [0, len]）。
    pub fn done(&self) -> i64 {
        (self.current - self.from).clamp(0, self.len())
    }

    /// 该分段是否已完成。
    pub fn finished(&self) -> bool {
        self.done() >= self.len()
    }
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
    /// 保存到哪个文件（完整路径）。**必填**：核心不替你猜文件名。
    pub target_file: Option<String>,
    /// 额外的请求头
    pub headers: Vec<(String, String)>,
    /// 外部取消标志：置为 true 即中止下载（已下进度保留，供续传）
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 外部暂停标志：置为 true 即暂停（连接保持，恢复后继续），可从别的线程调用
    pub pause: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 期望的 SHA-256（十六进制）；给了它，文件下完后必须匹配，否则判失败（防静默损坏）
    pub expected_sha256: Option<String>,
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
    /// 分段快照回调（可选）：与进度同频（同样节流），给出每一段的 [from,to,current]。
    /// 宿主可据此绘制"每段进度"。不关心分段时留空即可。
    pub on_parts: Option<Box<dyn Fn(Vec<PartProgress>) + Send + Sync>>,
}

impl std::fmt::Debug for Callbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Callbacks { .. }")
    }
}
