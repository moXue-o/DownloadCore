use crate::config::Config;
use crate::errors::{fatal, retryable, Result, ERR_RANGE_MISMATCH};
use crate::util::{parse_content_range, parse_filename};
use reqwest::Client;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

/// 每个域名最多并行使用的 IP 数
const MAX_IPS_PER_HOST: usize = 4;

#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub size: i64,
    pub range_ok: bool,
    pub etag: String,
    pub last_modified: String,
    pub file_name: String,
}

/// 一个下载来源：URL +（可选）绑定到某个 IP 的客户端。
/// 多个来源轮换使用 → 突破"单 IP / 单源限速"。
#[derive(Clone)]
pub struct Source {
    pub url: String,
    pub client: Client,
    pub label: String, // 日志用：IP 或 "default"
}

pub struct HttpClient {
    sources: Vec<Source>,
    user_agent: String,
}

fn header_str(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(|s| s.to_string())
}

/// 把域名解析成多个 IP；优先 IPv4，最多取 MAX_IPS_PER_HOST 个。
fn resolve_ips(host: &str) -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> = (host, 0)
        .to_socket_addrs()
        .map(|it| it.collect())
        .unwrap_or_default();
    v.sort_by_key(|a| a.is_ipv6()); // IPv4 在前
    v.dedup();
    v.truncate(MAX_IPS_PER_HOST);
    v
}

fn build_client(cfg: &Config, pin: Option<(&str, SocketAddr)>) -> Result<Client> {
    let mut b = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(cfg.idle_timeout)
        .redirect(reqwest::redirect::Policy::limited(10))
        .pool_max_idle_per_host(cfg.max_threads.max(1))
        .http1_only()
        // 不使用系统代理：代理属于宿主/系统的设置，应由宿主显式决定
        .no_proxy();
    if let Some((host, ip)) = pin {
        b = b.resolve(host, ip);
    }
    b.build().map_err(|e| fatal("http", format!("创建 HTTP 客户端失败: {e}")))
}

/// 为一个 URL 建来源：多 IP 则每个 IP 一个来源，否则单个未绑定来源。
fn sources_for(cfg: &Config, url: &str, use_multi_ip: bool) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    if use_multi_ip {
        if let Some(host) = host_of(url) {
            let ips = resolve_ips(&host);
            if ips.len() > 1 {
                for ip in ips {
                    let client = build_client(cfg, Some((&host, ip)))?;
                    out.push(Source { url: url.to_string(), client, label: ip.to_string() });
                }
                return Ok(out);
            }
        }
    }
    let client = build_client(cfg, None)?;
    out.push(Source { url: url.to_string(), client, label: "default".to_string() });
    Ok(out)
}

/// 两个来源算不算"同一个文件"：大小一致、ETag 一致（都为空也算）、分段支持一致。
fn same_file(a: &ProbeInfo, b: &ProbeInfo) -> bool {
    if a.size != b.size || a.range_ok != b.range_ok {
        return false;
    }
    if !a.etag.is_empty() && !b.etag.is_empty() && a.etag != b.etag {
        return false;
    }
    true
}

impl HttpClient {
    /// 建源池并探路。镜像只有"与主源是同一文件"才被采纳。
    pub async fn build(cfg: &Config, primary: &str, mirrors: &[String]) -> Result<(Self, ProbeInfo)> {
        let user_agent = cfg.user_agent.clone();
        let primary_sources = sources_for(cfg, primary, cfg.use_multiple_ips)?;
        let mut http = HttpClient { sources: primary_sources, user_agent };
        let info = http.probe(0).await?;

        for m in mirrors {
            if m.trim().is_empty() {
                continue;
            }
            // 先用未绑定客户端探路，确认是同一文件再纳入
            let tmp = match sources_for(cfg, m, false) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let pi = http.probe_with(&tmp[0], m).await;
            match pi {
                Ok(pi) if same_file(&info, &pi) => match sources_for(cfg, m, cfg.use_multiple_ips) {
                    Ok(mut srcs) => http.sources.append(&mut srcs),
                    Err(_) => {}
                },
                Ok(_) => {}
                Err(_) => {}
            }
        }
        Ok((http, info))
    }

    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    pub fn source(&self, idx: usize) -> &Source {
        &self.sources[idx % self.sources.len().max(1)]
    }

    async fn probe(&self, idx: usize) -> Result<ProbeInfo> {
        let s = &self.sources[idx];
        self.probe_with(s, &s.url).await
    }

    /// 先问服务器一句"能分段吗、文件多大"。
    pub async fn probe_with(&self, s: &Source, url: &str) -> Result<ProbeInfo> {
        let rb = self
            .apply(s.client.get(url))
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

    fn apply(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header("Accept-Encoding", "identity")
            .header("User-Agent", self.user_agent.clone())
    }

    /// 打开某一段连接并"对暗号"：状态必须 206，且返回起点必须等于 from。
    pub async fn open_range(
        &self,
        s: &Source,
        headers: &[(String, String)],
        from: i64,
        to: i64,
    ) -> Result<reqwest::Response> {
        let mut rb = self.apply(s.client.get(&s.url));
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        let resp = rb
            .header("Range", format!("bytes={from}-{to}"))
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
        s: &Source,
        headers: &[(String, String)],
    ) -> Result<reqwest::Response> {
        let mut rb = self.apply(s.client.get(&s.url));
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
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
