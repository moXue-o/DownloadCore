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
}

pub type dc_progress_cb = Option<extern "C" fn(userdata: *mut c_void, p: *const dc_progress)>;
pub type dc_status_cb = Option<extern "C" fn(userdata: *mut c_void, status: c_int)>;
pub type dc_log_cb =
    Option<extern "C" fn(userdata: *mut c_void, level: c_int, message: *const c_char)>;

pub struct dc_engine {
    engine: Engine,
    cancel: Arc<AtomicBool>,
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
        }
        Box::into_raw(Box::new(dc_engine {
            engine: Engine::new(c),
            cancel: Arc::new(AtomicBool::new(false)),
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

/// 同步下载一个文件。
///
/// 返回 0 成功；非 0 失败（此时 `*err_msg` 为错误信息，需 `dc_string_free`）。
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
    out_path: *mut *mut c_char,
    out_size: *mut i64,
    out_speed: *mut i64,
    out_parts: *mut usize,
    err_msg: *mut *mut c_char,
) -> c_int {
    if err_msg.is_null() {
        return 1;
    }

    let outcome = catch_unwind(AssertUnwindSafe(
        || -> std::result::Result<crate::types::DownloadResult, String> {
            if engine.is_null() || req.is_null() {
                return Err("engine/req 为空".to_string());
            }
            let e = unsafe { &*engine };
            let r = unsafe { &*req };
            let url = cstr_to_string(r.url).ok_or_else(|| "URL 为空".to_string())?;

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

            let request = Request {
                url,
                target_file: cstr_to_string(r.target_file),
                target_dir: cstr_to_string(r.target_dir),
                headers,
                cancel: Some(e.cancel.clone()),
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

            e.cancel.store(false, Ordering::SeqCst);
            e.engine
                .download(request, cbs)
                .map_err(|err| err.to_string())
        },
    ));

    match outcome {
        Ok(Ok(res)) => {
            unsafe {
                if !out_path.is_null() {
                    *out_path = leak_cstring(res.path);
                }
                if !out_size.is_null() {
                    *out_size = res.size;
                }
                if !out_speed.is_null() {
                    *out_speed = res.speed;
                }
                if !out_parts.is_null() {
                    *out_parts = res.parts;
                }
            }
            0
        }
        Ok(Err(msg)) => {
            unsafe { *err_msg = leak_cstring(msg) };
            1
        }
        Err(_) => {
            unsafe { *err_msg = leak_cstring("内部 panic 已被捕获".to_string()) };
            2
        }
    }
}
