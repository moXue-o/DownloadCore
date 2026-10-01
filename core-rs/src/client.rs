//! LTS 网络后端：用现成的 reqwest（含 tokio）实现统一后端接口。
//!
//! 定位与自研后端完全一致（同一套引擎、同一个 C 接口、同样功能），
//! 区别只在"上网这一层"用了成熟框架——稳、边角全，但体积大。
//!
//! 与自研后端平起平坐的几个做法：
//!   · 按来源**缓存并复用 `reqwest::Client`** → 连接池/keep-alive 生效；
//!   · 响应体走 channel **流式**转给阻塞读（去掉"每块 block_on"）；
//!   · **记住跳转后的真实地址**，后面分段直接打过去（省掉每一跳）。

use crate::backend::{host_of, port_of, Backend, Endpoint, ProbeInfo};
use crate::config::Config;
use crate::errors::{fatal, retryable, Result, ERR_RANGE_MISMATCH};
use crate::util::{parse_content_range, parse_filename};
use std::collections::HashMap;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// 连接超时：与自研后端一致（短超时，避免个别地址连不通时干等）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// 连接池里每条来源最多留多少条空闲长连接。
const POOL_IDLE_SECS: u64 = 90;
/// "跳转后的真实地址"记多久（之后重新解析，避免签名过期）。
const FINAL_URL_TTL: Duration = Duration::from_secs(300);

pub struct LtsBackend {
    rt: Arc<tokio::runtime::Runtime>,
    user_agent: String,
    idle_timeout: Duration,
    max_threads: usize,
    /// 按 "host|ip" 缓存客户端：连同一个来源的请求复用同一个连接池。
    clients: Mutex<HashMap<String, reqwest::Client>>,
    /// 跳转缓存：原始地址 -> 跳转后的真实地址（省掉每分段的一跳）。
    final_cache: Mutex<HashMap<String, (String, Instant)>>,
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
        })
    }

    fn client_for(&self, url: &str, ip: Option<IpAddr>) -> Result<reqwest::Client> {
        let host = host_of(url).unwrap_or_default();
        let key = format!("{host}|{}", ip.map(|i| i.to_string()).unwrap_or_default());
        if let Some(c) = self.clients.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Ok(c.clone());
        }

        let mut b = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(self.idle_timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .pool_max_idle_per_host(self.max_threads)
            .pool_idle_timeout(Duration::from_secs(POOL_IDLE_SECS))
            .http1_only()
            // 不使用系统代理：代理属于宿主/系统的设置，应由宿主显式决定
            .no_proxy();
        if let Some(ip) = ip {
            if !host.is_empty() {
                b = b.resolve(&host, SocketAddr::new(ip, port_of(url)));
            }
        }
        let client =
            b.build().map_err(|e| fatal("http", format!("创建 HTTP 客户端失败: {e}")))?;
        self.clients.lock().unwrap_or_else(|e| e.into_inner()).insert(key, client.clone());
        Ok(client)
    }

    fn cached_final(&self, url: &str) -> Option<String> {
        let m = self.final_cache.lock().unwrap_or_else(|e| e.into_inner());
        m.get(url)
            .and_then(|(u, t)| if t.elapsed() < FINAL_URL_TTL { Some(u.clone()) } else { None })
    }

    fn clear_final(&self, url: &str) {
        self.final_cache.lock().unwrap_or_else(|e| e.into_inner()).remove(url);
    }

    /// 发一次 GET（带可选 Range）。优先走上次跳转后的真实地址。
    fn get(
        &self,
        ep: &Endpoint,
        headers: &[(String, String)],
        range: Option<(i64, i64)>,
    ) -> Result<reqwest::Response> {
        let (url, ip) = match self.cached_final(&ep.url) {
            Some(f) if f != ep.url => (f, None), // 用真实地址，不再 pin 原始主机
            _ => (ep.url.clone(), ep.ip),
        };
        let client = self.client_for(&url, ip)?;
        let ua = self.user_agent.clone();
        let headers = headers.to_vec();
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
                for (k, v) in &headers {
                    rb = rb.header(k, v);
                }
                rb.send().await
            })
            .map_err(|e| retryable("request", format!("{e}")))?;

        // 记下"跳转后的真实地址"，供后续分段直接使用
        let final_url = resp.url().as_str();
        if final_url != ep.url {
            let mut m = self.final_cache.lock().unwrap_or_else(|e| e.into_inner());
            m.insert(ep.url.clone(), (final_url.to_string(), Instant::now()));
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

    fn probe(&self, ep: &Endpoint) -> Result<ProbeInfo> {
        let resp = self.get(ep, &[], Some((0, 0)))?;

        let status = resp.status().as_u16();
        let etag = header_str(&resp, "etag");
        let last_modified = header_str(&resp, "last-modified");
        let file_name = parse_filename(&header_str(&resp, "content-disposition"));
        let clen = resp.content_length().map(|v| v as i64).unwrap_or(0);
        let content_range = header_str(&resp, "content-range");
        let accept_ranges = header_str(&resp, "accept-ranges");
        drop(resp);

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
            self.clear_final(&ep.url); // 缓存可能失效，下次重新解析
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
    ) -> Result<Box<dyn Read + Send>> {
        let resp = self.get(ep, headers, Some((from, to)))?;

        let status = resp.status().as_u16();
        if status != 206 {
            self.clear_final(&ep.url);
            return Err(retryable("range", format!("{ERR_RANGE_MISMATCH}: 期望 206，实际 {status}")));
        }
        let cr = header_str(&resp, "content-range");
        let (start, _, _) = parse_content_range(&cr)
            .ok_or_else(|| retryable("range", format!("{ERR_RANGE_MISMATCH}: Content-Range 缺失")))?;
        if start != from {
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: 期望起点 {from}，服务器给了 {start}"),
            ));
        }
        Ok(Box::new(self.body(resp)))
    }

    fn open_plain(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<Box<dyn Read + Send>> {
        let resp = self.get(ep, headers, None)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            self.clear_final(&ep.url);
            return Err(retryable("whole", format!("服务器返回状态 {status}")));
        }
        Ok(Box::new(self.body(resp)))
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
