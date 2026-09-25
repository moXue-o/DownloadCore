//! 本地测试服务器：给 C 示例程序做离线对练用。
//!
//! 运行：
//!   cargo run --release --bin serve -- [大小MB] [端口]
//! 然后另开一个窗口：
//!   cd cdemo && demo.exe http://127.0.0.1:2121/file.bin .

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let size_mb: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(64);
    let port: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2121);

    let size = size_mb * 1024 * 1024;
    let data: Arc<Vec<u8>> = Arc::new((0..size).map(|i| ((i as u64 * 31 + 7) & 0xff) as u8).collect());

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind failed");
    println!("serving http://127.0.0.1:{port}/file.bin   ({size_mb} MB, 支持分段)");
    println!("按 Ctrl+C 退出。");

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            let d = data.clone();
            thread::spawn(move || {
                let _ = serve(stream, &d);
            });
        }
    }
}

fn serve(mut stream: TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut range: Option<(i64, i64)> = None;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h)?;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("range:") {
            range = parse_range(rest.trim(), data.len() as i64);
        }
    }

    let size = data.len() as i64;
    match range {
        Some((start, end)) if start <= end && start < size => {
            let end = end.min(size - 1);
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                end - start + 1
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(&data[start as usize..=end as usize])?;
        }
        _ => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nAccept-Ranges: none\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(data)?;
        }
    }
    stream.flush()
}

fn parse_range(h: &str, size: i64) -> Option<(i64, i64)> {
    let spec = h.strip_prefix("bytes=")?;
    let dash = spec.find('-')?;
    let left = spec[..dash].trim();
    let right = spec[dash + 1..].trim();
    let (start, end) = if left.is_empty() {
        let n: i64 = right.parse().ok()?;
        ((size - n).max(0), size - 1)
    } else {
        let s: i64 = left.parse().ok()?;
        let e = if right.is_empty() { size - 1 } else { right.parse().ok()? };
        (s, e)
    };
    Some((start.max(0), end.min(size - 1)))
}
