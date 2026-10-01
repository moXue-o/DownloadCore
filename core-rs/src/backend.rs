//! 网络后端抽象：引擎只认这个接口，具体"用谁去上网"由编译期决定。
//!
//! - 自研后端（正式版，`netclient`）：标准库 + 系统 TLS，小；
//! - 大框架后端（LTS 版，`client`）：reqwest，稳但大。
//!
//! 引擎、分段、续传、看门狗、写文件等全部与后端无关，因此"写一次代码、打包两个版本"。

use crate::config::Config;
use crate::errors::Result;
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
    /// 探路：文件多大、能不能分段、ETag 等。
    fn probe(&self, ep: &Endpoint) -> Result<ProbeInfo>;
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
        return after.split(']').next().map(|s| s.to_string());
    }
    Some(authority.split(':').next().unwrap_or(authority).to_string())
}

/// 网址的端口（默认 http=80 / https=443）。
pub fn port_of(url: &str) -> u16 {
    let default = if url.starts_with("https://") { 443 } else { 80 };
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
    v.dedup();
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

/// 建来源池并探路。镜像只有"与主源是同一文件"才被采纳。
pub fn build_pool(
    be: &dyn Backend,
    cfg: &Config,
    primary: &str,
    mirrors: &[String],
) -> Result<(Vec<Endpoint>, ProbeInfo)> {
    let mut eps = sources_for(cfg, primary);
    let info = be.probe(&eps[0])?;

    for m in mirrors {
        if m.trim().is_empty() {
            continue;
        }
        // 先用单个来源探路，确认是同一文件再纳入（并展开多 IP）
        let tmp = sources_for(cfg, m);
        match be.probe(&tmp[0]) {
            Ok(pi) if same_file(&info, &pi) => eps.extend(tmp),
            _ => {}
        }
    }
    Ok((eps, info))
}
