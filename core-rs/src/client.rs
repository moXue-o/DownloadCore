use crate::config::Config;
use crate::errors::{fatal, retryable, Error, Result, ERR_RANGE_MISMATCH};
use crate::util::{parse_content_range, parse_filename};
use reqwest::Client;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub size: i64,
    pub range_ok: bool,
    pub etag: String,
    pub last_modified: String,
    pub file_name: String,
}

pub struct HttpClient {
    client: Client,
    user_agent: String,
}

fn header_str(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

impl HttpClient {
    pub fn new(cfg: &Config) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(cfg.idle_timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .pool_max_idle_per_host(cfg.max_threads.max(1))
            .http1_only()
            // 不使用系统代理：代理属于宿主/系统的设置，应由宿主显式决定（见 TODO）
            .no_proxy()
            .build()
            .map_err(|e| fatal("http", format!("创建 HTTP 客户端失败: {e}")))?;
        Ok(HttpClient { client, user_agent: cfg.user_agent.clone() })
    }

    fn apply(&self, rb: reqwest::RequestBuilder, headers: &[(String, String)]) -> reqwest::RequestBuilder {
        let mut rb = rb
            .header("Accept-Encoding", "identity")
            .header("User-Agent", self.user_agent.clone());
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        rb
    }

    /// 先问服务器一句"能分段吗、文件多大"。
    pub async fn probe(&self, url: &str, headers: &[(String, String)]) -> Result<ProbeInfo> {
        let rb = self
            .apply(self.client.get(url), headers)
            .header("Range", "bytes=0-0");
        let resp = rb
            .send()
            .await
            .map_err(|e| retryable("probe", format!("{e}")))?;
        let status = resp.status().as_u16();
        let etag = header_str(&resp, "etag");
        let last_modified = header_str(&resp, "last-modified");
        let file_name = parse_filename(&header_str(&resp, "content-disposition"));
        let clen = resp.content_length().map(|v| v as i64).unwrap_or(0);
        let content_range = header_str(&resp, "content-range");
        let accept_ranges = header_str(&resp, "accept-ranges");
        drop(resp);

        let mut info = ProbeInfo {
            size: 0,
            range_ok: false,
            etag,
            last_modified,
            file_name,
        };
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

    /// 打开某一段连接并"对暗号"：状态必须 206，且返回起点必须等于 from。
    pub async fn open_range(
        &self,
        url: &str,
        headers: &[(String, String)],
        from: i64,
        to: i64,
    ) -> Result<reqwest::Response> {
        let rb = self
            .apply(self.client.get(url), headers)
            .header("Range", format!("bytes={from}-{to}"));
        let resp = rb
            .send()
            .await
            .map_err(|e| retryable("request", format!("{e}")))?;
        let status = resp.status().as_u16();
        if status != 206 {
            return Err(retryable(
                "range",
                format!("{ERR_RANGE_MISMATCH}: 期望 206，实际 {status}"),
            ));
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
        Ok(resp)
    }

    /// 普通 GET（不带分段），用于"服务器不支持分段"时的单线程兜底。
    pub async fn open_plain(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<reqwest::Response> {
        let rb = self.apply(self.client.get(url), headers);
        let resp = rb
            .send()
            .await
            .map_err(|e| retryable("request", format!("{e}")))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(retryable("whole", format!("服务器返回状态 {status}")));
        }
        Ok(resp)
    }
}

/// 让编译器知道 Error 在本模块被使用（open_range 用 retryable）。
#[allow(dead_code)]
fn _assert_error_type(_: &Error) {}
