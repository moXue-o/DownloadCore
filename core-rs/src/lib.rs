//! 下载核心（Rust 版）
//!
//! 定位：只做"把 URL 后面的字节，原样搬成本地文件"。
//! 不含界面、队列等外壳；引擎只负责榨干带宽并给出原始计数。

mod assemble;
mod client;
mod config;
mod engine;
mod errors;
mod ffi;
mod limiter;
mod part;
mod split;
mod store;
mod types;
mod util;

pub use config::{Config, DEFAULT_USER_AGENT};
pub use engine::Engine;
pub use errors::{Error, ErrorKind, Result};
pub use types::{Callbacks, DownloadResult, LogEntry, Progress, Request, Status};
