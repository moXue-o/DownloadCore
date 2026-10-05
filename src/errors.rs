use std::fmt;

/// 错误分类：告诉调用方"能不能重试"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 可以自动重试（网络抖动、超时等）
    Retryable,
    /// 不可重试，需要停下来告诉宿主（磁盘没空间、服务器文件变了等）
    Fatal,
    /// 用户主动取消
    Canceled,
    /// 连接过慢，需要重开连接（内部使用）
    Slow,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub kind: ErrorKind,
    pub op: &'static str,
    pub message: String,
}

impl Error {
    pub fn canceled() -> Self {
        Error { kind: ErrorKind::Canceled, op: "cancel", message: "已取消".to_string() }
    }
    pub fn is_retryable(&self) -> bool {
        self.kind == ErrorKind::Retryable
    }
}

pub fn retryable(op: &'static str, message: impl Into<String>) -> Error {
    Error { kind: ErrorKind::Retryable, op, message: message.into() }
}

pub fn fatal(op: &'static str, message: impl Into<String>) -> Error {
    Error { kind: ErrorKind::Fatal, op, message: message.into() }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.op, self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// 达到重试上限。
pub const ERR_TOO_MANY_FAILURES: &str = "重试次数用尽";
/// 服务器返回的分段与请求不一致。
pub const ERR_RANGE_MISMATCH: &str = "服务器返回的分段与请求不一致";
