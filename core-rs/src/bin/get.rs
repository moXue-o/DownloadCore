//! 示例程序：演示"宿主程序如何调用下载核心"。
//!
//! 它只做三件事，正好对应宿主集成的三个动作：
//!   1. 用 `Config` 配好参数，`Engine::new(cfg)` 造引擎；
//!   2. 把 URL/落盘位置装进 `Request`；
//!   3. 挂上 `Callbacks`（进度 / 状态 / 日志），调用 `engine.download(...)`。
//!
//! 引擎本身不含界面、队列、显示平滑；这个程序负责"怎么显示、怎么写日志"。
//!
//! 运行：
//!   cargo run --bin get                交互式（提示输入网址）
//!   cargo run --bin get -- <网址>       直接下载
//!   cargo run --bin get -- -v           只打印版本
//! 输入 `selftest` 可不联网自检。

use downloadcore::{Callbacks, Config, Engine, Request, Status};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_STAMP: &str = env!("BUILD_STAMP");
const LOG_FILE: &str = "download.log";

fn main() {
    // `-v`：只看版本
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-v" || a == "-V" || a == "--version") {
        println!("简单下载器 v{VERSION} (build {BUILD_STAMP})");
        return;
    }

    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let log_path = dir.join(LOG_FILE);

    println!("==============================================");
    println!("  简单下载器  v{VERSION}  (build {BUILD_STAMP})");
    println!("==============================================");
    println!("文件保存到：{}", dir.display());
    println!("详细日志：  {}", log_path.display());
    println!("提示：输入 selftest 可不联网自检；直接回车退出。");
    println!();

    let logger = Arc::new(Logger::new(&log_path));
    logger.log("INFO", &format!("程序启动 v{VERSION} (build {BUILD_STAMP})，工作目录={}", dir.display()));

    // 命令行直接给了网址，就下这一个然后退出
    let direct = args.into_iter().find(|a| !a.starts_with('-'));
    if let Some(url) = direct {
        let rc = if url.eq_ignore_ascii_case("selftest") {
            run_selftest(&logger, &dir);
            0
        } else {
            run_one(&logger, &url, &dir)
        };
        std::process::exit(rc);
    }

    // 交互式循环
    let stdin = std::io::stdin();
    loop {
        print!("请输入下载网址（直接回车退出）：");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let link = line.trim().to_string();
        if link.is_empty() {
            logger.log("INFO", "用户选择退出");
            println!("已退出。");
            return;
        }
        if link.eq_ignore_ascii_case("selftest") {
            run_selftest(&logger, &dir);
        } else {
            run_one(&logger, &link, &dir);
        }
        println!();
    }
}

/// 一次真实下载：这就是"其他程序员调用引擎"的完整样子。
fn run_one(logger: &Arc<Logger>, url: &str, dir: &PathBuf) -> i32 {
    // 1) 配置 + 造引擎
    let mut cfg = Config::default();
    cfg.temp_dir = dir.join(".download-temp");
    let engine = Engine::new(cfg);

    // 2) 装请求
    let req = Request {
        url: url.to_string(),
        target_dir: Some(dir.display().to_string()), // 留空文件名 → 自动取名、下到当前目录
        ..Default::default()
    };

    // 3) 挂回调（进度 / 状态 / 日志）
    let cbs = make_callbacks(logger);

    logger.log("INFO", &format!("开始下载：{url}"));
    let start = Instant::now();
    match engine.download(req, cbs) {
        Ok(res) => {
            let abs = std::path::absolute(&res.path)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| res.path.clone());
            logger.log(
                "INFO",
                &format!(
                    "下载成功：{abs}（{} 字节，用时 {:.2} 秒，平均 {:.2} MB/s，分段 {}）",
                    res.size,
                    start.elapsed().as_secs_f64(),
                    res.speed as f64 / 1024.0 / 1024.0,
                    res.parts
                ),
            );
            println!(">>> 下载完成：{abs}");
            0
        }
        Err(e) => {
            logger.log("ERROR", &format!("下载失败：{e}"));
            println!(">>> 下载失败！详细原因见 {LOG_FILE} 最后几行。");
            1
        }
    }
}

/// 把引擎的三种回调接到我们的日志器上。
///
/// 真实宿主可以：进度 → 进度条；状态 → 界面；日志 → 落盘/上报。
/// 引擎只给"原始累计字节 + 瞬时速度"，平滑与显示由宿主决定。
fn make_callbacks(logger: &Arc<Logger>) -> Callbacks {
    let lg_log = logger.clone();
    let lg_status = logger.clone();
    let lg_prog = logger.clone();
    let last = Arc::new(Mutex::new(Instant::now()));
    Callbacks {
        on_status: Some(Box::new(move |s: Status| {
            lg_status.log("INFO", &format!("状态：{}", s.as_str()));
        })),
        on_log: Some(Box::new(move |entry| {
            lg_log.log(entry.level, &entry.message);
        })),
        on_progress: Some(Box::new(move |p| {
            // 每秒最多打一行，避免刷屏
            let mut last = last.lock().unwrap();
            if last.elapsed() < Duration::from_secs(1) {
                return;
            }
            *last = Instant::now();
            drop(last);
            let pct = if p.total > 0 {
                p.downloaded as f64 / p.total as f64 * 100.0
            } else {
                0.0
            };
            lg_prog.log(
                "INFO",
                &format!(
                    "进度：{pct:.1}%  已下 {:.2}/{:.2} MB  {:.2} MB/s  分段 {}",
                    p.downloaded as f64 / 1024.0 / 1024.0,
                    p.total as f64 / 1024.0 / 1024.0,
                    p.speed as f64 / 1024.0 / 1024.0,
                    p.parts
                ),
            );
        })),
    }
}

/// 不联网自检：起一个本地测试服务器，完整下一遍并校验。
fn run_selftest(logger: &Arc<Logger>, dir: &PathBuf) {
    logger.log("INFO", "开始离线自检（不联网）");
    let size = 8 << 20;
    let data: Vec<u8> = (0..size).map(|i| ((i as u64 * 31 + 7) & 0xff) as u8).collect();

    let (addr, _stop) = start_server(data.clone());
    let url = format!("http://{addr}/file.bin");

    let mut cfg = Config::default();
    cfg.temp_dir = dir.join(".download-temp");
    let engine = Engine::new(cfg);
    let req = Request {
        url,
        target_dir: Some(dir.display().to_string()),
        ..Default::default()
    };

    match engine.download(req, make_callbacks(logger)) {
        Ok(res) => {
            let got = match std::fs::read(&res.path) {
                Ok(g) => g,
                Err(e) => {
                    logger.log("ERROR", &format!("自检读取结果失败：{e}"));
                    println!(">>> 自检失败！详见 {LOG_FILE}");
                    return;
                }
            };
            if got == data {
                logger.log("INFO", "自检通过：与原文件逐字节一致");
                println!(">>> 自检通过");
            } else {
                logger.log("ERROR", "自检失败：内容不一致");
                println!(">>> 自检失败");
            }
            let _ = std::fs::remove_file(&res.path);
        }
        Err(e) => {
            logger.log("ERROR", &format!("自检下载失败：{e}"));
            println!(">>> 自检失败！详见 {LOG_FILE}");
        }
    }
}

// ---------------- 一个极简的本地测试服务器（仅用于 selftest） ----------------

fn start_server(data: Vec<u8>) -> (SocketAddr, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let st = stop.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            if st.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(stream) = stream {
                let d = data.clone();
                thread::spawn(move || {
                    let _ = serve(stream, &d);
                });
            }
        }
    });
    (addr, stop)
}

fn serve(mut stream: TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut range: Option<(i64, i64)> = None;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h)?;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("range:") {
            range = parse_range(rest.trim(), data.len() as i64);
        }
    }
    let size = data.len() as i64;
    match range {
        Some((start, end)) if start <= end && start < size => {
            let end = end.min(size - 1);
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                end - start + 1
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(&data[start as usize..=end as usize])?;
        }
        _ => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nAccept-Ranges: none\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(data)?;
        }
    }
    stream.flush()
}

fn parse_range(h: &str, size: i64) -> Option<(i64, i64)> {
    let spec = h.strip_prefix("bytes=")?;
    let dash = spec.find('-')?;
    let left = spec[..dash].trim();
    let right = spec[dash + 1..].trim();
    let (start, end) = if left.is_empty() {
        let n: i64 = right.parse().ok()?;
        ((size - n).max(0), size - 1)
    } else {
        let s: i64 = left.parse().ok()?;
        let e = if right.is_empty() { size - 1 } else { right.parse().ok()? };
        (s, e)
    };
    Some((start.max(0), end.min(size - 1)))
}

// ---------------- 详细日志器 ----------------

/// 同时写到控制台（INFO 以上）和日志文件（含 DEBUG），带时间戳。
/// 新文件开头写 UTF-8 BOM，方便 Windows 记事本正确识别中文。
struct Logger {
    file: Mutex<Option<File>>,
}

impl Logger {
    fn new(path: &PathBuf) -> Self {
        let fresh = !path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(path).ok();
        if fresh {
            if let Some(f) = file.as_mut() {
                let _ = f.write_all("\u{feff}".as_bytes());
            }
        }
        Logger { file: Mutex::new(file) }
    }

    fn log(&self, level: &str, msg: &str) {
        let line = format!("{} [{:<5}] {}", fmt_utc(SystemTime::now()), level, msg);
        if level != "DEBUG" {
            println!("{line}");
        }
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }
    }
}

/// 把 SystemTime 格式化成 UTC 的 "YYYY-MM-DD HH:MM:SS.mmm"（无第三方依赖）。
fn fmt_utc(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, dd) = civil_from_days(days);
    format!("{y:04}-{m:02}-{dd:02} {h:02}:{mi:02}:{s:02}.{millis:03}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
