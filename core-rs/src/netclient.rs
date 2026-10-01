//! 自研网络层（“正式版”后端）：**不依赖 reqwest / tokio**，只用标准库 + 系统 TLS。
//!
//! 引擎需要的三件事，全在这里：
//!   · `probe`      —— 问服务器"文件多大、能不能分段、身份证（ETag）"
//!   · `open_range` —— 打开某一段的字节流，并"对暗号"（起点必须一致）
//!   · `open_plain` —— 整文件不分段的字节流（服务器不支持分段时兜底）
//!
//! 设计取舍（为了"小 + 可控"）：
//!   · **阻塞式**：一条连接一个线程，天然吻合"每段一个工人"的模型，甩掉异步运行时；
//!   · **系统 TLS**：Windows 走 Schannel（系统自带），不自己实现加密；
//!   · **用后即关**（Connection: close）：逻辑简单；引擎本身"每段一条长连接"，不需要连接池；
//!   · **卡住检测更准**：直接设套接字读超时，一次 read 超时即可判定（框架做不到这么细）。
//!
//! 这里只负责"把 HTTP 说明白"；分段、续传、看门狗、写文件等仍由引擎负责。

use crate::errors::{fatal, retryable, Result, ERR_RANGE_MISMATCH};
use crate::util::{parse_content_range, parse_filename};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

const MAX_REDIRECTS: usize = 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEADERS: usize = 500;

/// 探路结果（与框架版保持同一形状，便于两版后端接口统一）。
#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub size: i64,
    pub range_ok: bool,
    pub etag: String,
    pub last_modified: String,
    pub file_name: String,
}

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

/// 自研 HTTP 客户端。无状态（没有连接池），可自由跨线程共享（`&self` 即可）。
pub struct NetClient {
    user_agent: String,
    idle_timeout: Duration,
}

impl NetClient {
    pub fn new(user_agent: impl Into<String>, idle_timeout: Duration) -> Self {
        NetClient { user_agent: user_agent.into(), idle_timeout }
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

    /// 发一次请求（含跟随跳转），返回最终响应的状态码、响应头和响应体读取器。
    fn request(
        &self,
        url: &str,
        pin: Option<IpAddr>,
        headers: &[(String, String)],
        range: Option<(i64, i64)>,
    ) -> Result<(u16, Vec<(String, String)>, Body)> {
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let u = parse_url(&current)?;
            let mut stream = BufReader::new(self.connect(&u, pin)?);
            let req = build_request(&u, &self.user_agent, headers, range);
            stream
                .get_mut()
                .write_all(req.as_bytes())
                .map_err(|e| map_io("request", e))?;
            stream.get_mut().flush().map_err(|e| map_io("request", e))?;

            let status = read_status_line(&mut stream)?;
            let hdrs = read_headers(&mut stream)?;

            let loc = header_get(&hdrs, "location");
            if matches!(status, 301 | 302 | 303 | 307 | 308) && !loc.is_empty() {
                let next = resolve(&u, loc);
                drop(stream); // 关掉旧连接
                current = next;
                continue;
            }
            let body = Body::new(stream, &hdrs);
            return Ok((status, hdrs, body));
        }
        Err(retryable("redirect", "跳转次数过多"))
    }

    fn connect(&self, u: &ParsedUrl, pin: Option<IpAddr>) -> Result<Stream> {
        let addrs: Vec<SocketAddr> = match pin {
            Some(ip) => vec![SocketAddr::new(ip, u.port)],
            None => (u.host.as_str(), u.port)
                .to_socket_addrs()
                .map_err(|e| fatal("connect", format!("解析主机失败 {}: {e}", u.host)))?
                .collect(),
        };
        if addrs.is_empty() {
            return Err(fatal("connect", format!("找不到主机 {}", u.host)));
        }

        let mut tcp = None;
        let mut last = String::new();
        for a in &addrs {
            match TcpStream::connect_timeout(a, CONNECT_TIMEOUT) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = e.to_string(),
            }
        }
        let tcp = tcp.ok_or_else(|| retryable("connect", format!("连接 {} 失败: {last}", u.host)))?;
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(self.idle_timeout));
        let _ = tcp.set_write_timeout(Some(WRITE_TIMEOUT));

        if u.https {
            let connector = native_tls::TlsConnector::new()
                .map_err(|e| fatal("tls", format!("初始化系统 TLS 失败: {e}")))?;
            let tls = connector
                .connect(&u.host, tcp)
                .map_err(|e| retryable("tls", format!("TLS 握手失败: {e}")))?;
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
pub struct Body {
    inner: BufReader<Stream>,
    mode: Mode,
}

impl Body {
    fn new(inner: BufReader<Stream>, headers: &[(String, String)]) -> Body {
        let te = header_get(headers, "transfer-encoding").to_ascii_lowercase();
        let mode = if te.contains("chunked") {
            Mode::Chunked { remaining: 0, done: false, need_crlf: false }
        } else if let Ok(n) = header_get(headers, "content-length").trim().parse::<u64>() {
            Mode::Length(n)
        } else {
            Mode::Eof
        };
        Body { inner, mode }
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut mode = std::mem::replace(&mut self.mode, Mode::Eof);
        let r = read_mode(&mut self.inner, &mut mode, buf);
        self.mode = mode;
        r
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
    s.push_str("Connection: close\r\n");

    for (k, v) in headers {
        // host/range 由我们自己控制，避免重复或误覆盖
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
