//! LTS 网络后端：用现成的 reqwest（含 tokio）实现统一后端接口。
//!
//! 定位与自研后端完全一致（同一套引擎、同一个 C 接口、同样功能），
//! 区别只在"上网这一层"用了成熟框架——稳、边角全，但体积大。
//!
//! 与自研后端平起平坐的几个做法：
//!   · 按来源**缓存并复用 `reqwest::Client`** → 连接池/keep-alive 生效；
//!   · 响应体走 channel **流式**转给阻塞读（去掉"每块 block_on"）；
//!   · **记住跳转后的真实地址**，后面分段直接打过去（省掉每一跳）。

use crate::backend::{
    host_of, port_of, redact_sensitive, same_origin, Backend, Endpoint, ProbeInfo, RangeCheck,
};
use crate::config::Config;
use crate::errors::{fatal, retryable, Result, ERR_RANGE_MISMATCH};
use crate::util::parse_content_range;
use std::collections::HashMap;
use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// 连接超时：与自研后端一致（短超时，避免个别地址连不通时干等）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// 连接池里每条来源最多留多少条空闲长连接。
const POOL_IDLE_SECS: u64 = 90;
/// "跳转后的真实地址"记多久（之后重新解析，避免签名过期）。
const FINAL_URL_TTL: Duration = Duration::from_secs(300);
/// 跳转缓存最多多少条；超过就清理过期项/整体清空（防跨大量 URL 无界增长）。
const MAX_FINAL_CACHE: usize = 256;
/// 客户端缓存最多多少个来源键；超过就整体清空（防跨大量主机长期运行无界增长）。
const MAX_CLIENT_KEYS: usize = 64;

pub struct LtsBackend {
    rt: Arc<tokio::runtime::Runtime>,
    user_agent: String,
    idle_timeout: Duration,
    max_threads: usize,
    /// 按 "host|ip" 缓存客户端：连同一个来源的请求复用同一个连接池。
    clients: Mutex<HashMap<String, reqwest::Client>>,
    /// 跳转缓存：原始地址 -> 跳转后的真实地址（省掉每分段的一跳）。
    final_cache: Mutex<HashMap<String, (String, Instant)>>,
    // 诊断计数
    stat_final_hits: AtomicUsize,
    stat_client_builds: AtomicUsize,
}

impl LtsBackend {
    pub fn new(cfg: &Config) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cfg.max_threads.clamp(4, 64))
            .enable_all()
            .build()
            .map_err(|e| fatal("runtime", format!("创建 tokio 运行时失败: {e}")))?;
        Ok(LtsBackend {
            rt: Arc::new(rt),
            user_agent: cfg.user_agent.clone(),
            idle_timeout: cfg.idle_timeout,
            max_threads: cfg.max_threads.max(1),
            clients: Mutex::new(HashMap::new()),
            final_cache: Mutex::new(HashMap::new()),
            stat_final_hits: AtomicUsize::new(0),
            stat_client_builds: AtomicUsize::new(0),
        })
    }

    fn client_for(&self, url: &str) -> Result<reqwest::Client> {
        let host = host_of(url).unwrap_or_default();
        let scheme = if url.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://")) {
            "https"
        } else {
            "http"
        };
        let key = format!("{scheme}://{host}:{}", port_of(url));
        if let Some(c) = self.clients.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Ok(c.clone());
        }

        let b = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(self.idle_timeout)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                // 拒绝从 HTTPS 降级到 HTTP
                if let Some(prev) = attempt.previous().last() {
                    if prev.scheme() == "https" && attempt.url().scheme() == "http" {
                        return attempt.error("拒绝从 HTTPS 降级到 HTTP");
                    }
                }
                attempt.follow()
            }))
            .pool_max_idle_per_host(self.max_threads)
            .pool_idle_timeout(Duration::from_secs(POOL_IDLE_SECS))
            .http1_only()
            // 不使用系统代理：代理属于宿主/系统的设置，应由宿主显式决定
            .no_proxy();
        let client =
            b.build().map_err(|e| fatal("http", format!("创建 HTTP 客户端失败: {e}")))?;
        let mut m = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() > MAX_CLIENT_KEYS {
            m.clear(); // 键数超限：整体清空（连同其连接池一起释放）
        }
        m.insert(key, client.clone());
        self.stat_client_builds.fetch_add(1, Ordering::Relaxed);
        Ok(client)
    }

    fn cached_final(&self, key: &str) -> Option<String> {
        let m = self.final_cache.lock().unwrap_or_else(|e| e.into_inner());
        m.get(key)
            .and_then(|(u, t)| if t.elapsed() < FINAL_URL_TTL { Some(u.clone()) } else { None })
    }

    fn clear_final(&self, key: &str) {
        self.final_cache.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    /// 发一次 GET（带可选 Range）。优先走上次跳转后的真实地址。
    fn get(
        &self,
        ep: &Endpoint,
        headers: &[(String, String)],
        range: Option<(i64, i64)>,
    ) -> Result<reqwest::Response> {
        let ck = cache_key(&ep.url);
        let cached = self.cached_final(&ck);
        let (url, send_headers) = match &cached {
            Some(f) if f.as_str() != ep.url => {
                self.stat_final_hits.fetch_add(1, Ordering::Relaxed);
                let same = same_origin(&ep.url, f);
                // 跨域：别把 Authorization/Cookie 发到跳转后的主机
                let hdrs = if same { headers.to_vec() } else { redact_sensitive(headers) };
                (f.clone(), hdrs)
            }
            _ => (ep.url.clone(), headers.to_vec()),
        };
        let client = self.client_for(&url)?;
        let ua = self.user_agent.clone();
        let resp = self
            .rt
            .block_on(async move {
                let mut rb = client
                    .get(&url)
                    .header("Accept-Encoding", "identity")
                    .header("User-Agent", ua);
                if let Some((a, b)) = range {
                    rb = rb.header("Range", format!("bytes={a}-{b}"));
                }
                for (k, v) in &send_headers {
                    rb = rb.header(k, v);
                }
                rb.send().await
            })
            .map_err(|e| retryable("send", format!("{e}")))?;

        // 记下"跳转后的真实地址"，供后续分段直接使用
        let final_url = resp.url().as_str();
        if final_url != ep.url {
            let mut m = self.final_cache.lock().unwrap_or_else(|e| e.into_inner());
            if m.len() > MAX_FINAL_CACHE {
                m.retain(|_, (_, t)| t.elapsed() < FINAL_URL_TTL);
                if m.len() > MAX_FINAL_CACHE {
                    m.clear();
                }
            }
            m.insert(ck, (final_url.to_string(), Instant::now()));
        }
        Ok(resp)
    }

    /// 把响应体交给运行时异步读取，再经 channel 流式转给阻塞读（有背压）。
    fn body(&self, resp: reqwest::Response) -> LtsBody {
        let (tx, rx) = mpsc::channel::<io::Result<Vec<u8>>>(8);
        self.rt.spawn(async move {
            let mut resp = resp;
            loop {
                match resp.chunk().await {
                    Ok(Some(b)) => {
                        if tx.send(Ok(b.to_vec())).await.is_err() {
                            return; // 读端已关闭
                        }
                    }
                    Ok(None) => return, // 正常结束 → 丢掉 tx，读端收到"结束"
                    Err(e) => {
                        let _ = tx.send(Err(reqwest_to_io(e))).await;
                        return;
                    }
                }
            }
        });
        LtsBody { rx, buf: Vec::new(), off: 0 }
    }
}

/// 跳转缓存键。
fn cache_key(url: &str) -> String {
    url.to_string()
}

fn reqwest_to_io(e: reqwest::Error) -> io::Error {
    let kind = if e.is_timeout() { io::ErrorKind::TimedOut } else { io::ErrorKind::Other };
    io::Error::new(kind, e.to_string())
}

fn header_str(resp: &reqwest::Response, name: &str) -> String {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

impl Backend for LtsBackend {
    fn name(&self) -> &'static str {
        "lts"
    }

    fn probe(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<ProbeInfo> {
        let mut resp = self.get(ep, headers, Some((0, 0)))?;
        if resp.status().as_u16() == 416 {
            // 个别服务器对 "bytes=0-0" 回 416：退回不带 Range 再探一次
            resp = self.get(ep, headers, None)?;
        }

        let status = resp.status().as_u16();
        let etag = header_str(&resp, "etag");
        let last_modified = header_str(&resp, "last-modified");
        let clen = resp.content_length().map(|v| v.min(i64::MAX as u64) as i64).unwrap_or(0);
        let content_range = header_str(&resp, "content-range");
        let accept_ranges = header_str(&resp, "accept-ranges");
        drop(resp);

        let mut info = ProbeInfo { size: 0, range_ok: false, etag, last_modified };
        if status == 206 {
            if let Some((start, _end, total)) = parse_content_range(&content_range) {
                // 只有总长已知（非 `*`）才敢定 size
                if start == 0 && total > 0 {
                    info.range_ok = true;
                    info.size = total;
                }
            }
        } else if (200..300).contains(&status) {
            info.range_ok = false;
            info.size = clen;
        } else {
            self.clear_final(&cache_key(&ep.url)); // 缓存可能失效，下次重新解析
            return Err(fatal("probe", format!("服务器返回状态 {status}")));
        }
        if accept_ranges.eq_ignore_ascii_case("none") {
            info.range_ok = false;
        }
        Ok(info)
    }

    fn open_range(
        &self,
        ep: &Endpoint,
        headers: &[(String, String)],
        from: i64,
        to: i64,
        expect: &RangeCheck,
    ) -> Result<Box<dyn Read + Send>> {
        let resp = self.get(ep, headers, Some((from, to)))?;

        let status = resp.status().as_u16();
        if status != 206 {
            self.clear_final(&cache_key(&ep.url));
            return Err(retryable("range", format!("{ERR_RANGE_MISMATCH}: 期望 206，实际 {status}")));
        }
        let cr = header_str(&resp, "content-range");
        let (start, _end, total) = match parse_content_range(&cr) {
            Some(x) => x,
            None => {
                self.clear_final(&cache_key(&ep.url));
                return Err(retryable("range", format!("{ERR_RANGE_MISMATCH}: Content-Range 缺失")));
            }
        };
        if start != from {
            self.clear_final(&cache_key(&ep.url));
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: 期望起点 {from}，服务器给了 {start}"),
            ));
        }
        // 复核总长/验证器：堵"诚实服务器中途换了内容或换了个来源"
        if expect.total > 0 && total > 0 && total != expect.total {
            self.clear_final(&cache_key(&ep.url));
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: 总长变了（探路 {}，现在 {total}）", expect.total),
            ));
        }
        let re = header_str(&resp, "etag");
        let rl = header_str(&resp, "last-modified");
        // 强 ETag 优先且排他；响应带了验证器就必须与期望相容
        let expect_has = !expect.etag.is_empty() || !expect.last_modified.is_empty();
        if expect_has
            && (!re.is_empty() || !rl.is_empty())
            && !crate::backend::validators_compatible(expect.etag, expect.last_modified, &re, &rl)
        {
            self.clear_final(&cache_key(&ep.url));
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: 验证器变了（ETag '{re}' / Last-Modified '{rl}'）"),
            ));
        }
        Ok(Box::new(self.body(resp)))
    }

    fn open_plain(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<Box<dyn Read + Send>> {
        let resp = self.get(ep, headers, None)?;
        let status = resp.status().as_u16();
        // 204/205 是"无内容"，不能当成功空文件
        if !(200..300).contains(&status) || status == 204 || status == 205 {
            self.clear_final(&cache_key(&ep.url));
            return Err(retryable("whole", format!("服务器返回状态 {status}")));
        }
        Ok(Box::new(self.body(resp)))
    }

    fn stats(&self) -> String {
        format!(
            "lts 新建Client={} 跳转缓存命中={}",
            self.stat_client_builds.load(Ordering::Relaxed),
            self.stat_final_hits.load(Ordering::Relaxed),
        )
    }
}

/// 阻塞式响应体：从 channel 收数据；channel 关闭即"结束"。
struct LtsBody {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    off: usize,
}

impl Read for LtsBody {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.off >= self.buf.len() {
            match self.rx.blocking_recv() {
                Some(Ok(b)) => {
                    self.buf = b;
                    self.off = 0;
                    if self.buf.is_empty() {
                        return Ok(0);
                    }
                }
                Some(Err(e)) => return Err(e),
                None => return Ok(0), // channel 关闭：读完
            }
        }
        let n = (self.buf.len() - self.off).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.off..self.off + n]);
        self.off += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Callbacks, Config, Engine, Request};
    use std::io::{BufRead, BufReader, Write};

    /// 宿主若在异步运行时里调用，也不能 panic（LTS 后端内部用 block_on）。
    #[test]
    fn download_inside_tokio_context_does_not_panic() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                if let Ok(mut s) = s {
                    std::thread::spawn(move || {
                        let mut r = BufReader::new(s.try_clone().unwrap());
                        loop {
                            let mut l = String::new();
                            if r.read_line(&mut l).unwrap() == 0 || l == "\r\n" {
                                break;
                            }
                        }
                        let body = vec![7u8; 1 << 20];
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        );
                        let _ = s.write_all(&body);
                    });
                }
            }
        });

        let dir = std::env::temp_dir().join(format!("dlcore-async-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("o.bin");
        let mut cfg = Config::default();
        cfg.temp_dir = dir.join("t");
        cfg.initial_threads = 1;
        cfg.max_threads = 2;
        let engine = Engine::new(cfg);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(async {
            engine.download(
                Request {
                    url: format!("http://{addr}/f"),
                    target_file: Some(out.display().to_string()),
                    ..Default::default()
                },
                Callbacks::default(),
            )
        });
        assert!(res.is_ok(), "异步上下文里调用不应 panic: {:?}", res.err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
