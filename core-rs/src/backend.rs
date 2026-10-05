//! 网络后端抽象：引擎只认这个接口，具体"用谁去上网"由编译期决定。
//!
//! - 自研后端（正式版，`netclient`）：标准库 + 系统 TLS，小；
//! - 大框架后端（LTS 版，`client`）：reqwest，稳但大。
//!
//! 引擎、分段、续传、看门狗、写文件等全部与后端无关，因此"写一次代码、打包两个版本"。

use crate::config::Config;
use crate::errors::{retryable, Error, Result};
use std::io::Read;

/// 探路结果（两个后端共用同一形状）。
#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub size: i64,
    pub range_ok: bool,
    pub etag: String,
    pub last_modified: String,
}

/// 下载目标：就一个网址。核心不做"多来源/多 IP 并行"。
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub url: String,
}

/// 分段请求的"期望值"：用于在收到 206 后复核服务器给的验证器/总长（堵"中途换内容/换来源"）。
#[derive(Clone, Copy, Default)]
pub struct RangeCheck<'a> {
    pub etag: &'a str,
    pub last_modified: &'a str,
    /// 探路得到的总大小（0=未知，不校验）
    pub total: i64,
}

impl RangeCheck<'_> {
    pub fn none() -> RangeCheck<'static> {
        RangeCheck { etag: "", last_modified: "", total: 0 }
    }
}

/// "上网"这件事的抽象。实现必须可跨线程共享。
pub trait Backend: Send + Sync {
    /// 后端名字，日志用。
    fn name(&self) -> &'static str;
    /// 探路：文件多大、能不能分段、ETag 等。带上传入的额外请求头（Cookie/鉴权等）。
    fn probe(&self, ep: &Endpoint, headers: &[(String, String)]) -> Result<ProbeInfo>;
    /// 打开某一段的字节流（内部需"对暗号"，并复核 `expect` 里的验证器/总长）。
    fn open_range(
        &self,
        ep: &Endpoint,
        headers: &[(String, String)],
        from: i64,
        to: i64,
        expect: &RangeCheck,
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

/// 弱 ETag（`W/"..."`）不能当强校验器用：这里把它视为"没有 ETag"。
pub fn strong_etag(e: &str) -> &str {
    if e.trim_start().starts_with("W/") {
        ""
    } else {
        e
    }
}

/// 两个验证器组是否"相容"（同一文件）：**强 ETag 优先且排他**；否则比原始 ETag；
/// 再否则比 Last-Modified。没有任何共同可比项 → false（不认）。
pub fn validators_compatible(
    a_etag: &str,
    a_lm: &str,
    b_etag: &str,
    b_lm: &str,
) -> bool {
    let (sa, sb) = (strong_etag(a_etag), strong_etag(b_etag));
    if !sa.is_empty() && !sb.is_empty() {
        return sa == sb; // 强 ETag 不匹配即不同，不再看 Last-Modified
    }
    if !a_etag.is_empty() && !b_etag.is_empty() {
        return a_etag == b_etag; // ETag 是 opaque，大小写敏感
    }
    if !a_lm.is_empty() && !b_lm.is_empty() {
        return a_lm == b_lm;
    }
    false
}

/// 探路（带重试）。探路失败 = 整个任务失败，所以不能只试一次。
pub fn probe_target(
    be: &dyn Backend,
    cfg: &Config,
    url: &str,
    headers: &[(String, String)],
    canceled: &dyn Fn() -> bool,
) -> Result<ProbeInfo> {
    let ep = Endpoint { url: url.to_string() };
    let rounds = cfg.max_retries.clamp(1, 3);
    let mut last_err = None;
    for r in 0..rounds {
        if canceled() {
            return Err(Error::canceled());
        }
        match be.probe(&ep, headers) {
            Ok(pi) => return Ok(pi),
            Err(e) => last_err = Some(e),
        }
        if r + 1 < rounds {
            if canceled() {
                return Err(Error::canceled());
            }
            std::thread::sleep(cfg.retry_delay);
        }
    }
    Err(last_err.unwrap_or_else(|| retryable("probe", "探路失败")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validators_compatible_rules() {
        // 都缺 → 不相容（不认）
        assert!(!validators_compatible("", "", "", ""));
        // 强 ETag 相同/不同
        assert!(validators_compatible("\"a\"", "", "\"a\"", ""));
        assert!(!validators_compatible("\"a\"", "", "\"b\"", ""));
        // 强 ETag 优先且排他：强 ETag 不同 → 不认（即便 Last-Modified 相同）
        assert!(!validators_compatible("\"a\"", "x", "\"b\"", "x"));
        // 强 ETag 匹配即可（Last-Modified 不同也认）
        assert!(validators_compatible("\"a\"", "x", "\"a\"", "y"));
        // 弱 ETag：相同 → 相容；不同 → 不认（即便 LM 相同）
        assert!(validators_compatible("W/\"a\"", "", "W/\"a\"", ""));
        assert!(!validators_compatible("W/\"a\"", "x", "W/\"b\"", "x"));
        // 无 ETag 时用 Last-Modified
        assert!(validators_compatible("", "x", "", "x"));
        assert!(!validators_compatible("", "x", "", "y"));
    }
}
