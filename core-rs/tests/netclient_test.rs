//! 自研网络层（`netclient`）的独立测试：先单独把"和网络打交道"这件事测通，
//! 再谈接进下载引擎。
//!
//! 覆盖：探路、定长/分块/读到尾三种回包、对暗号、跳转、空闲超时、错误状态。

mod common;

use common::{make_data, TestServer};
use downloadcore::backend::RangeCheck;
use downloadcore::netclient::{NetClient, Target};
use downloadcore::ErrorKind;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn client() -> NetClient {
    NetClient::new("downloadcore-test/0.1", Duration::from_secs(3))
}

fn read_all(mut body: impl Read) -> Vec<u8> {
    let mut out = Vec::new();
    body.read_to_end(&mut out).unwrap();
    out
}

#[test]
fn probe_reports_size_and_range() {
    let data = make_data(1 << 20, 1);
    let srv = TestServer::new(data.clone());
    let info = client().probe(&Target::new(srv.url()), &[]).unwrap();

    assert!(info.range_ok, "支持分段的服务器应当 range_ok");
    assert_eq!(info.size, data.len() as i64);
    assert_eq!(info.etag, "\"v1\"");
}

#[test]
fn probe_no_range_server() {
    let data = make_data(200 << 10, 2);
    let srv = TestServer::new(data.clone());
    srv.set_no_range(true);
    let info = client().probe(&Target::new(srv.url()), &[]).unwrap();

    assert!(!info.range_ok, "不支持分段时不应报 range_ok");
    assert_eq!(info.size, data.len() as i64);
}

#[test]
fn probe_error_status_is_fatal() {
    let srv = TestServer::new(make_data(1 << 10, 3));
    srv.set_forced_status(500);
    let err = client().probe(&Target::new(srv.url()), &[]).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Fatal);
}

#[test]
fn open_range_reads_exact_bytes() {
    let data = make_data(200_000, 4);
    let srv = TestServer::new(data.clone());
    let (from, to) = (1000i64, 5000i64);

    let body = client().open_range(&Target::new(srv.url()), &[], from, to, &RangeCheck::none()).unwrap();
    let got = read_all(body);

    assert_eq!(got.len() as i64, to - from + 1);
    assert_eq!(got, data[from as usize..=to as usize]);
}

#[test]
fn range_mismatch_is_detected() {
    let srv = TestServer::new(make_data(100_000, 5));
    srv.set_fail_range(true); // 服务器故意把起点报错 1 个字节

    let err = match client().open_range(&Target::new(srv.url()), &[], 0, 2000, &RangeCheck::none()) {
        Ok(_) => panic!("服务器给错起点，应当报对暗号失败"),
        Err(e) => e,
    };
    assert!(err.is_retryable());
    assert!(err.message.contains("不一致"), "错误信息应指出对暗号失败: {}", err.message);
}

#[test]
fn open_range_detects_validator_change() {
    let data = make_data(1 << 20, 26);
    let srv = TestServer::new(data.clone());
    let c = client();
    let t = Target::new(srv.url());
    let info = c.probe(&t, &[]).unwrap();
    assert_eq!(info.etag, "\"v1\"");

    // 探路之后内容/ETag 变了：分段请求必须能检出，而不是把新内容拼进去
    srv.set_data(data.clone(), "\"v2\"");
    let expect = RangeCheck {
        etag: &info.etag,
        last_modified: &info.last_modified,
        total: info.size,
    };
    let r = c.open_range(&t, &[], 0, 1000, &expect);
    assert!(r.is_err(), "ETag 变了应报错");
}

#[test]
fn plain_download_reads_all() {
    let data = make_data(300_000, 6);
    let srv = TestServer::new(data.clone());
    let body = client().open_plain(&Target::new(srv.url()), &[]).unwrap();
    assert_eq!(read_all(body), data);
}

#[test]
fn chunked_body_is_decoded() {
    let data = make_data(300_000, 7);
    let srv = TestServer::new(data.clone());
    srv.set_chunked(true); // Transfer-Encoding: chunked

    let body = client().open_plain(&Target::new(srv.url()), &[]).unwrap();
    assert_eq!(read_all(body), data, "分块传输应当被正确解码");
}

#[test]
fn chunked_range_is_decoded() {
    let data = make_data(300_000, 8);
    let srv = TestServer::new(data.clone());
    srv.set_chunked(true);

    let body = client().open_range(&Target::new(srv.url()), &[], 100, 999, &RangeCheck::none()).unwrap();
    let got = read_all(body);
    assert_eq!(got, data[100..1000]);
}

#[test]
fn redirect_is_followed() {
    let data = make_data(150_000, 9);
    let target = TestServer::new(data.clone());
    let redirector = TestServer::new(make_data(1 << 10, 10));
    redirector.set_redirect(Some(target.url()));

    let info = client().probe(&Target::new(redirector.url_redir()), &[]).unwrap();
    assert_eq!(info.size, data.len() as i64, "应当跟随 302 到真正的目标");
    assert!(target.hits() > 0, "目标服务器应当被访问到");
}

#[test]
fn early_hints_are_skipped() {
    let data = make_data(300_000, 20);
    let srv = TestServer::new(data.clone());
    srv.set_early_hints(true); // 正式响应前先来一个 103
    let body = client().open_plain(&Target::new(srv.url()), &[]).unwrap();
    assert_eq!(read_all(body), data, "1xx 临时响应应被跳过");
}

/// 起一个只应答一次的自定义服务器：读完请求头后执行 `write`。
fn raw_server(write: impl FnOnce(&mut std::net::TcpStream) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            loop {
                let mut l = String::new();
                if r.read_line(&mut l).unwrap() == 0 || l == "\r\n" {
                    break;
                }
            }
            write(&mut s);
            let _ = s.flush();
        }
    });
    format!("http://{addr}/f")
}

#[test]
fn too_many_interim_responses_errors() {
    let url = raw_server(|s| {
        for _ in 0..50 {
            let _ = s.write_all(b"HTTP/1.1 103 Early Hints\r\n\r\n");
        }
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
    });
    let err = client().probe(&Target::new(url), &[]).unwrap_err();
    assert!(!err.message.is_empty(), "1xx 洪泛应报错而不是一直读");
}

#[test]
fn no_content_status_is_rejected() {
    let url = raw_server(|s| {
        let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
    });
    assert!(client().open_plain(&Target::new(url), &[]).is_err(), "204 应被拒绝");
}

#[test]
fn chunked_empty_line_flood_errors() {
    let url = raw_server(|s| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        for _ in 0..1_000_000 {
            if s.write_all(b"\r\n").is_err() {
                break;
            }
        }
    });
    let mut body = client().open_plain(&Target::new(url), &[]).unwrap();
    let mut buf = [0u8; 1024];
    let mut errored = false;
    for _ in 0..1000 {
        match body.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => {
                errored = true;
                break;
            }
        }
    }
    assert!(errored, "空行洪泛应报错而不是一直读");
}

#[test]
fn idle_timeout_is_reported() {
    let srv = TestServer::new(make_data(2 << 20, 11));
    srv.set_stall_after(32 << 10); // 发 32KB 后卡住

    let mut body = client().open_plain(&Target::new(srv.url()), &[]).unwrap();
    let mut sink = [0u8; 16 << 10];
    let mut total = 0usize;
    let mut timed_out = false;
    for _ in 0..1000 {
        match body.read(&mut sink) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                timed_out = true;
                break;
            }
            Err(e) => panic!("意外错误: {e}"),
        }
    }
    assert!(timed_out, "卡住时应报 TimedOut（已读 {total} 字节）");
}

/// 只接受**一条**连接、却在这条连接上服务两次请求：
/// 若客户端复用了连接，两次请求都会成功；否则第二次会因无人 accept 而超时。
#[test]
fn keep_alive_reuses_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let c = conns.clone();
    let body = b"hello keep-alive".to_vec();
    let payload = body.clone();

    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        c.fetch_add(1, Ordering::SeqCst);
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        for _ in 0..2 {
            // 读完一次请求头
            loop {
                let mut l = String::new();
                let n = reader.read_line(&mut l).unwrap();
                if n == 0 || l == "\r\n" {
                    break;
                }
            }
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", payload.len());
            let mut w = stream.try_clone().unwrap();
            w.write_all(head.as_bytes()).unwrap();
            w.write_all(&payload).unwrap();
            w.flush().unwrap();
        }
    });

    let client = NetClient::new("t", Duration::from_secs(3));
    let url = format!("http://{addr}/f");
    let a = read_all(client.open_plain(&Target::new(url.clone()), &[]).unwrap());
    let b = read_all(client.open_plain(&Target::new(url), &[]).unwrap());

    assert_eq!(a, body);
    assert_eq!(b, body);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(conns.load(Ordering::SeqCst), 1, "应当复用同一条连接");
}
