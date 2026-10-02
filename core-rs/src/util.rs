use std::path::Path;

/// 解析 "bytes start-end/total"。
pub fn parse_content_range(v: &str) -> Option<(i64, i64, i64)> {
    let v = v.trim();
    // 注意：不能直接 v[..5]（多字节字符会 panic），用 get 做边界安全比较
    if !v.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("bytes")) {
        return None;
    }
    let rest = v[5..].trim();
    let slash = rest.find('/')?;
    let range_part = rest[..slash].trim();
    let total_part = rest[slash + 1..].trim();
    let dash = range_part.find('-')?;
    let start = range_part[..dash].trim().parse::<i64>().ok()?;
    let end = range_part[dash + 1..].trim().parse::<i64>().ok()?;
    let total = if total_part == "*" {
        -1
    } else {
        total_part.parse::<i64>().ok()?
    };
    Some((start, end, total))
}

/// 从 Content-Disposition 里挖文件名（支持 filename* 和 filename）。
pub fn parse_filename(cd: &str) -> String {
    if cd.is_empty() {
        return String::new();
    }
    let lower = cd.to_ascii_lowercase();
    if let Some(idx) = lower.find("filename*=") {
        let mut rest = &cd[idx + "filename*=".len()..];
        if let Some(semi) = rest.find(';') {
            rest = &rest[..semi];
        }
        let rest = rest.trim();
        if let Some(p) = rest.find("''") {
            return percent_decode(&rest[p + 2..]);
        }
    }
    if let Some(idx) = lower.find("filename=") {
        let mut rest = &cd[idx + "filename=".len()..];
        if let Some(semi) = rest.find(';') {
            rest = &rest[..semi];
        }
        let rest = rest.trim().trim_matches('"');
        return rest.to_string();
    }
    String::new()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 从网址路径里猜文件名。
pub fn filename_from_url(raw: &str) -> String {
    // 只取 path 部分（去协议/主机/查询/锚点），再取最后一段
    let rest = match raw.split_once("://") {
        Some((_, r)) => r,
        None => raw,
    };
    let after_host = match rest.find('/') {
        Some(i) => &rest[i..],
        None => return String::new(),
    };
    let path = after_host.split(['?', '#']).next().unwrap_or(after_host);
    let name = path.rsplit('/').next().unwrap_or("");
    if name.is_empty() || name == "." {
        return String::new();
    }
    percent_decode(name)
}

/// 去掉文件名里操作系统的非法字符。
pub fn sanitize_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let mut s: String = base
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect();
    s = s.trim_matches(|c| c == ' ' || c == '.').to_string();
    if s.is_empty() {
        "download.bin".to_string()
    } else {
        s
    }
}

/// 用目标文件绝对路径算一个稳定的目录名（FNV-1a，够用且无依赖）。
pub fn job_key(abs: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in abs.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

use std::sync::{Mutex, MutexGuard};

/// 在文件的指定偏移处写入（不改变文件游标），用于"多段并发写同一个文件"。
/// 循环写满，处理短写。
#[cfg(windows)]
pub fn write_all_at(f: &std::fs::File, buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0;
    while written < buf.len() {
        let n = f.seek_write(&buf[written..], offset)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "write_at 返回 0"));
        }
        written += n;
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
pub fn write_all_at(f: &std::fs::File, buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut written = 0;
    while written < buf.len() {
        let n = f.write_at(&buf[written..], offset)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "write_at 返回 0"));
        }
        written += n;
        offset += n as u64;
    }
    Ok(())
}

/// 容错加锁：即使别的线程 panic 导致锁"中毒"，也取回内部数据，而不是跟着 panic。
/// 用它替换裸 `Mutex`，可消除一大类 `unwrap` 崩溃点。
pub struct Lock<T>(Mutex<T>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_range_is_boundary_safe() {
        // 多字节字符跨越第 5 字节：不能 panic，只能返回 None
        assert_eq!(parse_content_range("abcd€ 0-9/10"), None);
        assert_eq!(parse_content_range(""), None);
        assert_eq!(parse_content_range("bye"), None);
        // 正常
        assert_eq!(parse_content_range("bytes 0-9/10"), Some((0, 9, 10)));
        assert_eq!(parse_content_range("bytes 5-5/*"), Some((5, 5, -1)));
    }
}

impl<T> Lock<T> {
    pub fn new(v: T) -> Self {
        Lock(Mutex::new(v))
    }
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
