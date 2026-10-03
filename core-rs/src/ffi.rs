//! C ABI 层：把下载核心暴露成宿主可"编译期吸收"的 C 接口。
//!
//! 设计要点：
//!   - 不透明句柄 `dc_engine*`；
//!   - 回调 + `userdata`（进度 / 状态 / 日志）；
//!   - 取消通过 `dc_engine_cancel`（可从另一个线程调用）；
//!   - 每个 `extern "C"` 边界都 `catch_unwind`，panic 不会穿透 FFI；
//!   - 返回的字符串由本侧分配，宿主用 `dc_string_free` 释放。

#![allow(non_camel_case_types)]

use crate::config::Config;
use crate::engine::Engine;
use crate::types::{Callbacks, LogEntry, Progress, Request, Status};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const DC_STATUS_PENDING: c_int = 0;
const DC_STATUS_PROBING: c_int = 1;
const DC_STATUS_DOWNLOADING: c_int = 2;
const DC_STATUS_ASSEMBLING: c_int = 3;
const DC_STATUS_COMPLETED: c_int = 4;
const DC_STATUS_FAILED: c_int = 5;
const DC_STATUS_CANCELED: c_int = 6;

const DC_LOG_DEBUG: c_int = 0;
const DC_LOG_INFO: c_int = 1;
const DC_LOG_WARN: c_int = 2;
const DC_LOG_ERROR: c_int = 3;

// 错误码（与 downloadcore.h 的 dc_error 一致）
const DC_OK: c_int = 0;
const DC_ERR_BUSY: c_int = 1;
const DC_ERR_CANCELED: c_int = 2;
const DC_ERR_RETRY_EXHAUSTED: c_int = 3;
const DC_ERR_RANGE: c_int = 4;
const DC_ERR_HTTP: c_int = 5;
const DC_ERR_IO: c_int = 6;
const DC_ERR_INVALID: c_int = 7;
const DC_ERR_INTERNAL: c_int = 8;
const DC_ERR_TARGET_BUSY: c_int = 9;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct dc_result {
    pub path: *mut c_char,
    pub size: i64,
    pub speed: i64,
    pub parts: usize,
    pub range_ok: c_int,
}

/// 把内部错误映射成对外的错误码。
fn error_code(e: &crate::errors::Error) -> c_int {
    use crate::errors::{ErrorKind, ERR_RANGE_MISMATCH, ERR_TOO_MANY_FAILURES};
    if e.kind == ErrorKind::Canceled {
        return DC_ERR_CANCELED;
    }
    // 分段不一致要优先判：它会被包成"重试次数用尽"，否则就永远报不出 DC_ERR_RANGE
    if e.op == "range" || e.message.contains(ERR_RANGE_MISMATCH) {
        return DC_ERR_RANGE;
    }
    if e.message.starts_with(ERR_TOO_MANY_FAILURES) {
        return DC_ERR_RETRY_EXHAUSTED;
    }
    // 构造/URL 错误（reqwest 会把它们包成 "send: builder error for url ..."）→ 参数无效
    if e.message.contains("builder error") || e.message.contains("invalid URL") {
        return DC_ERR_INVALID;
    }
    match e.op {
        "write" | "open" | "create" | "mkdir" | "assemble" | "rename" | "preallocate" | "seek" => {
            DC_ERR_IO
        }
        // 目标文件被别的任务占（区别于"同一句柄重入"的 DC_ERR_BUSY）
        "busy" => DC_ERR_TARGET_BUSY,
        // 网络层错误（既不是"参数无效"，也不是磁盘/内部错误）
        "send" | "connect" | "tls" | "probe" | "response" | "read" | "range" | "whole"
        | "redirect" => DC_ERR_HTTP,
        // 真正的参数问题（URL 为空/非法、构造失败）
        "url" | "request" => DC_ERR_INVALID,
        _ => {
            if e.message.contains("builder error") || e.message.contains("invalid URL") {
                DC_ERR_INVALID
            } else if e.message.contains("服务器返回状态") {
                DC_ERR_HTTP
            } else {
                DC_ERR_INTERNAL
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct dc_progress {
    pub downloaded: i64,
    pub total: i64,
    pub speed: i64,
    pub parts: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct dc_config {
    pub initial_threads: c_int,
    pub max_threads: c_int,
    pub min_part_size: i64,
    pub buffer_size: c_int,
    pub idle_timeout_ms: c_int,
    pub max_retries: c_int,
    pub retry_delay_ms: c_int,
    pub temp_dir: *const c_char,
    pub incomplete_suffix: *const c_char,
    pub user_agent: *const c_char,
    pub max_speed: u64,
    pub adaptive_threads: c_int,
    pub use_multiple_ips: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct dc_request {
    pub url: *const c_char,
    pub target_file: *const c_char,
    pub target_dir: *const c_char,
    pub header_keys: *const *const c_char,
    pub header_values: *const *const c_char,
    pub header_count: usize,
    pub mirror_urls: *const *const c_char,
    pub mirror_count: usize,
    /// 期望的 SHA-256（可选，NULL=不校验）；给了就必须匹配，否则判失败
    pub expected_sha256: *const c_char,
}

pub type dc_progress_cb = Option<extern "C" fn(userdata: *mut c_void, p: *const dc_progress)>;
pub type dc_status_cb = Option<extern "C" fn(userdata: *mut c_void, status: c_int)>;
pub type dc_log_cb =
    Option<extern "C" fn(userdata: *mut c_void, level: c_int, message: *const c_char)>;

pub struct dc_engine {
    engine: Engine,
    /// 防并发：同一句柄一次只允许一个下载
    busy: AtomicBool,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
}

/// 进下载时置忙、离开时自动复位（任何提前返回都会复位）。
/// 结束时也清掉 cancel/pause：避免"上一个任务的迟到取消"误伤下一个任务。
struct BusyGuard<'a>(&'a dc_engine);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.cancel.store(false, Ordering::SeqCst);
        self.0.pause.store(false, Ordering::SeqCst);
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

fn cstr_to_string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn leak_cstring(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => CString::new("(message contained NUL)").unwrap().into_raw(),
    }
}

fn status_to_c(s: Status) -> c_int {
    match s {
        Status::Pending => DC_STATUS_PENDING,
        Status::Probing => DC_STATUS_PROBING,
        Status::Downloading => DC_STATUS_DOWNLOADING,
        Status::Assembling => DC_STATUS_ASSEMBLING,
        Status::Completed => DC_STATUS_COMPLETED,
        Status::Failed => DC_STATUS_FAILED,
        Status::Canceled => DC_STATUS_CANCELED,
    }
}

fn level_to_c(level: &str) -> c_int {
    match level {
        "DEBUG" => DC_LOG_DEBUG,
        "WARN" => DC_LOG_WARN,
        "ERROR" => DC_LOG_ERROR,
        _ => DC_LOG_INFO,
    }
}

/// 返回版本号（静态字符串，无需释放）。
#[unsafe(no_mangle)]
pub extern "C" fn dc_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// 返回编译时间戳（静态字符串，无需释放）。
#[unsafe(no_mangle)]
pub extern "C" fn dc_build_stamp() -> *const c_char {
    concat!(env!("BUILD_STAMP"), "\0").as_ptr() as *const c_char
}

/// 释放由本库返回的字符串。
///
/// # Safety
/// `s` 必须来自本库（如 `err_msg`/`out_path`），且只能释放一次。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dc_string_free(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}

/// 释放 dc_result 里由本库分配的内容（并把字段清零）。
///
/// # Safety
/// `r` 必须是本库填过的 dc_result，且只能释放一次。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dc_result_free(r: *mut dc_result) {
    if r.is_null() {
        return;
    }
    let r = unsafe { &mut *r };
    if !r.path.is_null() {
        unsafe { drop(CString::from_raw(r.path)) };
        r.path = std::ptr::null_mut();
    }
}

/// 返回一套默认配置（字符串字段为空 = 用内置默认）。
#[unsafe(no_mangle)]
pub extern "C" fn dc_config_default() -> dc_config {
    let d = Config::default();
    dc_config {
        initial_threads: d.initial_threads as c_int,
        max_threads: d.max_threads as c_int,
        min_part_size: d.min_part_size,
        buffer_size: d.buffer_size as c_int,
        idle_timeout_ms: d.idle_timeout.as_millis() as c_int,
        max_retries: d.max_retries as c_int,
        retry_delay_ms: d.retry_delay.as_millis() as c_int,
        temp_dir: std::ptr::null(),
        incomplete_suffix: std::ptr::null(),
        user_agent: std::ptr::null(),
        max_speed: d.max_speed,
        adaptive_threads: if d.adaptive_threads { 1 } else { 0 },
        use_multiple_ips: if d.use_multiple_ips { 1 } else { 0 },
    }
}

/// 创建引擎。失败返回 NULL。
///
/// # Safety
/// `cfg` 可为 NULL（表示全用默认）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dc_engine_new(cfg: *const dc_config) -> *mut dc_engine {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut c = Config::default();
        if !cfg.is_null() {
            let cc = unsafe { *cfg };
            if cc.initial_threads > 0 {
                c.initial_threads = cc.initial_threads as usize;
            }
            if cc.max_threads > 0 {
                c.max_threads = cc.max_threads as usize;
            }
            if cc.min_part_size > 0 {
                c.min_part_size = cc.min_part_size;
            }
            if cc.buffer_size > 0 {
                c.buffer_size = cc.buffer_size as usize;
            }
            if cc.idle_timeout_ms > 0 {
                c.idle_timeout = Duration::from_millis(cc.idle_timeout_ms as u64);
            }
            if cc.max_retries >= 0 {
                c.max_retries = cc.max_retries as usize;
            }
            if cc.retry_delay_ms > 0 {
                c.retry_delay = Duration::from_millis(cc.retry_delay_ms as u64);
            }
            if let Some(s) = cstr_to_string(cc.temp_dir) {
                c.temp_dir = PathBuf::from(s);
            }
            if let Some(s) = cstr_to_string(cc.incomplete_suffix) {
                c.incomplete_suffix = s;
            }
            if let Some(s) = cstr_to_string(cc.user_agent) {
                c.user_agent = s;
            }
            c.max_speed = cc.max_speed;
            c.adaptive_threads = cc.adaptive_threads != 0;
            c.use_multiple_ips = cc.use_multiple_ips != 0;
        }
        Box::into_raw(Box::new(dc_engine {
            engine: Engine::new(c),
            busy: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
        }))
    }));
    result.unwrap_or(std::ptr::null_mut())
}

/// 销毁引擎。
///
/// # Safety
/// `engine` 必须来自 `dc_engine_new`，且只能销毁一次；
/// 调用前必须确保没有正在进行的 `dc_engine_download`。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dc_engine_free(engine: *mut dc_engine) {
    if !engine.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(engine));
        }));
    }
}

/// 请求取消当前下载（可从另一个线程调用）。
///
/// # Safety
/// `engine` 必须是有效句柄。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dc_engine_cancel(engine: *mut dc_engine) {
    if !engine.is_null() {
        let e = unsafe { &*engine };
        e.cancel.store(true, Ordering::SeqCst);
    }
}

/// 暂停当前下载：连接保持，恢复后继续（可从另一个线程调用）。
///
/// # Safety
/// `engine` 必须是有效句柄。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dc_engine_pause(engine: *mut dc_engine) {
    if !engine.is_null() {
        let e = unsafe { &*engine };
        e.pause.store(true, Ordering::SeqCst);
    }
}

/// 恢复被暂停的下载。
///
/// # Safety
/// `engine` 必须是有效句柄。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dc_engine_resume(engine: *mut dc_engine) {
    if !engine.is_null() {
        let e = unsafe { &*engine };
        e.pause.store(false, Ordering::SeqCst);
    }
}

/// 同步下载一个文件。
///
/// 返回 0 成功；非 0 为 dc_error 错误码（此时 `*err_msg` 为错误信息，需 `dc_string_free`）。
/// 结果写入 `*out`（可为 NULL），其中 path 需 `dc_result_free` 释放。
///
/// # Safety
/// 指针参数必须有效；回调需在下载期间保持有效。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dc_engine_download(
    engine: *mut dc_engine,
    req: *const dc_request,
    on_progress: dc_progress_cb,
    on_status: dc_status_cb,
    on_log: dc_log_cb,
    userdata: *mut c_void,
    out: *mut dc_result,
    err_msg: *mut *mut c_char,
) -> c_int {
    if err_msg.is_null() {
        return DC_ERR_INVALID;
    }
    unsafe { *err_msg = std::ptr::null_mut() } // 成功时不返回错误信息，先置空免得宿主读到垃圾
    if !out.is_null() {
        unsafe {
            *out = dc_result {
                path: std::ptr::null_mut(),
                size: 0,
                speed: 0,
                parts: 0,
                range_ok: 0,
            };
        }
    }

    let outcome = catch_unwind(AssertUnwindSafe(
        || -> std::result::Result<crate::types::DownloadResult, (c_int, String)> {
            if engine.is_null() || req.is_null() {
                return Err((DC_ERR_INVALID, "engine/req 为空".to_string()));
            }
            let e = unsafe { &*engine };
            if e.busy.swap(true, Ordering::SeqCst) {
                return Err((DC_ERR_BUSY, "引擎正忙：同一个句柄不支持并发下载".to_string()));
            }
            let _busy = BusyGuard(e);
            // 每次下载前复位取消/暂停标志
            e.cancel.store(false, Ordering::SeqCst);
            e.pause.store(false, Ordering::SeqCst);
            let r = unsafe { &*req };
            let url = cstr_to_string(r.url).ok_or_else(|| (DC_ERR_INVALID, "URL 为空".to_string()))?;

            let mut headers = Vec::new();
            if r.header_count > 0 && !r.header_keys.is_null() && !r.header_values.is_null() {
                for i in 0..r.header_count {
                    let k = unsafe { *r.header_keys.add(i) };
                    let v = unsafe { *r.header_values.add(i) };
                    if let (Some(k), Some(v)) = (cstr_to_string(k), cstr_to_string(v)) {
                        headers.push((k, v));
                    }
                }
            }

            let mut mirrors = Vec::new();
            if r.mirror_count > 0 && !r.mirror_urls.is_null() {
                for i in 0..r.mirror_count {
                    let m = unsafe { *r.mirror_urls.add(i) };
                    if let Some(m) = cstr_to_string(m) {
                        mirrors.push(m);
                    }
                }
            }

            let request = Request {
                url,
                target_file: cstr_to_string(r.target_file),
                target_dir: cstr_to_string(r.target_dir),
                headers,
                cancel: Some(e.cancel.clone()),
                pause: Some(e.pause.clone()),
                mirrors,
                expected_sha256: cstr_to_string(r.expected_sha256),
            };

            let ud = userdata as usize;
            let cbs = Callbacks {
                on_progress: on_progress.map(|cb| {
                    Box::new(move |p: Progress| {
                        let cp = dc_progress {
                            downloaded: p.downloaded,
                            total: p.total,
                            speed: p.speed,
                            parts: p.parts,
                        };
                        cb(ud as *mut c_void, &cp as *const dc_progress);
                    }) as Box<dyn Fn(Progress) + Send + Sync>
                }),
                on_status: on_status.map(|cb| {
                    Box::new(move |s: Status| {
                        cb(ud as *mut c_void, status_to_c(s));
                    }) as Box<dyn Fn(Status) + Send + Sync>
                }),
                on_log: on_log.map(|cb| {
                    Box::new(move |entry: LogEntry| {
                        if let Ok(cs) = CString::new(entry.message) {
                            cb(ud as *mut c_void, level_to_c(entry.level), cs.as_ptr());
                        }
                    }) as Box<dyn Fn(LogEntry) + Send + Sync>
                }),
            };

            e.engine
                .download(request, cbs)
                .map_err(|err| (error_code(&err), err.to_string()))
        },
    ));

    match outcome {
        Ok(Ok(res)) => {
            if !out.is_null() {
                unsafe {
                    let o = &mut *out;
                    o.path = leak_cstring(res.path);
                    o.size = res.size;
                    o.speed = res.speed;
                    o.parts = res.parts;
                    o.range_ok = if res.range_ok { 1 } else { 0 };
                }
            }
            DC_OK
        }
        Ok(Err((code, msg))) => {
            unsafe { *err_msg = leak_cstring(msg) };
            code
        }
        Err(_) => {
            unsafe { *err_msg = leak_cstring("内部 panic 已被捕获".to_string()) };
            DC_ERR_INTERNAL
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    /// 这些数字必须与 `include/downloadcore.h` 里的 C 结构体一致。
    /// 任何一边改动导致对不齐，这个测试会先炸出来。
    #[test]
    fn c_layout_matches_header() {
        assert_eq!(align_of::<dc_progress>(), 8);
        assert_eq!(size_of::<dc_progress>(), 32);

        assert_eq!(align_of::<dc_result>(), 8);
        assert_eq!(size_of::<dc_result>(), 40);
        assert_eq!(offset_of!(dc_result, path), 0);
        assert_eq!(offset_of!(dc_result, size), 8);
        assert_eq!(offset_of!(dc_result, speed), 16);
        assert_eq!(offset_of!(dc_result, parts), 24);
        assert_eq!(offset_of!(dc_result, range_ok), 32);

        assert_eq!(align_of::<dc_request>(), 8);
        assert_eq!(size_of::<dc_request>(), 72);
        assert_eq!(offset_of!(dc_request, url), 0);
        assert_eq!(offset_of!(dc_request, target_file), 8);
        assert_eq!(offset_of!(dc_request, target_dir), 16);
        assert_eq!(offset_of!(dc_request, header_keys), 24);
        assert_eq!(offset_of!(dc_request, header_values), 32);
        assert_eq!(offset_of!(dc_request, header_count), 40);
        assert_eq!(offset_of!(dc_request, mirror_urls), 48);
        assert_eq!(offset_of!(dc_request, mirror_count), 56);
        assert_eq!(offset_of!(dc_request, expected_sha256), 64);

        assert_eq!(align_of::<dc_config>(), 8);
        assert_eq!(size_of::<dc_config>(), 72);
        assert_eq!(offset_of!(dc_config, initial_threads), 0);
        assert_eq!(offset_of!(dc_config, max_threads), 4);
        assert_eq!(offset_of!(dc_config, min_part_size), 8);
        assert_eq!(offset_of!(dc_config, temp_dir), 32);
        assert_eq!(offset_of!(dc_config, incomplete_suffix), 40);
        assert_eq!(offset_of!(dc_config, user_agent), 48);
        assert_eq!(offset_of!(dc_config, max_speed), 56);
        assert_eq!(offset_of!(dc_config, adaptive_threads), 64);
        assert_eq!(offset_of!(dc_config, use_multiple_ips), 68);
    }

    #[test]
    fn error_code_mapping() {
        use crate::errors::{fatal, retryable, Error, ERR_RANGE_MISMATCH, ERR_TOO_MANY_FAILURES};
        assert_eq!(error_code(&Error::canceled()), DC_ERR_CANCELED);
        // 非法 URL → INVALID（不是 INTERNAL）
        assert_eq!(error_code(&fatal("url", "网址不合法: x")), DC_ERR_INVALID);
        // 重试次数用尽 → RETRY_EXHAUSTED
        assert_eq!(
            error_code(&fatal("part", format!("{ERR_TOO_MANY_FAILURES}: x"))),
            DC_ERR_RETRY_EXHAUSTED
        );
        // 分段不一致即使被包成"重试次数用尽"，也要报 RANGE（不能死码）
        assert_eq!(
            error_code(&fatal("part", format!("{ERR_TOO_MANY_FAILURES}: {ERR_RANGE_MISMATCH}: x"))),
            DC_ERR_RANGE
        );
        assert_eq!(error_code(&retryable("range", format!("{ERR_RANGE_MISMATCH}: x"))), DC_ERR_RANGE);
        // 目标被占 vs 磁盘/网络
        assert_eq!(error_code(&fatal("busy", "目标文件正在被另一个下载任务使用")), DC_ERR_TARGET_BUSY);
        assert_eq!(error_code(&fatal("write", "x")), DC_ERR_IO);
        assert_eq!(error_code(&retryable("connect", "x")), DC_ERR_HTTP);
        // LTS 会把非法 URL 包成 send: builder error → 仍要判 INVALID
        assert_eq!(
            error_code(&retryable("send", "builder error for url (ftp://x/f)")),
            DC_ERR_INVALID
        );
    }

    #[test]
    fn dc_engine_download_end_to_end() {
        use std::ffi::CStr;
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        // 极简本地服务器（支持 Range）
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                if let Ok(mut s) = s {
                    std::thread::spawn(move || {
                        let mut r = BufReader::new(s.try_clone().unwrap());
                        let mut range: Option<(i64, i64)> = None;
                        loop {
                            let mut l = String::new();
                            if r.read_line(&mut l).unwrap() == 0 || l == "\r\n" {
                                break;
                            }
                            let low = l.to_ascii_lowercase();
                            if let Some(rest) = low.strip_prefix("range:") {
                                let spec = rest.trim().strip_prefix("bytes=").unwrap_or("");
                                if let Some((a, b)) = spec.split_once('-') {
                                    let a: i64 = a.parse().unwrap_or(0);
                                    let b: i64 = if b.is_empty() {
                                        999_999
                                    } else {
                                        b.parse().unwrap_or(0)
                                    };
                                    range = Some((a, b));
                                }
                            }
                        }
                        let total: i64 = 1_000_000;
                        let body: Vec<u8> = match range {
                            Some((a, b)) => {
                                let b = b.min(total - 1);
                                let head = format!(
                                    "HTTP/1.1 206 Partial Content\r\nETag: \"v1\"\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {a}-{b}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    b - a + 1
                                );
                                let _ = s.write_all(head.as_bytes());
                                (a..=b).map(|i| (i & 0xff) as u8).collect()
                            }
                            None => {
                                let head = format!(
                                    "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nAccept-Ranges: bytes\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                                );
                                let _ = s.write_all(head.as_bytes());
                                (0..total).map(|i| (i & 0xff) as u8).collect()
                            }
                        };
                        let _ = s.write_all(&body);
                    });
                }
            }
        });

        let url = format!("http://{addr}/f.bin");
        let dir = std::env::temp_dir().join(format!("dcdemo-ffi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c_url = std::ffi::CString::new(url).unwrap();
        let c_dir = std::ffi::CString::new(dir.display().to_string()).unwrap();
        let mut req: dc_request = unsafe { std::mem::zeroed() };
        req.url = c_url.as_ptr();
        req.target_dir = c_dir.as_ptr();

        let e = unsafe { dc_engine_new(std::ptr::null()) };
        assert!(!e.is_null());
        let mut out: dc_result = unsafe { std::mem::zeroed() };
        let mut err: *mut c_char = std::ptr::null_mut();
        let rc = unsafe {
            dc_engine_download(
                e,
                &req,
                None,
                None,
                None,
                std::ptr::null_mut(),
                &mut out,
                &mut err,
            )
        };
        assert_eq!(rc, DC_OK, "FFI 下载失败");
        assert_eq!(out.size, 1_000_000);
        assert!(!out.path.is_null());
        let path = unsafe { CStr::from_ptr(out.path) }.to_string_lossy().to_string();
        let got = std::fs::read(&path).unwrap();
        let expect: Vec<u8> = (0..1_000_000).map(|i| (i & 0xff) as u8).collect();
        assert_eq!(got, expect, "FFI 下载内容不一致");

        unsafe {
            dc_result_free(&mut out);
            dc_engine_free(e);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
