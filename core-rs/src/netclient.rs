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

use crate::backend::{redact_sensitive, same_origin, Backend, Endpoint, RangeCheck};
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
/// 单次连接最多并发尝试几个地址（防止起一堆线程）。
const MAX_CONNECT_ADDRS: usize = 4;
/// 某个地址连接失败后，进"冷宫"多久（期间不再优先尝试它）。
const BAD_ADDR_COOLDOWN: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEADERS: usize = 500;
/// 单次请求最多容忍多少个 1xx 临时响应
const MAX_INTERIM_RESPONSES: usize = 10;
/// chunked 里连续空行 / trailer 行数上限（防坏服务器持续发控制行让读永不返回）
const MAX_CHUNK_EMPTY_LINES: usize = 64;
const MAX_CHUNK_TRAILERS: usize = 1024;
/// 每个来源最多缓存多少条空闲长连接（对齐最大并发，避免"用完就关、下次重连"）
const POOL_MAX_IDLE: usize = 32;
/// 连接池最多记多少个"来源键"；超过就清理过期项（防跨大量主机长期运行无界增长）
const MAX_POOL_KEYS: usize = 64;
/// 跳转缓存最多多少条；超过就清理过期项/整体清空
const MAX_FINAL_CACHE: usize = 256;
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
        // 读超时为 0 会让 set_read_timeout 静默失败 → 卡死；这里兜底成默认 15s
        let idle_timeout = if idle_timeout.is_zero() {
            Duration::from_secs(15)
        } else {
            idle_timeout
        };
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
        let (mut status, mut hdrs, _body) = self.request(&t.url, t.ip, headers, Some((0, 0)))?;
        if status == 416 {
            // 个别服务器对 "bytes=0-0" 回 416：退回不带 Range 再探一次
            let (s2, h2, b2) = self.request(&t.url, t.ip, headers, None)?;
            status = s2;
            hdrs = h2;
            drop(b2);
        }

        let etag = header_get(&hdrs, "etag").to_string();
        let last_modified = header_get(&hdrs, "last-modified").to_string();
        let file_name = parse_filename(header_get(&hdrs, "content-disposition"));
        let content_range = header_get(&hdrs, "content-range").to_string();
        let accept_ranges = header_get(&hdrs, "accept-ranges").to_string();
        let clen = header_get(&hdrs, "content-length").trim().parse::<i64>().unwrap_or(0);

        let mut info = ProbeInfo { size: 0, range_ok: false, etag, last_modified, file_name };
        if status == 206 {
            if let Some((start, _end, total)) = parse_content_range(&content_range) {
                // 只有总长已知（非 `*`）才敢定 size；`bytes 0-0/*` 时 size 未知，交给整文件模式
                if start == 0 && total > 0 {
                    info.range_ok = true;
                    info.size = total;
                }
            }
        } else if (200..300).contains(&status) {
            info.range_ok = false;
            info.size = clen;
        } else {
            self.clear_final(&cache_key(&t.url, t.ip));
            return Err(fatal("probe", format!("服务器返回状态 {status}")));
        }
        if accept_ranges.eq_ignore_ascii_case("none") {
            info.range_ok = false;
        }
        Ok(info)
    }

    /// 打开某一段并"对暗号"：状态必须 206，起点必须等于 from；再复核 `expect` 的总长/验证器。
    pub fn open_range(
        &self,
        t: &Target,
        headers: &[(String, String)],
        from: i64,
        to: i64,
        expect: &RangeCheck,
    ) -> Result<Body> {
        let (status, hdrs, body) = self.request(&t.url, t.ip, headers, Some((from, to)))?;
        if status != 206 {
            self.clear_final(&cache_key(&t.url, t.ip));
            return Err(retryable("range", format!("{ERR_RANGE_MISMATCH}: 期望 206，实际 {status}")));
        }
        let cr = header_get(&hdrs, "content-range");
        let (start, _end, total) = match parse_content_range(cr) {
            Some(x) => x,
            None => {
                self.clear_final(&cache_key(&t.url, t.ip));
                return Err(retryable("range", format!("{ERR_RANGE_MISMATCH}: Content-Range 缺失")));
            }
        };
        if start != from {
            self.clear_final(&cache_key(&t.url, t.ip));
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: 期望起点 {from}，服务器给了 {start}"),
            ));
        }
        // 复核总长/验证器：堵"诚实服务器中途换了内容或换了个来源"
        if expect.total > 0 && total > 0 && total != expect.total {
            self.clear_final(&cache_key(&t.url, t.ip));
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: 总长变了（探路 {}，现在 {total}）", expect.total),
            ));
        }
        let re = header_get(&hdrs, "etag");
        if !expect.etag.is_empty() && !re.is_empty() && !re.eq_ignore_ascii_case(expect.etag) {
            self.clear_final(&cache_key(&t.url, t.ip));
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: ETag 变了（探路 {}，现在 {re}）", expect.etag),
            ));
        }
        let rl = header_get(&hdrs, "last-modified");
        if !expect.last_modified.is_empty() && !rl.is_empty() && rl != expect.last_modified {
            self.clear_final(&cache_key(&t.url, t.ip));
            return Err(retryable("range", format!("{ERR_RANGE_MISMATCH}: Last-Modified 变了")));
        }
        Ok(body)
    }

    /// 整文件 GET（不带 Range），用于"服务器不支持分段"时的单线程兜底。
    pub fn open_plain(&self, t: &Target, headers: &[(String, String)]) -> Result<Body> {
        let (status, _hdrs, body) = self.request(&t.url, t.ip, headers, None)?;
        // 204/205 是"无内容"，不能当成功空文件
        if !(200..300).contains(&status) || status == 204 || status == 205 {
            self.clear_final(&cache_key(&t.url, t.ip));
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
        let ck = cache_key(url, pin);
        if let Some(final_url) = self.cached_final(&ck) {
            if final_url != url {
                self.stat_final_hits.fetch_add(1, Ordering::Relaxed);
                let same = same_origin(url, &final_url);
                let hop_pin = if same { pin } else { None };
                // 跨域：别把 Authorization/Cookie 发到跳转后的主机
                let hdrs: Vec<(String, String)> =
                    if same { headers.to_vec() } else { redact_sensitive(headers) };
                if let Ok((status, hdrs_r, body, _)) =
                    self.request_follow(&final_url, hop_pin, &hdrs, range)
                {
                    if (200..400).contains(&status) {
                        return Ok((status, hdrs_r, body));
                    }
                }
                // 缓存失效（签名过期等）：清掉，回退原始地址重新跳转
                self.clear_final(&ck);
            }
        }
        let (status, hdrs, body, final_url) = self.request_follow(url, pin, headers, range)?;
        if final_url != url {
            let mut m = self.final_cache.lock().unwrap_or_else(|e| e.into_inner());
            if m.len() > MAX_FINAL_CACHE {
                m.retain(|_, (_, t)| t.elapsed() < FINAL_URL_TTL);
                if m.len() > MAX_FINAL_CACHE {
                    m.clear();
                }
            }
            m.insert(ck, (final_url, Instant::now()));
        }
        Ok((status, hdrs, body))
    }

    /// 取一条仍然新鲜的"真实地址"缓存。
    fn cached_final(&self, key: &str) -> Option<String> {
        let m = self.final_cache.lock().unwrap_or_else(|e| e.into_inner());
        m.get(key)
            .and_then(|(u, t)| if t.elapsed() < FINAL_URL_TTL { Some(u.clone()) } else { None })
    }

    /// 清掉"跳转后真实地址"缓存（缓存地址失效时用）。
    fn clear_final(&self, key: &str) {
        self.final_cache.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    /// 跟随跳转发送请求；返回最终地址。
    fn request_follow(
        &self,
        url: &str,
        pin: Option<IpAddr>,
        headers: &[(String, String)],
        range: Option<(i64, i64)>,
    ) -> Result<(u16, Vec<(String, String)>, Body, String)> {
        // 原始地址（用于判断"是否跨域"）：跨域就剥敏感头、也别再绑原主机的 IP
        let base = parse_url(url)?;
        let redacted = redact_sensitive(headers);
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let u = parse_url(&current)?;
            let same = u.https == base.https && u.host == base.host && u.port == base.port;
            let hop_pin = if same { pin } else { None };
            let send_headers: &[(String, String)] = if same { headers } else { &redacted };
            let key = conn_key(&u, hop_pin);

            // 复用连接可能已被对端关掉：失败一次就换新连接重试（最多两次）
            let mut got = None;
            for attempt in 0..2 {
                let (mut reader, reused) = self.take_conn(&key, &u, hop_pin)?;
                match self.exchange(&mut reader, &u, send_headers, range) {
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
                // 拒绝从 HTTPS 降级到 HTTP（内容会被明文传输、可被篡改）
                if u.https && parse_url(&next).map(|nu| !nu.https).unwrap_or(false) {
                    return Err(fatal("redirect", "拒绝从 HTTPS 降级到 HTTP"));
                }
                self.stat_follows.fetch_add(1, Ordering::Relaxed);
                // 跳转用的连接也回收：否则每个分段都要重新和"跳转服务器"握手
                self.recycle_conn(reader, &key, &hdrs, status);
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
            .map_err(|e| map_io("send", e))?;
        reader.get_mut().flush().map_err(|e| map_io("send", e))?;
        // 跳过 1xx 临时响应（103 Early Hints 等），一直读到最终响应
        let mut interim = 0usize;
        loop {
            let status = read_status_line(reader)?;
            let hdrs = read_headers(reader)?;
            if (100..200).contains(&status) && status != 101 {
                interim += 1;
                if interim > MAX_INTERIM_RESPONSES {
                    return Err(retryable("response", "1xx 临时响应过多"));
                }
                continue;
            }
            return Ok((status, hdrs));
        }
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
    fn recycle_conn(&self, reader: BufReader<Stream>, key: &str, hdrs: &[(String, String)], status: u16) {
        if !wants_keep_alive(hdrs) {
            return;
        }
        let te_chunked =
            header_get(hdrs, "transfer-encoding").to_ascii_lowercase().contains("chunked");
        let has_cl = hdrs.iter().any(|(k, _)| k == "content-length");
        let clen = header_get(hdrs, "content-length").trim().parse::<u64>().unwrap_or(0);
        // 只有"确定没有响应体"才敢复用：204/304/1xx，或显式 Content-Length: 0。
        // 缺 CL 又非 chunked 的响应是"读到连接关闭"定界的，可能带体 → 绝不复用（否则下条响应串味）。
        let no_body = matches!(status, 204 | 304) || (100..200).contains(&status) || (has_cl && clen == 0);
        if te_chunked || !no_body || !reader.buffer().is_empty() {
            return;
        }
        let mut m = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        // 池子键数封顶：跨大量主机长期运行时，防止连接/fd 无界增长
        if m.len() > MAX_POOL_KEYS && !m.contains_key(key) {
            m.retain(|_, v| {
                v.retain(|p| p.idle.elapsed() < POOL_IDLE_MAX);
                !v.is_empty()
            });
        }
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
        cands.truncate(MAX_CONNECT_ADDRS); // 别为了一个连接起一堆线程

        // 把"冷宫"里的地址排到最后；若全在冷宫，则照常逐个尝试（限期已过或都坏）
        let now = Instant::now();
        let ordered: Vec<IpAddr> = {
            let mut bad = self.bad.lock().unwrap_or_else(|e| e.into_inner());
            bad.retain(|_, t| now.duration_since(*t) < BAD_ADDR_COOLDOWN); // 顺手清理过期项
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
        // 键数封顶：跨大量来源长期运行时，防止池键/空闲 socket 无界增长
        if m.len() > MAX_POOL_KEYS && !m.contains_key(&key) {
            m.retain(|_, v| {
                v.retain(|p| p.idle.elapsed() < POOL_IDLE_MAX);
                !v.is_empty()
            });
        }
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
        Mode::Chunked { remaining, done, need_crlf } => {
            let mut empty_lines = 0usize;
            loop {
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
                        // 容忍少量空行；但必须封顶，否则坏服务器持续发空行会让这里永不返回
                        empty_lines += 1;
                        if empty_lines > MAX_CHUNK_EMPTY_LINES {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk 空行过多"));
                        }
                        continue;
                    }
                    let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidData, format!("chunk 大小不合法: {line}"))
                        })?;
                    if size == 0 {
                        // 收尾：读掉 trailer，直到空行（同样要封顶）
                        let mut trailers = 0usize;
                        loop {
                            if read_line(inner)?.is_empty() {
                                break;
                            }
                            trailers += 1;
                            if trailers > MAX_CHUNK_TRAILERS {
                                return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk trailer 过多"));
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
            }
        }
    }
}

// ---------------- 报文读写 ----------------

/// 单行响应头上限，防坏服务器用"永不换行的超长行"把内存撑爆
const MAX_LINE_BYTES: usize = 64 * 1024;
/// 响应头总字节上限
const MAX_HEADER_BYTES: usize = 1024 * 1024;

fn read_line<R: BufRead>(r: &mut R) -> io::Result<String> {
    let mut raw: Vec<u8> = Vec::new();
    loop {
        let available = match r.fill_buf() {
            Ok(b) => b,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            if raw.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "连接提前关闭"));
            }
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            raw.extend_from_slice(&available[..=pos]);
            r.consume(pos + 1);
            break;
        }
        let n = available.len();
        raw.extend_from_slice(available);
        r.consume(n);
        if raw.len() > MAX_LINE_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "响应行过长"));
        }
    }
    while matches!(raw.last().copied(), Some(b'\n') | Some(b'\r')) {
        raw.pop();
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

fn read_status_line<R: BufRead>(r: &mut R) -> Result<u16> {
    let line = read_line(r).map_err(|e| map_io("response", e))?;
    // 用 split_whitespace：容忍 `HTTP/1.1  200` 这种多空格（非合规但常见）
    let code = line.split_whitespace().nth(1).unwrap_or("");
    code.parse::<u16>().map_err(|_| retryable("response", format!("状态行不合法: {line}")))
}

fn read_headers<R: BufRead>(r: &mut R) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut total = 0usize;
    loop {
        let line = read_line(r).map_err(|e| map_io("response", e))?;
        if line.is_empty() {
            break;
        }
        total += line.len() + 2;
        if out.len() >= MAX_HEADERS || total > MAX_HEADER_BYTES {
            return Err(fatal("response", "响应头过多/过大"));
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
    format!(
        "{}://{}:{}|{}",
        if u.https { "https" } else { "http" },
        u.host,
        u.port,
        pin.map(|i| i.to_string()).unwrap_or_default()
    )
}

/// 跳转缓存键：同一个 URL 绑不同 IP 要分开记（否则"多 IP 并行"会被一个跳转键覆盖掉）。
fn cache_key(url: &str, ip: Option<IpAddr>) -> String {
    format!("{url}|{}", ip.map(|i| i.to_string()).unwrap_or_default())
}

fn build_request(
    u: &ParsedUrl,
    user_agent: &str,
    headers: &[(String, String)],
    range: Option<(i64, i64)>,
) -> String {
    let default_port = if u.https { 443 } else { 80 };
    // IPv6 字面量要加方括号，否则 Host 头非法
    let host_disp = if u.host.contains(':') { format!("[{}]", u.host) } else { u.host.clone() };
    let host_header = if u.port == default_port {
        host_disp
    } else {
        format!("{host_disp}:{}", u.port)
    };

    let mut s = String::with_capacity(256);
    s.push_str(&format!("GET {} HTTP/1.1\r\n", u.path_query));
    s.push_str(&format!("Host: {host_header}\r\n"));
    // User-Agent 也要防注入（去掉 CR/LF）
    let ua: String = user_agent.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    s.push_str(&format!("User-Agent: {ua}\r\n"));
    s.push_str("Accept: */*\r\n");
    s.push_str("Accept-Encoding: identity\r\n");

    for (k, v) in headers {
        // host/range/connection/accept-encoding 由我们自己控制，避免重复或误覆盖
        if k.eq_ignore_ascii_case("host")
            || k.eq_ignore_ascii_case("range")
            || k.eq_ignore_ascii_case("connection")
            || k.eq_ignore_ascii_case("accept-encoding")
        {
            continue;
        }
        // 拒绝含 CR/LF 的头（防请求头注入）
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
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

/// 是否含控制字符（CR/LF/NUL 等）——URL 里出现它们会造成请求注入。
fn has_ctl(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

fn parse_url(raw: &str) -> Result<ParsedUrl> {
    let raw = raw.trim();
    // 去掉 fragment（#...）：它不属于请求目标
    let raw = match raw.find('#') {
        Some(i) => &raw[..i],
        None => raw,
    };
    let (scheme, rest) =
        raw.split_once("://").ok_or_else(|| fatal("url", "网址不合法（缺少 ://）"))?;
    let scheme_l = scheme.to_ascii_lowercase();
    let https = match scheme_l.as_str() {
        "http" => false,
        "https" => true,
        other => return Err(fatal("url", format!("不支持的协议: {other}"))),
    };

    // 去掉 userinfo（user:pass@host），但别把路径/查询里的 '@' 误伤
    let rest = match rest.find('@') {
        Some(i) if !rest[..i].contains(['/', '?']) => &rest[i + 1..],
        _ => rest,
    };

    // authority 到第一个 '/' 或 '?' 为止（否则 `http://host?x=1` 会把查询串并进主机名）
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let path = &rest[end..];
    if authority.is_empty() {
        return Err(fatal("url", "网址缺少主机"));
    }
    // 拒绝控制字符：否则 path/query 会被原样拼进请求行，可注入请求头/第二个请求
    if has_ctl(authority) || has_ctl(path) {
        return Err(fatal("url", "网址含非法控制字符（CR/LF 等）"));
    }

    let (host, port) = split_host_port(authority, https)?;
    let path_query = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('?') {
        format!("/{path}") // 只有查询串 → 补上根路径
    } else {
        path.to_string()
    };
    let origin = format!("{scheme_l}://{authority}");
    Ok(ParsedUrl { https, host, port, path_query, origin })
}

fn split_host_port(authority: &str, https: bool) -> Result<(String, u16)> {
    let default_port = if https { 443 } else { 80 };

    // IPv6：[::1]:8443
    if let Some(after) = authority.strip_prefix('[') {
        let close = after
            .find(']')
            .ok_or_else(|| fatal("url", "IPv6 地址缺少 ]"))?;
        let host = &after[..close];
        let rest = &after[close + 1..];
        let port = match rest.strip_prefix(':') {
            Some(p) if !p.is_empty() => {
                p.parse::<u16>().map_err(|_| fatal("url", format!("端口不合法: {p}")))?
            }
            Some(_) => default_port, // "[::1]:" → 取默认
            None if rest.is_empty() => default_port,
            // "[::1]extra" 之类：多余字符必须拒绝，别静默丢弃
            None => return Err(fatal("url", "IPv6 地址后有多余字符")),
        };
        return Ok((host.to_string(), port));
    }

    match authority.rsplit_once(':') {
        Some((h, p)) if p.is_empty() => Ok((h.to_string(), default_port)), // "host:"
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
            let port = p.parse().map_err(|_| fatal("url", format!("端口不合法: {p}")))?;
            Ok((h.to_string(), port))
        }
        // 有 ':' 但不是合法端口（如 "host:p80"）→ 报错，别把 "host:p80" 当主机名
        Some((_, p)) => Err(fatal("url", format!("端口不合法: {p}"))),
        None => Ok((authority.to_string(), default_port)),
    }
}

/// 把 Location 解析成绝对地址（绝对 / 协议相对 / 根相对 / 仅查询 / 目录相对含 `./` `../`）。
fn resolve(base: &ParsedUrl, location: &str) -> String {
    let loc = location.trim();
    if loc.is_empty() {
        return format!("{}{}", base.origin, base.path_query);
    }
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    if let Some(rest) = loc.strip_prefix("//") {
        let scheme = if base.https { "https:" } else { "http:" };
        return format!("{scheme}//{rest}");
    }
    // 仅查询/仅锚点：保留原路径
    if loc.starts_with('?') {
        let p = base.path_query.split(['?', '#']).next().unwrap_or("/");
        return format!("{}{}{}", base.origin, p, loc);
    }
    if loc.starts_with('#') {
        return format!("{}{}", base.origin, base.path_query);
    }
    if loc.starts_with('/') {
        return format!("{}{}", base.origin, remove_dot_segments(loc));
    }
    // 目录相对：以 base 的目录为基，合并并处理 ./ ../
    let base_path = base.path_query.split(['?', '#']).next().unwrap_or("/");
    let dir = match base_path.rfind('/') {
        Some(i) => &base_path[..=i],
        None => "/",
    };
    format!("{}{}", base.origin, merge_path(dir, loc))
}

/// 按 RFC 3986 的“合并路径”语义，把相对引用并到目录上（保留尾部斜杠与空段）。
fn merge_path(dir: &str, rel: &str) -> String {
    let (rel_path, suffix) = match rel.find(['?', '#']) {
        Some(i) => (&rel[..i], &rel[i..]),
        None => (rel, ""),
    };
    let merged = format!("{dir}{rel_path}");
    let out = remove_dot_segments(&merged);
    format!("{out}{suffix}")
}

/// RFC 3986 §5.2.4：去掉路径里的 `.` / `..`，并保留结尾斜杠。
fn remove_dot_segments(path: &str) -> String {
    let mut input = path.to_string();
    let mut output = String::new();
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../") {
            input = rest.to_string();
        } else if let Some(rest) = input.strip_prefix("./") {
            input = rest.to_string();
        } else if let Some(rest) = input.strip_prefix("/./") {
            input = format!("/{rest}");
        } else if input == "/." {
            input = "/".to_string();
        } else if let Some(rest) = input.strip_prefix("/../") {
            input = format!("/{rest}");
            if let Some(i) = output.rfind('/') {
                output.truncate(i);
            }
        } else if input == "/.." {
            input = "/".to_string();
            if let Some(i) = output.rfind('/') {
                output.truncate(i);
            }
        } else if input == "." || input == ".." {
            input.clear();
        } else {
            // 取第一段（到下一个 '/' 为止）
            let start = usize::from(input.starts_with('/'));
            match input[start..].find('/').map(|i| i + start) {
                Some(i) => {
                    output.push_str(&input[..i]);
                    input = input[i..].to_string();
                }
                None => {
                    output.push_str(&input);
                    input.clear();
                }
            }
        }
    }
    output
}

// ---------------- 接入统一后端接口 ----------------

fn to_target(ep: &Endpoint) -> Target {
    Target { url: ep.url.clone(), ip: ep.ip }
}

impl Backend for NetClient {
    fn name(&self) -> &'static str {
        "native"
    }

    fn probe(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<ProbeInfo> {
        NetClient::probe(self, &to_target(ep), headers)
    }

    fn open_range(
        &self,
        ep: &Endpoint,
        headers: &[(String, String)],
        from: i64,
        to: i64,
        expect: &RangeCheck,
    ) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(NetClient::open_range(self, &to_target(ep), headers, from, to, expect)?))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn status(s: &str) -> Result<u16> {
        let mut r = std::io::BufReader::new(std::io::Cursor::new(s.as_bytes().to_vec()));
        read_status_line(&mut r)
    }

    #[test]
    fn status_line_tolerates_extra_spaces() {
        assert_eq!(status("HTTP/1.1  200 OK\r\n").unwrap(), 200);
        assert_eq!(status("HTTP/1.1 206 Partial Content\r\n").unwrap(), 206);
    }

    #[test]
    fn parse_url_handles_query_only_and_fragment() {
        let u = parse_url("http://host?token=1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path_query.as_str()), ("host", 80, "/?token=1"));
        let u = parse_url("http://host/a/b#frag").unwrap();
        assert_eq!(u.path_query, "/a/b");
        let u = parse_url("https://host").unwrap();
        assert_eq!((u.port, u.path_query.as_str()), (443, "/"));
    }

    #[test]
    fn parse_url_rejects_crlf_injection() {
        assert!(parse_url("http://h/a\r\nX-Injected: 1").is_err());
        assert!(parse_url("http://h/a\nX: 1").is_err());
        assert!(parse_url("http://h/ok?a=1").is_ok());
    }

    #[test]
    fn strip_userinfo_removes_credentials() {
        assert_eq!(crate::backend::strip_userinfo("https://u:p@h/f"), "https://h/f");
        assert_eq!(crate::backend::strip_userinfo("http://h/f"), "http://h/f");
        assert_eq!(crate::backend::strip_userinfo("http://h/a@b/c"), "http://h/a@b/c");
        assert_eq!(crate::backend::strip_userinfo("http://h?email=a@b"), "http://h?email=a@b");
        assert_eq!(crate::backend::strip_userinfo("http://h#a@b"), "http://h#a@b");
    }

    #[test]
    fn resolve_cases() {
        let base = parse_url("http://h/a/b/c").unwrap();
        assert_eq!(resolve(&base, "?page=2"), "http://h/a/b/c?page=2");
        assert_eq!(resolve(&base, "../x"), "http://h/a/x");
        assert_eq!(resolve(&base, "./x"), "http://h/a/b/x");
        assert_eq!(resolve(&base, "d"), "http://h/a/b/d");
        assert_eq!(resolve(&base, "/root"), "http://h/root");
        assert_eq!(resolve(&base, "//other/z"), "http://other/z");
        assert_eq!(resolve(&base, "https://x/y"), "https://x/y");
    }

    #[test]
    fn resolve_keeps_trailing_slash_and_empty_segments() {
        let base = parse_url("http://h/a/b/c").unwrap();
        assert_eq!(resolve(&base, "g/"), "http://h/a/b/g/");
        assert_eq!(resolve(&base, "."), "http://h/a/b/");
        assert_eq!(resolve(&base, ".."), "http://h/a/");
        assert_eq!(resolve(&base, "a//b"), "http://h/a/b/a//b");
        assert_eq!(resolve(&base, "/./g"), "http://h/g");
        assert_eq!(resolve(&base, "/../g"), "http://h/g");
    }

    #[test]
    fn ipv6_port_is_validated() {
        assert_eq!(parse_url("https://[::1]:8443/x").unwrap().port, 8443);
        assert_eq!(parse_url("https://[::1]/x").unwrap().port, 443);
        assert!(parse_url("https://[::1]:70000/x").is_err());
        assert!(parse_url("https://[::1]:abc/x").is_err());
    }

    #[test]
    fn host_port_validation() {
        assert!(parse_url("http://host:p80/x").is_err(), "非数字端口应报错");
        assert_eq!(parse_url("http://host:/x").unwrap().port, 80, "空端口取默认");
        assert!(parse_url("http://[::1]extra/x").is_err(), "IPv6 后多余字符应拒绝");
        assert!(parse_url("http://[::1/x").is_err(), "缺 ] 应拒绝");
    }

    #[test]
    fn netclient_normalizes_zero_idle_timeout() {
        let c = NetClient::new("ua", Duration::ZERO);
        assert!(!c.idle_timeout.is_zero());
    }
}
