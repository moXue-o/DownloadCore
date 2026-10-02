//! 边角情况测试：自定义请求头（Cookie/鉴权）、各种跳转形态、跳转环。
//!
//! 两种后端共用同一套测试：`cargo test` 跑自研；`--features backend-lts` 跑 LTS。

mod common;

use common::{make_data, unique_dir, TestServer};
use downloadcore::{Callbacks, Config, Engine, Request};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
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

// ---------------- 安全：跨域跳转要剥掉敏感头 ----------------

#[test]
fn cross_origin_redirect_strips_sensitive_headers() {
    let data = make_data(1 << 20, 10);
    let target = TestServer::new(data.clone());
    target.set_capture_header("authorization"); // 记录它有没有收到 Authorization
    let redir = TestServer::new(data.clone()); // 另一台服务器 = 另一个源
    redir.set_redirect(Some(target.url()));
    let dir = unique_dir("xorigin-strip");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = download(
        &engine,
        &redir.url_redir(),
        &out,
        vec![("Authorization".to_string(), "Bearer SECRET".to_string())],
    )
    .unwrap();
    assert_eq!(read_file(&res.path), data);
    assert_eq!(target.captured(), None, "跨域跳转后 Authorization 不应被转发");
}

#[test]
fn same_origin_redirect_keeps_sensitive_headers() {
    let data = make_data(1 << 20, 11);
    let srv = TestServer::new(data.clone());
    srv.set_capture_header("authorization");
    srv.set_redirect(Some("/file.bin".to_string())); // 同源（同一台服务器）
    let dir = unique_dir("sameorigin-keep");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = download(
        &engine,
        &srv.url_redir(),
        &out,
        vec![("Authorization".to_string(), "Bearer KEEP".to_string())],
    )
    .unwrap();
    assert_eq!(read_file(&res.path), data);
    assert_eq!(srv.captured().as_deref(), Some("Bearer KEEP"), "同源跳转应保留 Authorization");
}

// ---------------- 防并发：同一个目标文件 ----------------

#[test]
fn concurrent_same_target_is_rejected() {
    let data = make_data(4 << 20, 12);
    let srv = TestServer::new(data);
    srv.set_speed(512 << 10); // 慢一点，保证第一个还在下
    let dir = unique_dir("concurrent");
    let out = dir.join("out.bin");
    let path = out.display().to_string();
    let engine = Arc::new(Engine::new(test_config(&dir)));

    let e2 = engine.clone();
    let url = srv.url();
    let p2 = path.clone();
    let h = thread::spawn(move || {
        let _ = e2.download(
            Request { url, target_file: Some(p2), ..Default::default() },
            Callbacks::default(),
        );
    });
    thread::sleep(Duration::from_millis(300));
    // 同一目标再来一次 → 必须被拒绝，不能两个一起写坏
    let err = download(&engine, &srv.url(), &out, vec![]).unwrap_err();
    assert!(err.message.contains("正在被另一个"), "错误应说明目标被占用: {}", err.message);
    h.join().unwrap();
}

// ---------------- 续传：没有"身份证"就不硬接 ----------------

#[test]
fn resume_is_skipped_without_validators() {
    let data = make_data(4 << 20, 13);
    let srv = TestServer::new(data.clone());
    srv.set_data(data.clone(), ""); // 清空 ETag → 服务器没有任何"身份证"
    srv.set_speed(2 << 20);
    let dir = unique_dir("novalid");
    let out = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.max_retries = 0;
    cfg.idle_timeout = Duration::from_secs(5);
    let engine = Engine::new(cfg);

    // 先下一半取消
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(400));
        c2.store(true, Ordering::SeqCst);
    });
    let err = engine
        .download(
            Request {
                url: srv.url(),
                target_file: Some(out.display().to_string()),
                cancel: Some(cancel),
                ..Default::default()
            },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, downloadcore::ErrorKind::Canceled);

    // 恢复：没有身份证 → 应当"重新下载"，且结果正确
    srv.set_speed(0);
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lc = logs.clone();
    let cbs = Callbacks {
        on_log: Some(Box::new(move |e| lc.lock().unwrap().push(e.message))),
        ..Default::default()
    };
    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(out.display().to_string()), ..Default::default() },
            cbs,
        )
        .unwrap();
    assert_eq!(read_file(&res.path), data);
    let logged = logs.lock().unwrap().join("\n");
    assert!(logged.contains("重新下载"), "无身份证时应当重新下载:\n{logged}");
}

// ---------------- 探路遇 416 要能退回 ----------------

#[test]
fn probe_416_falls_back_to_plain() {
    let data = make_data(1 << 20, 14);
    let srv = TestServer::new(data.clone());
    srv.set_reject_range(true); // 对任何 Range 请求回 416
    let dir = unique_dir("probe-416");
    let out = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = download(&engine, &srv.url(), &out, vec![]).unwrap();
    assert!(!res.range_ok, "回退到整文件后应报不支持分段");
    assert_eq!(read_file(&res.path), data);
}

// ---------------- 内容变了（If-Range 不匹配）必须干净失败 ----------------

#[test]
fn if_range_mismatch_fails_loudly() {
    let data = make_data(2 << 20, 15);
    let srv = TestServer::new(data);
    srv.set_reject_if_range(true); // 带 If-Range 的请求一律回 200 = "验证器不匹配"
    let dir = unique_dir("ifrange");
    let out = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.max_retries = 1;
    let engine = Engine::new(cfg);

    // 必须干净失败：绝不把"拼错的字节"当成功产出正式文件
    let err = download(&engine, &srv.url(), &out, vec![]).unwrap_err();
    assert!(!err.message.is_empty());
    assert!(!out.exists(), "失败时不应留下正式文件");
}
