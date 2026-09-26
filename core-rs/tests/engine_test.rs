//! 端到端测试：用一个 std 实现的、可摆布的测试 HTTP 服务器，
//! 验证多线程、动态分段、不支持分段、对暗号、空闲超时、取消续传、换文件。

use downloadcore::{Callbacks, Config, Engine, ErrorKind, Request};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ---------------- 测试服务器 ----------------

struct TestServer {
    addr: SocketAddr,
    data: Arc<Mutex<Vec<u8>>>,
    etag: Arc<Mutex<String>>,
    no_range: Arc<AtomicBool>,
    fail_range: Arc<AtomicBool>,
    stall_after: Arc<AtomicI64>,
    speed: Arc<AtomicI64>,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn new(data: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let data = Arc::new(Mutex::new(data));
        let etag = Arc::new(Mutex::new("\"v1\"".to_string()));
        let no_range = Arc::new(AtomicBool::new(false));
        let fail_range = Arc::new(AtomicBool::new(false));
        let stall_after = Arc::new(AtomicI64::new(0));
        let speed = Arc::new(AtomicI64::new(0));
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let (d, e, n, f, s, sp, st, ht) = (
            data.clone(),
            etag.clone(),
            no_range.clone(),
            fail_range.clone(),
            stall_after.clone(),
            speed.clone(),
            stop.clone(),
            hits.clone(),
        );
        let handle = thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let (d, e, n, f, s, sp, ht) =
                            (d.clone(), e.clone(), n.clone(), f.clone(), s.clone(), sp.clone(), ht.clone());
                        thread::spawn(move || {
                            let _ = handle_conn(stream, d, e, n, f, s, sp, ht);
                        });
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        TestServer { addr, data, etag, no_range, fail_range, stall_after, speed, hits, stop, handle: Some(handle) }
    }

    fn url(&self) -> String {
        format!("http://{}/file.bin", self.addr)
    }
    fn set_data(&self, d: Vec<u8>, etag: &str) {
        *self.data.lock().unwrap() = d;
        *self.etag.lock().unwrap() = etag.to_string();
    }
    fn set_no_range(&self, v: bool) {
        self.no_range.store(v, Ordering::SeqCst);
    }
    fn set_fail_range(&self, v: bool) {
        self.fail_range.store(v, Ordering::SeqCst);
    }
    fn set_stall_after(&self, n: i64) {
        self.stall_after.store(n, Ordering::SeqCst);
    }
    fn set_speed(&self, bps: i64) {
        self.speed.store(bps, Ordering::SeqCst);
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_conn(
    mut stream: TcpStream,
    data: Arc<Mutex<Vec<u8>>>,
    etag: Arc<Mutex<String>>,
    no_range: Arc<AtomicBool>,
    fail_range: Arc<AtomicBool>,
    stall_after: Arc<AtomicI64>,
    speed: Arc<AtomicI64>,
    hits: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    hits.fetch_add(1, Ordering::SeqCst);
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
            let size = data.lock().unwrap().len() as i64;
            range = parse_range(rest.trim(), size);
        }
    }

    let body = data.lock().unwrap().clone();
    let etag_v = etag.lock().unwrap().clone();
    let size = body.len() as i64;
    let stall = stall_after.load(Ordering::SeqCst);
    let bps = speed.load(Ordering::SeqCst);

    if no_range.load(Ordering::SeqCst) || range.is_none() {
        let head = format!(
            "HTTP/1.1 200 OK\r\nETag: {etag_v}\r\nAccept-Ranges: none\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes())?;
        write_body(&mut stream, &body, stall, bps)?;
        return Ok(());
    }

    let (start, end) = range.unwrap();
    if start > end || start >= size {
        stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }
    let end = end.min(size - 1);
    let report_start = if fail_range.load(Ordering::SeqCst) && end - start + 1 > 1 {
        start + 1
    } else {
        start
    };
    let head = format!(
        "HTTP/1.1 206 Partial Content\r\nETag: {etag_v}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {report_start}-{end}/{size}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        end - start + 1
    );
    stream.write_all(head.as_bytes())?;
    write_body(&mut stream, &body[start as usize..=end as usize], stall, bps)?;
    Ok(())
}

fn write_body(stream: &mut TcpStream, data: &[u8], stall_after: i64, bps: i64) -> std::io::Result<()> {
    let mut written = 0i64;
    for chunk in data.chunks(32 * 1024) {
        if stall_after > 0 && written >= stall_after {
            thread::sleep(Duration::from_secs(10));
            return Ok(());
        }
        stream.write_all(chunk)?;
        stream.flush()?;
        written += chunk.len() as i64;
        if bps > 0 {
            let secs = chunk.len() as f64 / bps as f64;
            thread::sleep(Duration::from_secs_f64(secs));
        }
    }
    Ok(())
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
    if start < 0 {
        return Some((0, end.min(size - 1)));
    }
    Some((start, end.min(size - 1)))
}

// ---------------- 测试辅助 ----------------

fn make_data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u64 * 31 + seed as u64 * 7) & 0xff) as u8).collect()
}

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("dlcore-{tag}-{nanos}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn test_config(dir: &PathBuf) -> Config {
    let mut c = Config::default();
    c.initial_threads = 1;
    c.max_threads = 4;
    c.min_part_size = 256 << 10;
    c.temp_dir = dir.join("temp");
    c.idle_timeout = Duration::from_secs(3);
    c.max_retries = 2;
    c.retry_delay = Duration::from_millis(50);
    c.adaptive_threads = false; // 测试默认固定并发，便于断言
    c
}

fn read_file(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

// ---------------- 测试 ----------------

#[test]
fn download_multithread_matches_source() {
    let data = make_data(4 << 20, 1);
    let srv = TestServer::new(data.clone());
    let dir = unique_dir("multi");
    let target = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    assert!(res.range_ok);
    assert!(res.parts >= 2, "期望动态分段 >1 段，实际 {}", res.parts);
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn no_range_falls_back_to_single_stream() {
    let data = make_data(1 << 20, 2);
    let srv = TestServer::new(data.clone());
    srv.set_no_range(true);
    let dir = unique_dir("norange");
    let target = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    assert!(!res.range_ok);
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn range_mismatch_is_detected() {
    let data = make_data(1 << 20, 3);
    let srv = TestServer::new(data);
    srv.set_fail_range(true);
    let dir = unique_dir("mismatch");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.initial_threads = 1;
    cfg.max_threads = 1;
    cfg.max_retries = 1;
    let engine = Engine::new(cfg);

    let err = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Fatal);
}

#[test]
fn idle_timeout_triggers_error() {
    let data = make_data(2 << 20, 4);
    let srv = TestServer::new(data);
    srv.set_stall_after(64 << 10);
    let dir = unique_dir("idle");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.initial_threads = 1;
    cfg.max_threads = 1;
    cfg.idle_timeout = Duration::from_millis(300);
    cfg.max_retries = 1;
    let engine = Engine::new(cfg);

    let err = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Fatal);
}

#[test]
fn cancel_then_resume() {
    let data = make_data(4 << 20, 5);
    let srv = TestServer::new(data.clone());
    srv.set_speed(2 << 20); // 2 MiB/s，保证能中途取消
    let dir = unique_dir("resume");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.max_retries = 0;
    cfg.idle_timeout = Duration::from_secs(5);
    let engine = Engine::new(cfg);

    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(400));
        c2.store(true, Ordering::SeqCst);
    });

    let err = engine
        .download(
            Request {
                url: srv.url(),
                target_file: Some(target.display().to_string()),
                cancel: Some(cancel),
                ..Default::default()
            },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Canceled);
    assert!(!target.exists(), "取消后不应存在最终文件");

    // 恢复：全速重下剩余部分
    srv.set_speed(0);
    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn file_changed_on_resume_starts_fresh() {
    let data1 = make_data(2 << 20, 6);
    let data2 = make_data(2 << 20, 7);
    let srv = TestServer::new(data1);
    srv.set_speed(1 << 20);
    let dir = unique_dir("changed");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.max_retries = 0;
    cfg.idle_timeout = Duration::from_secs(5);
    let engine = Engine::new(cfg);

    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        c2.store(true, Ordering::SeqCst);
    });
    let err = engine
        .download(
            Request {
                url: srv.url(),
                target_file: Some(target.display().to_string()),
                cancel: Some(cancel),
                ..Default::default()
            },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Canceled);

    // 服务器上的文件变了
    srv.set_data(data2.clone(), "\"v2\"");
    srv.set_speed(0);

    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    // 必须是"新文件"，绝不能新旧内容拼在一起
    assert_eq!(read_file(&res.path), data2);
}

#[test]
fn pause_and_resume_completes_correctly() {
    let data = make_data(4 << 20, 8);
    let srv = TestServer::new(data.clone());
    srv.set_speed(2 << 20); // 慢一点，便于中途暂停
    let dir = unique_dir("pause");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.idle_timeout = Duration::from_secs(5);
    let engine = Engine::new(cfg);

    let pause = Arc::new(AtomicBool::new(false));
    let p = pause.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        p.store(true, Ordering::SeqCst); // 暂停
        thread::sleep(Duration::from_millis(500));
        p.store(false, Ordering::SeqCst); // 恢复
    });

    let res = engine
        .download(
            Request {
                url: srv.url(),
                target_file: Some(target.display().to_string()),
                pause: Some(pause),
                ..Default::default()
            },
            Callbacks::default(),
        )
        .unwrap();
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn adaptive_threads_grow() {
    let data = make_data(6 << 20, 10);
    let srv = TestServer::new(data.clone());
    srv.set_speed(1 << 20); // 每连接 1 MB/s，慢一点给自适应时间
    let dir = unique_dir("adaptive");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.initial_threads = 1;
    cfg.max_threads = 8;
    cfg.adaptive_threads = true;
    let engine = Engine::new(cfg);

    let t0 = std::time::Instant::now();
    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    let dt = t0.elapsed();
    assert_eq!(read_file(&res.path), data);
    assert!(res.parts >= 2, "自适应应当会加人，实际分段 {}，用时 {:?}", res.parts, dt);
}

#[test]
fn mirrors_are_used() {
    let data = make_data(2 << 20, 11);
    let slow = TestServer::new(data.clone());
    slow.set_speed(1 << 20); // 主源限速
    let fast = TestServer::new(data.clone());
    let dir = unique_dir("mirror");
    let target = dir.join("out.bin");
    let cfg = test_config(&dir);
    let engine = Engine::new(cfg);

    let res = engine
        .download(
            Request {
                url: slow.url(),
                target_file: Some(target.display().to_string()),
                mirrors: vec![fast.url()],
                ..Default::default()
            },
            Callbacks::default(),
        )
        .unwrap();
    assert_eq!(read_file(&res.path), data);
    assert!(fast.hits() > 0, "镜像应当被用到");
}
