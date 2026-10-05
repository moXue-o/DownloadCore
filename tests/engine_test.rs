//! 端到端测试：用一个 std 实现的、可摆布的测试 HTTP 服务器（见 `common`），
//! 验证多线程、动态分段、不支持分段、对暗号、空闲超时、取消续传、换文件。

mod common;

use common::{make_data, unique_dir, TestServer};
use downloadcore::{Callbacks, Config, Engine, ErrorKind, Request};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// ---------------- 测试辅助 ----------------

fn test_config(dir: &PathBuf) -> Config {
    let mut c = Config::default();
    c.initial_threads = 1;
    c.max_threads = 4;
    c.min_part_size = 256 << 10;
    c.temp_dir = dir.join("temp");
    c.idle_timeout = Duration::from_secs(3);
    c.max_retries = 2;
    c.retry_delay = Duration::from_millis(50);
    c
}

fn read_file(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

// ---------------- 测试 ----------------

#[test]
fn download_multithread_matches_source() {
    let data = make_data(4 << 20, 1);
    let srv = TestServer::new(data.clone());
    let dir = unique_dir("multi");
    let target = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    assert!(res.range_ok);
    assert!(res.parts >= 2, "期望动态分段 >1 段，实际 {}", res.parts);
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn no_range_falls_back_to_single_stream() {
    let data = make_data(1 << 20, 2);
    let srv = TestServer::new(data.clone());
    srv.set_no_range(true);
    let dir = unique_dir("norange");
    let target = dir.join("out.bin");
    let engine = Engine::new(test_config(&dir));

    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    assert!(!res.range_ok);
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn range_mismatch_is_detected() {
    let data = make_data(1 << 20, 3);
    let srv = TestServer::new(data);
    srv.set_fail_range(true);
    let dir = unique_dir("mismatch");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.initial_threads = 1;
    cfg.max_threads = 1;
    cfg.max_retries = 1;
    let engine = Engine::new(cfg);

    let err = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Fatal);
}

#[test]
fn idle_timeout_triggers_error() {
    let data = make_data(2 << 20, 4);
    let srv = TestServer::new(data);
    srv.set_stall_after(64 << 10);
    let dir = unique_dir("idle");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.initial_threads = 1;
    cfg.max_threads = 1;
    cfg.idle_timeout = Duration::from_millis(300);
    cfg.max_retries = 1;
    let engine = Engine::new(cfg);

    let err = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Fatal);
}

#[test]
fn cancel_then_resume() {
    let data = make_data(4 << 20, 5);
    let srv = TestServer::new(data.clone());
    srv.set_speed(2 << 20); // 2 MiB/s，保证能中途取消
    let dir = unique_dir("resume");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.max_retries = 0;
    cfg.idle_timeout = Duration::from_secs(5);
    let engine = Engine::new(cfg);

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
                target_file: Some(target.display().to_string()),
                cancel: Some(cancel),
                ..Default::default()
            },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Canceled);
    assert!(!target.exists(), "取消后不应存在最终文件");

    // 恢复：全速重下剩余部分
    srv.set_speed(0);
    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    assert_eq!(read_file(&res.path), data);
}

#[test]
fn file_changed_on_resume_starts_fresh() {
    let data1 = make_data(2 << 20, 6);
    let data2 = make_data(2 << 20, 7);
    let srv = TestServer::new(data1);
    srv.set_speed(1 << 20);
    let dir = unique_dir("changed");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.max_retries = 0;
    cfg.idle_timeout = Duration::from_secs(5);
    let engine = Engine::new(cfg);

    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        c2.store(true, Ordering::SeqCst);
    });
    let err = engine
        .download(
            Request {
                url: srv.url(),
                target_file: Some(target.display().to_string()),
                cancel: Some(cancel),
                ..Default::default()
            },
            Callbacks::default(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Canceled);

    // 服务器上的文件变了
    srv.set_data(data2.clone(), "\"v2\"");
    srv.set_speed(0);

    let res = engine
        .download(
            Request { url: srv.url(), target_file: Some(target.display().to_string()), ..Default::default() },
            Callbacks::default(),
        )
        .unwrap();
    // 必须是"新文件"，绝不能新旧内容拼在一起
    assert_eq!(read_file(&res.path), data2);
}

#[test]
fn pause_and_resume_completes_correctly() {
    let data = make_data(4 << 20, 8);
    let srv = TestServer::new(data.clone());
    srv.set_speed(2 << 20); // 慢一点，便于中途暂停
    let dir = unique_dir("pause");
    let target = dir.join("out.bin");
    let mut cfg = test_config(&dir);
    cfg.idle_timeout = Duration::from_secs(5);
    let engine = Engine::new(cfg);

    let pause = Arc::new(AtomicBool::new(false));
    let p = pause.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        p.store(true, Ordering::SeqCst); // 暂停
        thread::sleep(Duration::from_millis(500));
        p.store(false, Ordering::SeqCst); // 恢复
    });

    let res = engine
        .download(
            Request {
                url: srv.url(),
                target_file: Some(target.display().to_string()),
                pause: Some(pause),
                ..Default::default()
            },
            Callbacks::default(),
        )
        .unwrap();
    assert_eq!(read_file(&res.path), data);
}



