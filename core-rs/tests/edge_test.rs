//! 边角情况测试：自定义请求头（Cookie/鉴权）、各种跳转形态、跳转环。
//!
//! 两种后端共用同一套测试：`cargo test` 跑自研；`--features backend-lts` 跑 LTS。

mod common;

use common::{make_data, unique_dir, TestServer};
use downloadcore::{Callbacks, Config, Engine, Request};
use std::path::PathBuf;
use std::time::Duration;

fn test_config(dir: &PathBuf) -> Config {
    let mut c = Config::default();
    c.initial_threads = 2;
    c.max_threads = 4;
    c.min_part_size = 128 << 10; // 切小点，确保走"多段 + 每段请求"
    c.temp_dir = dir.join("temp");
    c.idle_timeout = Duration::from_secs(3);
    c.max_retries = 2;
    c.retry_delay = Duration::from_millis(50);
    c.adaptive_threads = false;
    c
}

fn read_file(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn download(engine: &Engine, url: &str, out: &PathBuf, headers: Vec<(String, String)>) -> downloadcore::Result<downloadcore::DownloadResult> {
    engine.download(
        Request { url: url.to_string(), target_file: Some(out.display().to_string()), headers, ..Default::default() },
        Callbacks::default(),
    )
}

// ---------------- 自定义请求头（Cookie / 鉴权） ----------------

#[test]
fn required_header_missing_fails() {
    let srv = TestServer::new(make_data(1 << 20, 1));
    srv.set_require_header("x-token", "secret");
    let dir = unique_dir("hdr-miss");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    // 没带 token，探路就该被 403 挡下
    let err = download(&engine, &srv.url(), &out, vec![]).unwrap_err();
    assert!(!err.message.is_empty());
}

#[test]
fn custom_header_sent_on_probe_and_parts() {
    let data = make_data(1 << 20, 2);
    let srv = TestServer::new(data.clone());
    srv.set_require_header("authorization", "Bearer abc123");
    let dir = unique_dir("hdr-ok");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    // 服务器对"每个请求"都查这个头：探路、各分段都要带上
    let res = download(
        &engine,
        &srv.url(),
        &out,
        vec![("Authorization".to_string(), "Bearer abc123".to_string())],
    )
    .unwrap();
    assert!(res.parts >= 2, "应切成多段（每段都带头发请求），实际 {}", res.parts);
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn cookie_header_is_forwarded() {
    let data = make_data(1 << 20, 3);
    let srv = TestServer::new(data.clone());
    srv.set_require_header("cookie", "session=xyz");
    let dir = unique_dir("cookie");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = download(&engine, &srv.url(), &out, vec![("Cookie".to_string(), "session=xyz".to_string())]).unwrap();
    assert_eq!(read_file(&res.path), data);
}

// ---------------- 各种跳转形态 ----------------

#[test]
fn redirect_absolute() {
    let data = make_data(1 << 20, 4);
    let target = TestServer::new(data.clone());
    let redir = TestServer::new(data.clone());
    redir.set_redirect(Some(target.url()));
    let dir = unique_dir("redir-abs");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = download(&engine, &redir.url_redir(), &out, vec![]).unwrap();
    assert_eq!(read_file(&res.path), data);
    assert!(target.hits() > 0);
}

#[test]
fn redirect_relative_root() {
    let data = make_data(1 << 20, 5);
    let srv = TestServer::new(data.clone());
    srv.set_redirect(Some("/file.bin".to_string())); // 根相对：应当解析到同一个服务器
    let dir = unique_dir("redir-rel");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = download(&engine, &srv.url_redir(), &out, vec![]).unwrap();
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn redirect_protocol_relative() {
    let data = make_data(1 << 20, 6);
    let target = TestServer::new(data.clone());
    let redir = TestServer::new(data.clone());
    let loc = format!("//{}", &target.url()["http://".len()..]); // //host/path
    redir.set_redirect(Some(loc));
    let dir = unique_dir("redir-proto");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = download(&engine, &redir.url_redir(), &out, vec![]).unwrap();
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn redirect_chain_of_three() {
    let data = make_data(1 << 20, 7);
    let c = TestServer::new(data.clone());
    let b = TestServer::new(data.clone());
    b.set_redirect(Some(c.url())); // b/redir -> c/file.bin
    let a = TestServer::new(data.clone());
    a.set_redirect(Some(b.url_redir())); // a/redir -> b/redir -> c/file.bin
    let dir = unique_dir("redir-chain");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = download(&engine, &a.url_redir(), &out, vec![]).unwrap();
    assert_eq!(read_file(&res.path), data);
    assert!(c.hits() > 0);
}

#[test]
fn redirect_status_303_307_308_are_followed() {
    for code in [303u16, 307, 308] {
        let data = make_data(1 << 20, code as u8);
        let target = TestServer::new(data.clone());
        let redir = TestServer::new(data.clone());
        redir.set_redirect(Some(target.url()));
        redir.set_redirect_status(code);
        let dir = unique_dir(&format!("redir-{code}"));
        let out = dir.join("out.bin");
        let engine = Engine::new(test_config(&dir));

        let res = download(&engine, &redir.url_redir(), &out, vec![])
            .unwrap_or_else(|e| panic!("状态码 {code} 应当能跟随: {e}"));
        assert_eq!(read_file(&res.path), data, "状态码 {code}");
    }
}

#[test]
fn redirect_loop_is_rejected() {
    let srv = TestServer::new(make_data(1 << 20, 9));
    let me = srv.url_redir();
    srv.set_redirect(Some(me)); // 自己跳自己 → 死循环
    let dir = unique_dir("redir-loop");
    let out = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.max_retries = 1;
    let engine = Engine::new(cfg);

    let err = download(&engine, &srv.url_redir(), &out, vec![]).unwrap_err();
    // 必须报错停下，绝不能无限打转
    assert!(!err.message.is_empty());
}
