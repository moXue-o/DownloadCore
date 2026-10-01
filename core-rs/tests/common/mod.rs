//! 测试公用件：一个 std 实现的、可摆布的本地 HTTP 服务器 + 小工具。
//! 各测试文件用 `mod common;` 引入。
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone)]
struct Shared {
    data: Arc<Mutex<Vec<u8>>>,
    etag: Arc<Mutex<String>>,
    no_range: Arc<AtomicBool>,
    fail_range: Arc<AtomicBool>,
    stall_after: Arc<AtomicI64>,
    speed: Arc<AtomicI64>,
    hits: Arc<AtomicUsize>,
    forced_status: Arc<AtomicI64>,
    chunked: Arc<AtomicBool>,
    redirect: Arc<Mutex<Option<String>>>,
}

pub struct TestServer {
    addr: SocketAddr,
    shared: Shared,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    pub fn new(data: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();

        let shared = Shared {
            data: Arc::new(Mutex::new(data)),
            etag: Arc::new(Mutex::new("\"v1\"".to_string())),
            no_range: Arc::new(AtomicBool::new(false)),
            fail_range: Arc::new(AtomicBool::new(false)),
            stall_after: Arc::new(AtomicI64::new(0)),
            speed: Arc::new(AtomicI64::new(0)),
            hits: Arc::new(AtomicUsize::new(0)),
            forced_status: Arc::new(AtomicI64::new(0)),
            chunked: Arc::new(AtomicBool::new(false)),
            redirect: Arc::new(Mutex::new(None)),
        };
        let stop = Arc::new(AtomicBool::new(false));

        let st = stop.clone();
        let sh = shared.clone();
        let handle = thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let sh = sh.clone();
                        thread::spawn(move || {
                            let _ = handle_conn(stream, sh);
                        });
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        TestServer { addr, shared, stop, handle: Some(handle) }
    }

    pub fn url(&self) -> String {
        format!("http://{}/file.bin", self.addr)
    }
    pub fn set_data(&self, d: Vec<u8>, etag: &str) {
        *self.shared.data.lock().unwrap() = d;
        *self.shared.etag.lock().unwrap() = etag.to_string();
    }
    pub fn set_no_range(&self, v: bool) {
        self.shared.no_range.store(v, Ordering::SeqCst);
    }
    pub fn set_fail_range(&self, v: bool) {
        self.shared.fail_range.store(v, Ordering::SeqCst);
    }
    pub fn set_stall_after(&self, n: i64) {
        self.shared.stall_after.store(n, Ordering::SeqCst);
    }
    pub fn set_speed(&self, bps: i64) {
        self.shared.speed.store(bps, Ordering::SeqCst);
    }
    /// 强制返回某个状态码（0 = 正常）。
    pub fn set_forced_status(&self, code: u16) {
        self.shared.forced_status.store(code as i64, Ordering::SeqCst);
    }
    /// 用分块传输（Transfer-Encoding: chunked）回包。
    pub fn set_chunked(&self, v: bool) {
        self.shared.chunked.store(v, Ordering::SeqCst);
    }
    /// 让服务器一直回 302 到某个地址。
    pub fn set_redirect(&self, location: Option<String>) {
        *self.shared.redirect.lock().unwrap() = location;
    }
    pub fn hits(&self) -> usize {
        self.shared.hits.load(Ordering::SeqCst)
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

fn handle_conn(mut stream: TcpStream, sh: Shared) -> std::io::Result<()> {
    sh.hits.fetch_add(1, Ordering::SeqCst);

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
            let size = sh.data.lock().unwrap().len() as i64;
            range = parse_range(rest.trim(), size);
        }
    }

    let body = sh.data.lock().unwrap().clone();
    let etag = sh.etag.lock().unwrap().clone();
    let size = body.len() as i64;
    let stall = sh.stall_after.load(Ordering::SeqCst);
    let bps = sh.speed.load(Ordering::SeqCst);
    let chunked = sh.chunked.load(Ordering::SeqCst);

    let forced = sh.forced_status.load(Ordering::SeqCst);
    if forced != 0 {
        let head = format!("HTTP/1.1 {forced} Forced\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        stream.write_all(head.as_bytes())?;
        return Ok(());
    }

    if let Some(loc) = sh.redirect.lock().unwrap().clone() {
        let head = format!(
            "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes())?;
        return Ok(());
    }

    if sh.no_range.load(Ordering::SeqCst) || range.is_none() {
        write_response(&mut stream, &body, 200, None, &etag, size, chunked, stall, bps)?;
        return Ok(());
    }

    let (start, end) = range.unwrap();
    if start > end || start >= size {
        stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }
    let end = end.min(size - 1);
    let report_start =
        if sh.fail_range.load(Ordering::SeqCst) && end - start + 1 > 1 { start + 1 } else { start };
    let cr = format!("bytes {report_start}-{end}/{size}");
    write_response(&mut stream, &body[start as usize..=end as usize], 206, Some(&cr), &etag, size, chunked, stall, bps)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_response(
    stream: &mut TcpStream,
    body: &[u8],
    status: u16,
    content_range: Option<&str>,
    etag: &str,
    full_size: i64,
    chunked: bool,
    stall_after: i64,
    bps: i64,
) -> std::io::Result<()> {
    let reason = if status == 206 { "Partial Content" } else { "OK" };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nETag: {etag}\r\n");
    if status == 206 {
        if let Some(cr) = content_range {
            head.push_str(&format!("Content-Range: {cr}\r\n"));
        }
        head.push_str("Accept-Ranges: bytes\r\n");
    } else {
        head.push_str("Accept-Ranges: none\r\n");
    }
    if chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    let _ = full_size;
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;

    if chunked {
        write_body_chunked(stream, body, stall_after, bps)
    } else {
        write_body(stream, body, stall_after, bps)
    }
}

fn throttle(written: i64, chunk_len: usize, stall_after: i64, bps: i64) -> bool {
    if stall_after > 0 && written >= stall_after {
        thread::sleep(Duration::from_secs(10));
        return true; // 卡住，停止发送
    }
    if bps > 0 {
        thread::sleep(Duration::from_secs_f64(chunk_len as f64 / bps as f64));
    }
    false
}

fn write_body(stream: &mut TcpStream, data: &[u8], stall_after: i64, bps: i64) -> std::io::Result<()> {
    let mut written = 0i64;
    for chunk in data.chunks(32 * 1024) {
        if throttle(written, chunk.len(), stall_after, bps) {
            return Ok(());
        }
        stream.write_all(chunk)?;
        stream.flush()?;
        written += chunk.len() as i64;
    }
    Ok(())
}

fn write_body_chunked(stream: &mut TcpStream, data: &[u8], stall_after: i64, bps: i64) -> std::io::Result<()> {
    let mut written = 0i64;
    for chunk in data.chunks(16 * 1024) {
        if throttle(written, chunk.len(), stall_after, bps) {
            return Ok(()); // 中途停掉：不写结束块，客户端应超时
        }
        stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())?;
        stream.write_all(chunk)?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        written += chunk.len() as i64;
    }
    stream.write_all(b"0\r\n\r\n")?;
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
    if start < 0 {
        return Some((0, end.min(size - 1)));
    }
    Some((start, end.min(size - 1)))
}

pub fn make_data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u64 * 31 + seed as u64 * 7) & 0xff) as u8).collect()
}

pub fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("dlcore-{tag}-{nanos}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}
