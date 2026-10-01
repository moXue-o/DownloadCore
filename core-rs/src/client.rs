//! LTS 网络后端：用现成的 reqwest（含 tokio）实现统一后端接口。
//!
//! 定位与自研后端完全一致（同一套引擎、同一个 C 接口、同样功能），
//! 区别只在"上网这一层"用了成熟框架——稳、边角全，但体积大。
//!
//! 引擎是"一个工人一条线程"的阻塞模型，所以这里把一个共享的多线程运行时
//! 借来驱动 reqwest 的异步调用：`block_on` 只负责把这条线程的活跑完，
//! 真正的 I/O 由运行时的工作线程处理。这样两种后端可以套同一个引擎。

use crate::backend::{host_of, port_of, Backend, Endpoint, ProbeInfo};
use crate::config::Config;
use crate::errors::{fatal, retryable, Result, ERR_RANGE_MISMATCH};
use crate::util::{parse_content_range, parse_filename};
use std::io::{self, Read};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub struct LtsBackend {
    rt: Arc<tokio::runtime::Runtime>,
    user_agent: String,
    idle_timeout: Duration,
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
        })
    }

    fn client_for(&self, ep: &Endpoint) -> Result<reqwest::Client> {
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(self.idle_timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .http1_only()
            // 不使用系统代理：代理属于宿主/系统的设置，应由宿主显式决定
            .no_proxy();
        if let Some(ip) = ep.ip {
            if let Some(host) = host_of(&ep.url) {
                b = b.resolve(&host, SocketAddr::new(ip, port_of(&ep.url)));
            }
        }
        b.build().map_err(|e| fatal("http", format!("创建 HTTP 客户端失败: {e}")))
    }
}

fn header_str(resp: &reqwest::Response, name: &str) -> String {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

impl Backend for LtsBackend {
    fn name(&self) -> &'static str {
        "lts"
    }

    fn probe(&self, ep: &Endpoint) -> Result<ProbeInfo> {
        let client = self.client_for(ep)?;
        let url = ep.url.clone();
        let ua = self.user_agent.clone();
        let resp = self
            .rt
            .block_on(async move {
                client
                    .get(&url)
                    .header("Accept-Encoding", "identity")
                    .header("User-Agent", ua)
                    .header("Range", "bytes=0-0")
                    .send()
                    .await
            })
            .map_err(|e| retryable("probe", format!("{e}")))?;

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
        let client = self.client_for(ep)?;
        let url = ep.url.clone();
        let ua = self.user_agent.clone();
        let headers = headers.to_vec();
        let resp = self
            .rt
            .block_on(async move {
                let mut rb = client
                    .get(&url)
                    .header("Accept-Encoding", "identity")
                    .header("User-Agent", ua)
                    .header("Range", format!("bytes={from}-{to}"));
                for (k, v) in &headers {
                    rb = rb.header(k, v);
                }
                rb.send().await
            })
            .map_err(|e| retryable("request", format!("{e}")))?;

        let status = resp.status().as_u16();
        if status != 206 {
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
        Ok(Box::new(LtsBody { rt: self.rt.clone(), resp, buf: Vec::new(), off: 0 }))
    }

    fn open_plain(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<Box<dyn Read + Send>> {
        let client = self.client_for(ep)?;
        let url = ep.url.clone();
        let ua = self.user_agent.clone();
        let headers = headers.to_vec();
        let resp = self
            .rt
            .block_on(async move {
                let mut rb = client
                    .get(&url)
                    .header("Accept-Encoding", "identity")
                    .header("User-Agent", ua);
                for (k, v) in &headers {
                    rb = rb.header(k, v);
                }
                rb.send().await
            })
            .map_err(|e| retryable("request", format!("{e}")))?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(retryable("whole", format!("服务器返回状态 {status}")));
        }
        Ok(Box::new(LtsBody { rt: self.rt.clone(), resp, buf: Vec::new(), off: 0 }))
    }
}

/// 把 reqwest 的响应体包装成阻塞式 `Read`：每次要数据就 `block_on` 拉一块。
struct LtsBody {
    rt: Arc<tokio::runtime::Runtime>,
    resp: reqwest::Response,
    buf: Vec<u8>,
    off: usize,
}

impl Read for LtsBody {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.off >= self.buf.len() {
            match self.rt.block_on(self.resp.chunk()) {
                Ok(Some(b)) => {
                    self.buf = b.to_vec();
                    self.off = 0;
                }
                Ok(None) => return Ok(0),
                Err(e) => {
                    let kind = if e.is_timeout() { io::ErrorKind::TimedOut } else { io::ErrorKind::Other };
                    return Err(io::Error::new(kind, e.to_string()));
                }
            }
            if self.buf.is_empty() {
                return Ok(0);
            }
        }
        let n = (self.buf.len() - self.off).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.off..self.off + n]);
        self.off += n;
        Ok(n)
    }
}
