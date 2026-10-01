//! 自研网络层（“正式版”后端）：**不依赖 reqwest / tokio**，只用标准库 + 系统 TLS。
//!
//! 引擎需要的三件事，全在这里：
//!   · `probe`      —— 问服务器"文件多大、能不能分段、身份证（ETag）"
//!   · `open_range` —— 打开某一段的字节流，并"对暗号"（起点必须一致）
//!   · `open_plain` —— 整文件不分段的字节流（服务器不支持分段时兜底）
//!
//! 设计取舍（为了"小 + 可控 + 不慢"）：
//!   · **阻塞式**：一条连接一个线程，天然吻合"每段一个工人"的模型，甩掉异步运行时；
//!   · **系统 TLS**：Windows 走 Schannel（系统自带），不自己实现加密；
//!   · **keep-alive 连接池**：同来源的连接用完回收、下次复用，避免"每段/每次重试都重新
//!     握手"，高并发下也不至于被丢 SYN；
//!   · **坏地址冷宫**：某 IP 连不通就短期不再优先尝试，并自动改连同域其它 IP；
//!   · **卡住检测更准**：直接设套接字读超时，一次 read 超时即可判定（框架做不到这么细）。
//!
//! 这里只负责"把 HTTP 说明白"；分段、续传、看门狗、写文件等仍由引擎负责。

use crate::backend::{Backend, Endpoint};
use crate::errors::{fatal, retryable, Result, ERR_RANGE_MISMATCH};
use crate::util::{parse_content_range, parse_filename};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_REDIRECTS: usize = 10;
/// 单次连接尝试的超时（按地址算）。短一点：一个地址不通就赶紧换下一个。
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(6);
/// 并发起跑（happy eyeballs）时，相邻地址之间的起跑间隔。
const CONNECT_STAGGER_MS: u64 = 200;
/// 某个地址连接失败后，进"冷宫"多久（期间不再优先尝试它）。
const BAD_ADDR_COOLDOWN: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEADERS: usize = 500;
/// 每个来源最多缓存多少条空闲长连接（对齐最大并发，避免"用完就关、下次重连"）
const POOL_MAX_IDLE: usize = 32;
/// 空闲连接最长留多久
const POOL_IDLE_MAX: Duration = Duration::from_secs(30);
/// "跳转后的真实地址"记多久（之后重新解析，避免签名过期）
const FINAL_URL_TTL: Duration = Duration::from_secs(300);

/// 探路结果：两个后端共用同一形状（定义在 `backend`）。
pub use crate::backend::ProbeInfo;

/// 一个下载目标：网址 +（可选）绑定到某个 IP。
/// 多 IP / 镜像并行时，由上层为每个来源建一个 Target。
#[derive(Clone, Debug)]
pub struct Target {
    pub url: String,
    pub ip: Option<IpAddr>,
}

impl Target {
    pub fn new(url: impl Into<String>) -> Self {
        Target { url: url.into(), ip: None }
    }
    pub fn pinned(url: impl Into<String>, ip: IpAddr) -> Self {
        Target { url: url.into(), ip: Some(ip) }
    }
}

/// 一条回收回来的空闲连接。
struct Pooled {
    reader: BufReader<Stream>,
    idle: Instant,
}

type Pool = Arc<Mutex<HashMap<String, Vec<Pooled>>>>;

/// 自研 HTTP 客户端。带 keep-alive 连接池，可自由跨线程共享（`&self` 即可）。
pub struct NetClient {
    user_agent: String,
    idle_timeout: Duration,
    /// "冷宫"：连接失败过的 IP 及其失败时刻，短时间内不再优先尝试。
    bad: Mutex<HashMap<IpAddr, Instant>>,
    /// keep-alive 连接池：按 "host:port|ip" 复用连接。
    pool: Pool,
    /// 跳转缓存：原始地址 -> 跳转后的真实地址（省掉每分段的一跳）。
    final_cache: Mutex<HashMap<String, (String, Instant)>>,
    /// 系统 TLS 连接器：只建一次，反复用（别每条连接都重建）。
    tls: OnceLock<native_tls::TlsConnector>,
    // 诊断计数
    stat_connects: AtomicUsize,
    stat_tls: AtomicUsize,
    stat_pool_hits: AtomicUsize,
    stat_final_hits: AtomicUsize,
    stat_follows: AtomicUsize,
    stat_conn_fail: AtomicUsize,
}

impl NetClient {
    pub fn new(user_agent: impl Into<String>, idle_timeout: Duration) -> Self {
        NetClient {
            user_agent: user_agent.into(),
            idle_timeout,
            bad: Mutex::new(HashMap::new()),
            pool: Arc::new(Mutex::new(HashMap::new())),
            final_cache: Mutex::new(HashMap::new()),
            tls: OnceLock::new(),
            stat_connects: AtomicUsize::new(0),
            stat_tls: AtomicUsize::new(0),
            stat_pool_hits: AtomicUsize::new(0),
            stat_final_hits: AtomicUsize::new(0),
            stat_follows: AtomicUsize::new(0),
            stat_conn_fail: AtomicUsize::new(0),
        }
    }

    /// 探路。注意：会真的发一个 `Range: bytes=0-0` 的 GET。
    pub fn probe(&self, t: &Target, headers: &[(String, String)]) -> Result<ProbeInfo> {
        let (status, hdrs, _body) = self.request(&t.url, t.ip, headers, Some((0, 0)))?;

        let etag = header_get(&hdrs, "etag").to_string();
        let last_modified = header_get(&hdrs, "last-modified").to_string();
        let file_name = parse_filename(header_get(&hdrs, "content-disposition"));
        let content_range = header_get(&hdrs, "content-range").to_string();
        let accept_ranges = header_get(&hdrs, "accept-ranges").to_string();
        let clen = header_get(&hdrs, "content-length").trim().parse::<i64>().unwrap_or(0);

        let mut info = ProbeInfo { size: 0, range_ok: false, etag, last_modified, file_name };
        if status == 206 {
            if let Some((start, _end, total)) = parse_content_range(&content_range) {
                if start == 0 && total > 0 {
                    info.range_ok = true;
                    info.size = total;
                } else if clen > 0 {
                    info.size = clen;
                }
            }
        } else if (200..300).contains(&status) {
            info.range_ok = false;
            info.size = clen;
        } else {
            return Err(fatal("probe", format!("服务器返回状态 {status}")));
        }
        if accept_ranges.eq_ignore_ascii_case("none") {
            info.range_ok = false;
        }
        Ok(info)
    }

    /// 打开某一段并"对暗号"：状态必须 206，返回起点必须等于 from。
    pub fn open_range(
        &self,
        t: &Target,
        headers: &[(String, String)],
        from: i64,
        to: i64,
    ) -> Result<Body> {
        let (status, hdrs, body) = self.request(&t.url, t.ip, headers, Some((from, to)))?;
        if status != 206 {
            return Err(retryable("range", format!("{ERR_RANGE_MISMATCH}: 期望 206，实际 {status}")));
        }
        let cr = header_get(&hdrs, "content-range");
        let (start, _, _) = parse_content_range(cr)
            .ok_or_else(|| retryable("range", format!("{ERR_RANGE_MISMATCH}: Content-Range 缺失")))?;
        if start != from {
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: 期望起点 {from}，服务器给了 {start}"),
            ));
        }
        Ok(body)
    }

    /// 整文件 GET（不带 Range），用于"服务器不支持分段"时的单线程兜底。
    pub fn open_plain(&self, t: &Target, headers: &[(String, String)]) -> Result<Body> {
        let (status, _hdrs, body) = self.request(&t.url, t.ip, headers, None)?;
        if !(200..300).contains(&status) {
            return Err(retryable("whole", format!("服务器返回状态 {status}")));
        }
        Ok(body)
    }

    /// 发一次请求，返回状态码 / 响应头 / 响应体读取器。
    /// 优先走"上次跳转后的真实地址"（省掉每一跳的分段请求）；失效则回退原始地址。
    fn request(
        &self,
        url: &str,
        pin: Option<IpAddr>,
        headers: &[(String, String)],
        range: Option<(i64, i64)>,
    ) -> Result<(u16, Vec<(String, String)>, Body)> {
        if let Some(final_url) = self.cached_final(url) {
            if final_url != url {
                self.stat_final_hits.fetch_add(1, Ordering::Relaxed);
                if let Ok((status, hdrs, body, _)) =
                    self.request_follow(&final_url, None, headers, range)
                {
                    if (200..400).contains(&status) {
                        return Ok((status, hdrs, body));
                    }
                }
                // 缓存失效（签名过期等）：清掉，回退原始地址重新跳转
                self.final_cache.lock().unwrap_or_else(|e| e.into_inner()).remove(url);
            }
        }
        let (status, hdrs, body, final_url) = self.request_follow(url, pin, headers, range)?;
        if final_url != url {
            let mut m = self.final_cache.lock().unwrap_or_else(|e| e.into_inner());
            m.insert(url.to_string(), (final_url, Instant::now()));
        }
        Ok((status, hdrs, body))
    }

    /// 取一条仍然新鲜的"真实地址"缓存。
    fn cached_final(&self, url: &str) -> Option<String> {
        let m = self.final_cache.lock().unwrap_or_else(|e| e.into_inner());
        m.get(url)
            .and_then(|(u, t)| if t.elapsed() < FINAL_URL_TTL { Some(u.clone()) } else { None })
    }

    /// 跟随跳转发送请求；返回最终地址。
    fn request_follow(
        &self,
        url: &str,
        pin: Option<IpAddr>,
        headers: &[(String, String)],
        range: Option<(i64, i64)>,
    ) -> Result<(u16, Vec<(String, String)>, Body, String)> {
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let u = parse_url(&current)?;
            let key = conn_key(&u, pin);

            // 复用连接可能已被对端关掉：失败一次就换新连接重试（最多两次）
            let mut got = None;
            for attempt in 0..2 {
                let (mut reader, reused) = self.take_conn(&key, &u, pin)?;
                match self.exchange(&mut reader, &u, headers, range) {
                    Ok((status, hdrs)) => {
                        got = Some((reader, status, hdrs));
                        break;
                    }
                    Err(e) => {
                        if reused && attempt == 0 {
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
            let (reader, status, hdrs) =
                got.ok_or_else(|| retryable("request", "请求失败：无法建立连接"))?;

            let loc = header_get(&hdrs, "location");
            if matches!(status, 301 | 302 | 303 | 307 | 308) && !loc.is_empty() {
                let next = resolve(&u, loc);
                self.stat_follows.fetch_add(1, Ordering::Relaxed);
                // 跳转用的连接也回收：否则每个分段都要重新和"跳转服务器"握手
                self.recycle_conn(reader, &key, &hdrs);
                current = next;
                continue;
            }

            let keep = wants_keep_alive(&hdrs);
            let body = Body::new(reader, &hdrs, keep, Some((self.pool.clone(), key)));
            return Ok((status, hdrs, body, current));
        }
        Err(retryable("redirect", "跳转次数过多"))
    }

    /// 写请求 + 读状态行/响应头（连接池复用与新建共用这段）。
    fn exchange(
        &self,
        reader: &mut BufReader<Stream>,
        u: &ParsedUrl,
        headers: &[(String, String)],
        range: Option<(i64, i64)>,
    ) -> Result<(u16, Vec<(String, String)>)> {
        let req = build_request(u, &self.user_agent, headers, range);
        reader
            .get_mut()
            .write_all(req.as_bytes())
            .map_err(|e| map_io("request", e))?;
        reader.get_mut().flush().map_err(|e| map_io("request", e))?;
        let status = read_status_line(reader)?;
        let hdrs = read_headers(reader)?;
        Ok((status, hdrs))
    }

    /// 从池里取一条（仍新鲜）的连接；没有就新建。
    fn take_conn(&self, key: &str, u: &ParsedUrl, pin: Option<IpAddr>) -> Result<(BufReader<Stream>, bool)> {
        {
            let mut m = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = m.get_mut(key) {
                while let Some(p) = v.pop() {
                    if p.idle.elapsed() < POOL_IDLE_MAX {
                        self.stat_pool_hits.fetch_add(1, Ordering::Relaxed);
                        return Ok((p.reader, true));
                    }
                    // 过期的直接丢弃
                }
            }
        }
        Ok((BufReader::new(self.connect(u, pin)?), false))
    }

    /// 把一条（确认没有响应体的）连接放回池里复用。
    fn recycle_conn(&self, reader: BufReader<Stream>, key: &str, hdrs: &[(String, String)]) {
        if !wants_keep_alive(hdrs) {
            return;
        }
        // 有响应体而我们没读 → 不能复用（否则下条响应会串味）
        let te_chunked =
            header_get(hdrs, "transfer-encoding").to_ascii_lowercase().contains("chunked");
        let clen = header_get(hdrs, "content-length").trim().parse::<u64>().unwrap_or(0);
        if te_chunked || clen > 0 || !reader.buffer().is_empty() {
            return;
        }
        let mut m = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let v = m.entry(key.to_string()).or_default();
        if v.len() < POOL_MAX_IDLE {
            v.push(Pooled { reader, idle: Instant::now() });
        }
    }

    fn connect(&self, u: &ParsedUrl, pin: Option<IpAddr>) -> Result<Stream> {
        // 候选地址：优先"绑定的 IP"，然后补上同域名解析出的其它 IP。
        // 这样某个 IP 偶发连不通时，能在本次请求内直接换一个，而不是干等超时。
        let mut cands: Vec<IpAddr> = Vec::new();
        if let Some(ip) = pin {
            cands.push(ip);
        }
        if let Ok(res) = (u.host.as_str(), u.port).to_socket_addrs() {
            for a in res {
                let ip = a.ip();
                if !cands.contains(&ip) {
                    cands.push(ip);
                }
            }
        }
        if cands.is_empty() {
            return Err(fatal("connect", format!("找不到主机 {}", u.host)));
        }

        // 把"冷宫"里的地址排到最后；若全在冷宫，则照常逐个尝试（限期已过或都坏）
        let now = Instant::now();
        let ordered: Vec<IpAddr> = {
            let bad = self.bad.lock().unwrap_or_else(|e| e.into_inner());
            let is_bad = |ip: &IpAddr| {
                bad.get(ip).map(|t| now.duration_since(*t) < BAD_ADDR_COOLDOWN).unwrap_or(false)
            };
            let mut v: Vec<IpAddr> = cands.iter().filter(|ip| !is_bad(ip)).cloned().collect();
            v.extend(cands.iter().filter(|ip| is_bad(ip)).cloned());
            v
        };

        let mut tcp = None;
        let mut last = String::new();
        // 起跑式并发连接（happy eyeballs）：逐个地址晚一点起跑，谁先连上就用谁。
        // 避免"第一个地址不通 → 干等 6 秒再试下一个"。
        let (tx, rx) = std::sync::mpsc::channel::<(IpAddr, std::io::Result<TcpStream>)>();
        let port = u.port;
        for (i, ip) in ordered.iter().enumerate() {
            let tx = tx.clone();
            let ip = *ip;
            let delay = Duration::from_millis(CONNECT_STAGGER_MS * i as u64);
            std::thread::spawn(move || {
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let r = TcpStream::connect_timeout(&SocketAddr::new(ip, port), CONNECT_ATTEMPT_TIMEOUT);
                let _ = tx.send((ip, r));
            });
        }
        drop(tx);
        for _ in 0..ordered.len() {
            match rx.recv() {
                Ok((ip, Ok(s))) => {
                    self.bad.lock().unwrap_or_else(|e| e.into_inner()).remove(&ip);
                    self.stat_connects.fetch_add(1, Ordering::Relaxed);
                    tcp = Some(s);
                    break;
                }
                Ok((ip, Err(e))) => {
                    last = e.to_string();
                    self.stat_conn_fail.fetch_add(1, Ordering::Relaxed);
                    self.bad.lock().unwrap_or_else(|e| e.into_inner()).insert(ip, Instant::now());
                }
                Err(_) => break,
            }
        }
        let tcp = tcp.ok_or_else(|| retryable("connect", format!("连接 {} 失败: {last}", u.host)))?;
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(self.idle_timeout));
        let _ = tcp.set_write_timeout(Some(WRITE_TIMEOUT));

        if u.https {
            let connector = match self.tls.get() {
                Some(c) => c,
                None => {
                    let c = native_tls::TlsConnector::new()
                        .map_err(|e| fatal("tls", format!("初始化系统 TLS 失败: {e}")))?;
                    let _ = self.tls.set(c);
                    self.tls.get().expect("刚设置")
                }
            };
            let tls = connector
                .connect(&u.host, tcp)
                .map_err(|e| retryable("tls", format!("TLS 握手失败: {e}")))?;
            self.stat_tls.fetch_add(1, Ordering::Relaxed);
            Ok(Stream::Tls(Box::new(tls)))
        } else {
            Ok(Stream::Plain(tcp))
        }
    }
}

// ---------------- 传输层 ----------------

enum Stream {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

fn normalize_timeout(e: io::Error) -> io::Error {
    // Windows 的读超时是 WouldBlock；统一成 TimedOut，方便上层识别"卡住"
    if e.kind() == io::ErrorKind::WouldBlock {
        io::Error::new(io::ErrorKind::TimedOut, e)
    } else {
        e
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
        .map_err(normalize_timeout)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

// ---------------- 响应体 ----------------

enum Mode {
    /// 已知长度（Content-Length）
    Length(u64),
    /// 分块传输（Transfer-Encoding: chunked）
    Chunked { remaining: u64, done: bool, need_crlf: bool },
    /// 读到连接关闭为止
    Eof,
}

/// 响应体：实现 `Read`，屏蔽"定长/分块/读到尾"三种情况。
/// 读完且连接可复用时，`Drop` 会把连接还回连接池。
pub struct Body {
    inner: Option<BufReader<Stream>>,
    mode: Mode,
    keep_alive: bool,
    recycle: Option<(Pool, String)>,
}

impl Body {
    fn new(
        inner: BufReader<Stream>,
        headers: &[(String, String)],
        keep_alive: bool,
        recycle: Option<(Pool, String)>,
    ) -> Body {
        let te = header_get(headers, "transfer-encoding").to_ascii_lowercase();
        let mode = if te.contains("chunked") {
            Mode::Chunked { remaining: 0, done: false, need_crlf: false }
        } else if let Ok(n) = header_get(headers, "content-length").trim().parse::<u64>() {
            Mode::Length(n)
        } else {
            Mode::Eof
        };
        Body { inner: Some(inner), mode, keep_alive, recycle }
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut mode = std::mem::replace(&mut self.mode, Mode::Eof);
        let inner = self.inner.as_mut().expect("响应体已释放");
        let r = read_mode(inner, &mut mode, buf);
        self.mode = mode;
        r
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        if !self.keep_alive {
            return;
        }
        // 只有"整段读完"才可复用；另外缓冲区里不能有残留字节（否则会串味）
        let reusable = match &self.mode {
            Mode::Length(n) => *n == 0,
            Mode::Chunked { done, .. } => *done,
            Mode::Eof => false,
        };
        let (pool, key) = match self.recycle.take() {
            Some(x) => x,
            None => return,
        };
        let reader = match self.inner.take() {
            Some(r) => r,
            None => return,
        };
        if !reusable || !reader.buffer().is_empty() {
            return;
        }
        let mut m = pool.lock().unwrap_or_else(|e| e.into_inner());
        let v = m.entry(key).or_default();
        if v.len() < POOL_MAX_IDLE {
            v.push(Pooled { reader, idle: Instant::now() });
        }
    }
}

fn read_mode(inner: &mut BufReader<Stream>, mode: &mut Mode, buf: &mut [u8]) -> io::Result<usize> {
    match mode {
        Mode::Length(n) => {
            if *n == 0 {
                return Ok(0);
            }
            let want = (*n).min(buf.len() as u64) as usize;
            let got = inner.read(&mut buf[..want])?;
            if got == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "响应体提前结束"));
            }
            *n -= got as u64;
            Ok(got)
        }
        Mode::Eof => inner.read(buf),
        Mode::Chunked { remaining, done, need_crlf } => loop {
            if *done {
                return Ok(0);
            }
            if *need_crlf {
                let line = read_line(inner)?;
                *need_crlf = false;
                if !line.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk 后缺少 CRLF"));
                }
            }
            if *remaining == 0 {
                let line = read_line(inner)?;
                if line.is_empty() {
                    continue; // 容忍多余空行
                }
                let size = u64::from_str_radix(line.trim(), 16).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("chunk 大小不合法: {line}"))
                })?;
                if size == 0 {
                    // 收尾：读掉 trailer，直到空行
                    loop {
                        if read_line(inner)?.is_empty() {
                            break;
                        }
                    }
                    *done = true;
                    return Ok(0);
                }
                *remaining = size;
            }
            let want = (*remaining).min(buf.len() as u64) as usize;
            let got = inner.read(&mut buf[..want])?;
            if got == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunk 数据提前结束"));
            }
            *remaining -= got as u64;
            if *remaining == 0 {
                *need_crlf = true;
            }
            return Ok(got);
        },
    }
}

// ---------------- 报文读写 ----------------

fn read_line<R: BufRead>(r: &mut R) -> io::Result<String> {
    let mut raw: Vec<u8> = Vec::new();
    let n = r.read_until(b'\n', &mut raw)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "连接提前关闭"));
    }
    while matches!(raw.last().copied(), Some(b'\n') | Some(b'\r')) {
        raw.pop();
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

fn read_status_line<R: BufRead>(r: &mut R) -> Result<u16> {
    let line = read_line(r).map_err(|e| map_io("response", e))?;
    let code = line.split(' ').nth(1).unwrap_or("");
    code.parse::<u16>().map_err(|_| retryable("response", format!("状态行不合法: {line}")))
}

fn read_headers<R: BufRead>(r: &mut R) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    loop {
        let line = read_line(r).map_err(|e| map_io("response", e))?;
        if line.is_empty() {
            break;
        }
        if out.len() >= MAX_HEADERS {
            return Err(fatal("response", "响应头过多"));
        }
        if let Some(i) = line.find(':') {
            let name = line[..i].trim().to_ascii_lowercase();
            let value = line[i + 1..].trim().to_string();
            out.push((name, value));
        }
    }
    Ok(out)
}

fn header_get<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

/// HTTP/1.1 默认长连接；只有服务器明说 close 才不复用。
fn wants_keep_alive(headers: &[(String, String)]) -> bool {
    !header_get(headers, "connection").to_ascii_lowercase().contains("close")
}

fn conn_key(u: &ParsedUrl, pin: Option<IpAddr>) -> String {
    format!("{}:{}|{}", u.host, u.port, pin.map(|i| i.to_string()).unwrap_or_default())
}

fn build_request(
    u: &ParsedUrl,
    user_agent: &str,
    headers: &[(String, String)],
    range: Option<(i64, i64)>,
) -> String {
    let default_port = if u.https { 443 } else { 80 };
    let host_header = if u.port == default_port {
        u.host.clone()
    } else {
        format!("{}:{}", u.host, u.port)
    };

    let mut s = String::with_capacity(256);
    s.push_str(&format!("GET {} HTTP/1.1\r\n", u.path_query));
    s.push_str(&format!("Host: {host_header}\r\n"));
    s.push_str(&format!("User-Agent: {user_agent}\r\n"));
    s.push_str("Accept: */*\r\n");
    s.push_str("Accept-Encoding: identity\r\n");

    for (k, v) in headers {
        // host/range/connection 由我们自己控制，避免重复或误覆盖
        if k.eq_ignore_ascii_case("host")
            || k.eq_ignore_ascii_case("range")
            || k.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some((a, b)) = range {
        s.push_str(&format!("Range: bytes={a}-{b}\r\n"));
    }
    s.push_str("\r\n");
    s
}

fn map_io(op: &'static str, e: io::Error) -> crate::errors::Error {
    if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock {
        retryable(op, format!("{op} 超时（网络卡住）: {e}"))
    } else {
        retryable(op, format!("{op} 失败: {e}"))
    }
}

// ---------------- URL ----------------

#[derive(Debug, Clone)]
struct ParsedUrl {
    https: bool,
    host: String,
    port: u16,
    /// 以 '/' 开头，含查询串
    path_query: String,
    /// scheme://authority，用于拼相对跳转
    origin: String,
}

fn parse_url(raw: &str) -> Result<ParsedUrl> {
    let raw = raw.trim();
    let (scheme, rest) =
        raw.split_once("://").ok_or_else(|| fatal("url", format!("网址不合法: {raw}")))?;
    let https = match scheme.to_ascii_lowercase().as_str() {
        "http" => false,
        "https" => true,
        other => return Err(fatal("url", format!("不支持的协议: {other}"))),
    };

    // 去掉 userinfo（user:pass@host），但别把路径里的 '@' 误伤
    let rest = match rest.find('@') {
        Some(i) if !rest[..i].contains('/') => &rest[i + 1..],
        _ => rest,
    };

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(fatal("url", format!("网址缺少主机: {raw}")));
    }

    let (host, port) = split_host_port(authority, https)?;
    let path_query = if path.is_empty() { "/".to_string() } else { path.to_string() };
    let origin = format!("{}://{}", if https { "https" } else { "http" }, authority);
    Ok(ParsedUrl { https, host, port, path_query, origin })
}

fn split_host_port(authority: &str, https: bool) -> Result<(String, u16)> {
    let default_port = if https { 443 } else { 80 };

    // IPv6：[::1]:8443
    if let Some(after) = authority.strip_prefix('[') {
        if let Some(close) = after.find(']') {
            let host = &after[..close];
            let rest = &after[close + 1..];
            let port = rest.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(default_port);
            return Ok((host.to_string(), port));
        }
    }

    match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            let port = p.parse().map_err(|_| fatal("url", format!("端口不合法: {p}")))?;
            Ok((h.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// 把 Location 解析成绝对地址（支持绝对 / 协议相对 / 根相对 / 目录相对）。
fn resolve(base: &ParsedUrl, location: &str) -> String {
    let loc = location.trim();
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    if let Some(rest) = loc.strip_prefix("//") {
        let scheme = if base.https { "https:" } else { "http:" };
        return format!("{scheme}//{rest}");
    }
    if loc.starts_with('/') {
        return format!("{}{}", base.origin, loc);
    }
    let dir = match base.path_query.rfind('/') {
        Some(i) => &base.path_query[..=i],
        None => "/",
    };
    format!("{}{}{}", base.origin, dir, loc)
}

// ---------------- 接入统一后端接口 ----------------

fn to_target(ep: &Endpoint) -> Target {
    Target { url: ep.url.clone(), ip: ep.ip }
}

impl Backend for NetClient {
    fn name(&self) -> &'static str {
        "native"
    }

    fn probe(&self, ep: &Endpoint) -> Result<ProbeInfo> {
        NetClient::probe(self, &to_target(ep), &[])
    }

    fn open_range(
        &self,
        ep: &Endpoint,
        headers: &[(String, String)],
        from: i64,
        to: i64,
    ) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(NetClient::open_range(self, &to_target(ep), headers, from, to)?))
    }

    fn open_plain(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(NetClient::open_plain(self, &to_target(ep), headers)?))
    }

    fn stats(&self) -> String {
        format!(
            "native 新建TCP={} TLS握手={} 池复用={} 跳转缓存命中={} 跟随跳转={} 连接失败={}",
            self.stat_connects.load(Ordering::Relaxed),
            self.stat_tls.load(Ordering::Relaxed),
            self.stat_pool_hits.load(Ordering::Relaxed),
            self.stat_final_hits.load(Ordering::Relaxed),
            self.stat_follows.load(Ordering::Relaxed),
            self.stat_conn_fail.load(Ordering::Relaxed),
        )
    }
}
