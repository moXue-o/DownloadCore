//! 网络后端抽象：引擎只认这个接口，具体"用谁去上网"由编译期决定。
//!
//! - 自研后端（正式版，`netclient`）：标准库 + 系统 TLS，小；
//! - 大框架后端（LTS 版，`client`）：reqwest，稳但大。
//!
//! 引擎、分段、续传、看门狗、写文件等全部与后端无关，因此"写一次代码、打包两个版本"。

use crate::config::Config;
use crate::errors::{retryable, Error, Result};
use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};

/// 每个域名最多并行使用的 IP 数。
pub const MAX_IPS_PER_HOST: usize = 4;

/// 探路结果（两个后端共用同一形状）。
#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub size: i64,
    pub range_ok: bool,
    pub etag: String,
    pub last_modified: String,
    pub file_name: String,
}

/// 一个下载来源：网址 +（可选）绑定到某个 IP + 日志标签。
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub url: String,
    pub ip: Option<IpAddr>,
    pub label: String,
}

/// "上网"这件事的抽象。实现必须可跨线程共享。
pub trait Backend: Send + Sync {
    /// 后端名字，日志用。
    fn name(&self) -> &'static str;
    /// 探路：文件多大、能不能分段、ETag 等。带上传入的额外请求头（Cookie/鉴权等）。
    fn probe(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<ProbeInfo>;
    /// 打开某一段的字节流（内部需"对暗号"，起点必须一致）。
    fn open_range(
        &self,
        ep: &Endpoint,
        headers: &[(String, String)],
        from: i64,
        to: i64,
    ) -> Result<Box<dyn Read + Send>>;
    /// 整文件不分段的字节流。
    fn open_plain(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<Box<dyn Read + Send>>;
    /// 一行人类可读的网络统计（新建连接/TLS/复用/跳转等），供日志诊断。
    fn stats(&self) -> String {
        String::new()
    }
}

/// 某个请求头是不是"敏感"的（跨域跳转时要丢掉，别把凭据发到别的主机）。
pub fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "cookie2" | "proxy-authorization" | "www-authenticate"
    )
}

/// 去掉所有敏感头（用于跨域跳转）。
pub fn redact_sensitive(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers.iter().filter(|(k, _)| !is_sensitive_header(k)).cloned().collect()
}

fn origin_of(url: &str) -> Option<(String, u16, String)> {
    let (scheme, _) = url.trim().split_once("://")?;
    let host = host_of(url)?;
    Some((scheme.to_ascii_lowercase(), port_of(url), host))
}

/// 两个网址是否同源（scheme + host + port）。解析不出来就当作"不同源"，宁可从严。
pub fn same_origin(a: &str, b: &str) -> bool {
    match (origin_of(a), origin_of(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// 去掉网址里的 userinfo（`user:pass@host` → `host`），用于写日志/存续传记录，
/// 避免把凭据落到磁盘或日志里。（真正请求时也会剥掉它——它本就不用于鉴权。）
pub fn strip_userinfo(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some(i) = rest.find('@') {
            // 只有 '@' 在 authority 内（首个 '/', '?', '#' 之前）才算 userinfo
            if !rest[..i].contains(['/', '?', '#']) {
                return format!("{scheme}://{}", &rest[i + 1..]);
            }
        }
    }
    url.to_string()
}

/// 从网址里取主机名（去掉协议、userinfo、端口、路径、查询、锚点）。
pub fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    let authority = authority.rsplit('@').next().unwrap_or(authority); // 去 userinfo
    if let Some(after) = authority.strip_prefix('[') {
        // IPv6
        return after.split(']').next().map(|s| s.to_ascii_lowercase());
    }
    // 主机名大小写不敏感：统一小写，避免"同源"被误判为跨域
    Some(authority.split(':').next().unwrap_or(authority).to_ascii_lowercase())
}

/// 网址的端口（默认 http=80 / https=443）。
pub fn port_of(url: &str) -> u16 {
    let default = if url.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://")) { 443 } else { 80 };
    let Some((_, rest)) = url.split_once("://") else { return default };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(after) = authority.strip_prefix('[') {
        if let Some(close) = after.find(']') {
            return after[close + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default);
        }
    }
    match authority.rsplit_once(':') {
        Some((_, p)) if p.chars().all(|c| c.is_ascii_digit()) => p.parse().unwrap_or(default),
        _ => default,
    }
}

/// 把主机名解析成多个 IP；优先 IPv4。
pub fn resolve_ips(host: &str) -> Vec<IpAddr> {
    let mut v: Vec<IpAddr> = (host, 0u16)
        .to_socket_addrs()
        .map(|it| it.map(|a| a.ip()).collect())
        .unwrap_or_default();
    v.sort_by_key(|a| a.is_ipv6());
    // 去重（不能只用 dedup：排序只保证同族相邻，重复项未必相邻）
    let mut seen = std::collections::HashSet::new();
    v.retain(|ip| seen.insert(*ip));
    v.truncate(MAX_IPS_PER_HOST);
    v
}

/// 为一个 URL 建来源：多 IP 则每个 IP 一个来源，否则单个未绑定来源。
fn sources_for(cfg: &Config, url: &str) -> Vec<Endpoint> {
    if cfg.use_multiple_ips {
        if let Some(host) = host_of(url) {
            let ips = resolve_ips(&host);
            if ips.len() > 1 {
                return ips
                    .into_iter()
                    .map(|ip| Endpoint { url: url.to_string(), ip: Some(ip), label: ip.to_string() })
                    .collect();
            }
        }
    }
    vec![Endpoint { url: url.to_string(), ip: None, label: "default".to_string() }]
}

/// 两个来源算不算"同一个文件"：大小、分段支持一致，且**至少有一个身份证（ETag 或
/// Last-Modified）非空并相等**。没有身份证就只比大小太危险（同大小不同内容会拼坏文件）。
fn same_file(a: &ProbeInfo, b: &ProbeInfo) -> bool {
    if a.size != b.size || a.range_ok != b.range_ok {
        return false;
    }
    if !a.etag.is_empty() && !b.etag.is_empty() {
        return a.etag == b.etag;
    }
    if !a.last_modified.is_empty() && !b.last_modified.is_empty() {
        return a.last_modified == b.last_modified;
    }
    false
}

/// 建来源池并探路。镜像只有"与主源是同一文件"才被采纳。
///
/// 探路本身也会重试：依次试池里所有地址，失败就换下一个；全都不行再等一会儿重来。
/// （探路失败=整个任务失败，所以不能"只试一次"。两个后端共用此逻辑。）
pub fn build_pool(
    be: &dyn Backend,
    cfg: &Config,
    primary: &str,
    mirrors: &[String],
    headers: &[(String, String)],
    canceled: &dyn Fn() -> bool,
) -> Result<(Vec<Endpoint>, ProbeInfo)> {
    let mut eps = sources_for(cfg, primary);

    let rounds = cfg.max_retries.clamp(1, 3);
    let mut info: Option<ProbeInfo> = None;
    let mut last_err = None;
    'outer: for r in 0..rounds {
        for ep in eps.iter() {
            if canceled() {
                return Err(Error::canceled());
            }
            match be.probe(ep, headers) {
                Ok(pi) => {
                    info = Some(pi);
                    break 'outer;
                }
                Err(e) => last_err = Some(e),
            }
        }
        if r + 1 < rounds {
            if canceled() {
                return Err(Error::canceled());
            }
            std::thread::sleep(cfg.retry_delay);
        }
    }
    let info = match info {
        Some(i) => i,
        None => {
            return Err(last_err.unwrap_or_else(|| retryable("probe", "探路失败：所有来源都不通")))
        }
    };

    for m in mirrors {
        if m.trim().is_empty() {
            continue;
        }
        if canceled() {
            return Err(Error::canceled());
        }
        // 先用单个来源探路，确认是同一文件再纳入（并展开多 IP）
        let m_headers: Vec<(String, String)> = if same_origin(primary, m) {
            headers.to_vec()
        } else {
            redact_sensitive(headers) // 跨域镜像：别把 Authorization/Cookie 发过去
        };
        let tmp = sources_for(cfg, m);
        match be.probe(&tmp[0], &m_headers) {
            Ok(pi) if same_file(&info, &pi) => eps.extend(tmp),
            _ => {}
        }
    }
    Ok((eps, info))
}
