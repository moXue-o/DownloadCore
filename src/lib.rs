//! 下载核心（Rust 版）
//!
//! 定位：只做"把 URL 后面的字节，原样搬成本地文件"。
//! 不含界面、队列等外壳；引擎只负责榨干带宽并给出原始计数。

// 两套网络后端只能二选一（默认不带特性 = 自研后端）
#[cfg(all(feature = "backend-native", feature = "backend-lts"))]
compile_error!("`backend-native` 与 `backend-lts` 不能同时启用，请二选一");

pub mod backend;
#[cfg(feature = "backend-lts")]
mod client;
mod config;
mod engine;
mod errors;
mod ffi;
mod limiter;
pub mod netclient;
mod part;
mod split;
mod store;
mod types;
mod util;

pub use config::{Config, DEFAULT_USER_AGENT};
pub use engine::Engine;
pub use errors::{Error, ErrorKind, Result};
pub use types::{Callbacks, DownloadResult, LogEntry, PartProgress, Progress, Request, Status};
pub use util::{sha256_file, to_hex, Sha256};
