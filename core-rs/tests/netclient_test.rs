//! 自研网络层（`netclient`）的独立测试：先单独把"和网络打交道"这件事测通，
//! 再谈接进下载引擎。
//!
//! 覆盖：探路、定长/分块/读到尾三种回包、对暗号、跳转、空闲超时、错误状态。

mod common;

use common::{make_data, TestServer};
use downloadcore::netclient::{NetClient, Target};
use downloadcore::ErrorKind;
use std::io::Read;
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

    let body = client().open_range(&Target::new(srv.url()), &[], from, to).unwrap();
    let got = read_all(body);

    assert_eq!(got.len() as i64, to - from + 1);
    assert_eq!(got, data[from as usize..=to as usize]);
}

#[test]
fn range_mismatch_is_detected() {
    let srv = TestServer::new(make_data(100_000, 5));
    srv.set_fail_range(true); // 服务器故意把起点报错 1 个字节

    let err = match client().open_range(&Target::new(srv.url()), &[], 0, 2000) {
        Ok(_) => panic!("服务器给错起点，应当报对暗号失败"),
        Err(e) => e,
    };
    assert!(err.is_retryable());
    assert!(err.message.contains("不一致"), "错误信息应指出对暗号失败: {}", err.message);
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

    let body = client().open_range(&Target::new(srv.url()), &[], 100, 999).unwrap();
    let got = read_all(body);
    assert_eq!(got, data[100..1000]);
}

#[test]
fn redirect_is_followed() {
    let data = make_data(150_000, 9);
    let target = TestServer::new(data.clone());
    let redirector = TestServer::new(make_data(1 << 10, 10));
    redirector.set_redirect(Some(target.url()));

    let info = client().probe(&Target::new(redirector.url()), &[]).unwrap();
    assert_eq!(info.size, data.len() as i64, "应当跟随 302 到真正的目标");
    assert!(target.hits() > 0, "目标服务器应当被访问到");
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
