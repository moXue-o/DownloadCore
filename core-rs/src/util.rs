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

// ---------------- SHA-256（纯 Rust，零依赖） ----------------
//
// 用途：宿主要求校验和时，下完先算文件摘要、与期望值核对，再改名。
// 这是堵"静默数据损坏"（服务器谎报验证器/无验证器时内容中途变化、连接关闭定界的截断）
// 的唯一根治手段。

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let (chunk, rest) = data.split_at(64);
            let block: &[u8; 64] = chunk.try_into().expect("正好 64 字节");
            self.compress(block);
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0x00]);
        }
        self.update(&bit_len.to_be_bytes());
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
        self.h[5] = self.h[5].wrapping_add(f);
        self.h[6] = self.h[6].wrapping_add(g);
        self.h[7] = self.h[7].wrapping_add(h);
    }
}

/// 十六进制小写。
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 计算文件的 SHA-256（十六进制小写）；出错返回 io::Error。
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finish()))
}

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

    #[test]
    fn sha256_known_vectors() {
        let h = |s: &str| {
            let mut x = Sha256::new();
            x.update(s.as_bytes());
            to_hex(&x.finish())
        };
        assert_eq!(h(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(h("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            h("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 跨多块 + 各种 padding 边界（56/63/64/65 长度）
        for n in [55usize, 56, 63, 64, 65, 1000] {
            let s = "a".repeat(n);
            let mut x = Sha256::new();
            x.update(s.as_bytes());
            assert_eq!(to_hex(&x.finish()).len(), 64);
        }
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
