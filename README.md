<div align="center">

<h1>DownloadCore</h1>

<p><b>把 URL 后面的字节，原样搬成你指定的那一个文件。</b></p>

<p>
Rust 下载核心 · 多线程分段 · <code>staticlib</code> + C ABI · 在宿主编译期被吸收
</p>

<p>
  <img alt="rust"    src="https://img.shields.io/badge/Rust-edition%202024-000000?logo=rust&logoColor=white">
  <img alt="platform" src="https://img.shields.io/badge/platform-Windows-0078D6?logo=windows&logoColor=white">
  <img alt="abi"     src="https://img.shields.io/badge/ABI-C%20staticlib-555555">
  <img alt="http"    src="https://img.shields.io/badge/HTTP-1.1-4285F4">
  <img alt="version" src="https://img.shields.io/badge/version-0.1.0-success">
  <img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
  <img alt="deps"    src="https://img.shields.io/badge/dependencies-0%20CVE-success">
</p>

</div>

---

> **两种后端测试全绿** · **依赖零 CVE** · 真实地址实测通过（含 302 跳转）
> —— 只做一件事：**下载**。不含界面、队列、托盘、扩展、通知。

## 目录

- [特性](#特性)
- [交付物](#交付物)
- [快速开始](#快速开始)
- [Rust 宿主用法](#rust-宿主用法)
- [C ABI 集成](#c-abi-集成)
- [架构](#架构)
- [目录结构](#目录结构)
- [配置项](#配置项)
- [错误码](#错误码)
- [设计要点](#设计要点)
- [范围边界](#范围边界)
- [已知边界](#已知边界)
- [许可证](#许可证)

## 特性

| | |
| --- | --- |
| 🚀 **多线程分段** | 一个工人一条线程，阻塞式；开局均分，收尾把剩余切细 |
| ♻️ **续传** | 进度原子落盘；需服务器提供强 ETag / Last-Modified 才信任续传 |
| 🧱 **直写无拼装** | 按偏移直接写一个输出文件，下完才改名成正式文件 |
| 🛡️ **内容一致性** | 每段校验起点 / 总长 / 验证器；带 `If-Range`；可选端到端 SHA-256 |
| ⏱️ **看门狗 + 空闲超时** | 过慢连接重开；套接字读超时精确判定"卡住" |
| 🎚️ **全局限速** | 令牌桶；单线程/分段都生效 |
| ⏯️ **暂停 / 恢复 / 取消** | 可从别的线程调用 |
| 🔌 **两种网络后端** | 自研（小）与 `reqwest`（LTS）二选一，接口/行为一致 |

## 交付物

| 产物 | 路径 | 说明 |
| --- | --- | --- |
| **静态库** | `target/release/downloadcore.lib` | 宿主直接链接、编译期吸收 |
| **C 头文件** | `include/downloadcore.h` | 与库一一对应；结构体布局有 C/Rust 双向静态断言守护 |

> 真正交给宿主的就这两样。仓库里的 `tests/` 是自证用，不随库交付。

## 快速开始

需要 Rust（rustup 稳定版，edition 2024）；Windows 上还需 MSVC 链接器。

```bash
cargo test                          # 默认：自研网络后端
cargo test --features backend-lts   # LTS 网络后端（同一套测试）

cargo build --release --lib         # 产出 target/release/downloadcore.lib
```

**两个版本，写一次代码，编译期选"网络后端"：**

| | 正式版（默认） | LTS 版 |
| --- | --- | --- |
| 网络层 | 自研 `netclient`：标准库 + 系统 TLS | 现成 `reqwest` |
| 定位 | 小、依赖少 | 稳、边角全，体积大 |

TLS 走**系统实现**：Windows 用 Schannel、macOS 用系统安全框架，无需 nasm/cmake/OpenSSL；Linux 上 `native-tls` 走 OpenSSL。

## Rust 宿主用法

```rust
use downloadcore::{Callbacks, Config, Engine, Request};

let engine = Engine::new(Config::default());

let res = engine.download(
    Request {
        url: "https://example.com/file.bin".into(),
        target_file: Some("D:/downloads/file.bin".into()), // 必填：完整落盘路径
        ..Default::default()
    },
    Callbacks::default(),
)?;

println!("完成：{}（{} 字节）", res.path, res.size);
```

- **`target_file` 必填**——核心不替宿主猜文件名。
- **取消 / 暂停**：把 `Request.cancel` / `Request.pause`（`Arc<AtomicBool>`）从别的线程置位。
- **防静默损坏**：`Request.expected_sha256` 给了期望值，下完在**改名之前**核对。
- **回调线程安全**：`Callbacks` 会被多个工人线程并发调用。
- **分段进度（可选）**：`Callbacks.on_parts` 与进度同频回调每一段的 `[from, to, current]`（`PartProgress`），可用于绘制"每段进度"；不关心分段留空即可。

## C ABI 集成

对应头文件 `include/downloadcore.h`。

```c
dc_config cfg = dc_config_default();          /* 按需改字段 */
dc_engine* e  = dc_engine_new(&cfg);

dc_request req; memset(&req, 0, sizeof(req));
req.url         = "https://example.com/file.bin";
req.target_file = "D:/downloads/file.bin";    /* 必填：完整路径 */
/* req.header_keys / header_values / header_count —— 额外请求头（可空） */
/* req.expected_sha256                            —— 可选，给了就核对 */

dc_result res; memset(&res, 0, sizeof(res));
char* err = NULL;                              /* 必须非 NULL */
int rc = dc_engine_download(e, &req, on_progress, on_status, on_log, userdata, &res, &err);

if (rc == 0) {
    /* res.path / res.size / res.speed / res.parts / res.range_ok */
} else {
    dc_string_free(err);                       /* err 是文字说明 */
}
dc_result_free(&res);                          /* 释放 res.path */

/* 暂停 / 恢复 / 取消（可从别的线程调用）：dc_engine_pause / resume / cancel */
dc_engine_free(e);
```

**内存释放**：`err_msg` → `dc_string_free`；`res.path` → `dc_result_free`。两者**不可混用**。

## 架构

```text
        ┌─────────────────────────────────────┐
        │              宿主程序                │
        │   C / C++ / 任意能调 C ABI 的语言      │
        └──────────────────┬──────────────────┘
                           │  staticlib + C ABI
                           │  （请求 · 回调 · 结果 · 错误码）
        ┌──────────────────▼──────────────────┐
        │               Engine                │  分段 · 调度 · 续传
        │   一个工人一条线程，阻塞式            │  看门狗 · 直写 · 改名 · 校验和
        └──────────────────┬──────────────────┘
                           │  统一的 Backend 接口
        ┌──────────────────▼──────────────────┐
        │          Backend（上网层）           │
        │   netclient（自研） │ client（LTS）   │
        └──────────────────┬──────────────────┘
                           ▼
                     HTTP/1.1  ·  TLS
```

## 目录结构

| 路径 | 作用 |
| --- | --- |
| `src/lib.rs` | 库入口，对外导出 |
| `src/config.rs` | 可调参数 + 默认值 |
| `src/types.rs` | 请求 / 结果 / 进度 / 状态 / 日志 / 回调 |
| `src/errors.rs` | 错误分类（可重试 / 致命 / 取消 / 过慢） |
| `src/engine.rs` | 引擎：分段、调度、续传、看门狗、改名、校验和 |
| `src/backend.rs` | 网络后端抽象 + 探路 + 工具 |
| `src/netclient.rs` | 自研网络层（默认后端） |
| `src/client.rs` | LTS 网络层（`--features backend-lts`） |
| `src/part.rs` · `src/split.rs` | 分段状态 · 开局切分 |
| `src/store.rs` | 续传记录（原子写入） |
| `src/limiter.rs` | 全局限速 |
| `src/util.rs` | 定位写入 · 容错锁 · Content-Range · SHA-256 |
| `src/ffi.rs` | C ABI 本体 |
| `include/downloadcore.h` | C 头文件 |
| `tests/` | 自动化测试（可摆布的本地服务器 + 引擎/网络层/边角） |

## 配置项

Rust 侧 `Config` 与 C 侧 `dc_config` 一一对应（C 用毫秒）。`<=0` 表示用默认值。

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `initial_threads` | `32` | 开局工人数（连接数） |
| `max_threads` | `32` | 最多工人数 |
| `min_part_size` | `1 MiB` | 分段粒度（目标） |
| `buffer_size` | `256 KiB` | 每个工人的读写缓冲 |
| `idle_timeout_ms` | `15000` | 空闲多久判定"卡住" |
| `max_retries` | `10` | 每段最大重试次数 |
| `retry_delay_ms` | `1000` | 每次重试前等待 |
| `temp_dir` | 系统临时目录 | 续传/临时文件根目录 |
| `incomplete_suffix` | `.part` | 未完成文件后缀 |
| `user_agent` | `downloadcore/0.1` | 默认 User-Agent |
| `max_speed` | `0` | 全局限速（字节/秒），0 不限 |

## 错误码

`dc_engine_download` 的返回值（`dc_error`）：

| 码 | 名称 | 含义 |
| :---: | --- | --- |
| 0 | `DC_OK` | 成功 |
| 1 | `DC_ERR_BUSY` | 同一句柄已有下载在进行 |
| 2 | `DC_ERR_CANCELED` | 被取消 |
| 3 | `DC_ERR_RETRY_EXHAUSTED` | 重试次数用尽 |
| 4 | `DC_ERR_RANGE` | 服务器分段与请求不一致 |
| 5 | `DC_ERR_HTTP` | 网络 / 服务器状态异常 |
| 6 | `DC_ERR_IO` | 文件 / 磁盘错误 |
| 7 | `DC_ERR_INVALID` | 参数无效（含 URL 非法） |
| 8 | `DC_ERR_INTERNAL` | 其它内部错误 |
| 9 | `DC_ERR_TARGET_BUSY` | 目标文件正被另一个下载任务占用 |
| 10 | `DC_ERR_CHECKSUM` | 校验和不符 |

## 设计要点

- **一个工人一条线程**：阻塞式，"每段一条连接"语义最自然；对外仍是同步接口。
- **只换上网那一层**：引擎只认 `Backend` 接口（`probe` / `open_range` / `open_plain`），自研与 LTS 都实现它。
- **只用 HTTP/1.1**：多连接比多路复用更适合按连接限速的 CDN。
- **引擎不管平滑 / 显示**：只给"累计字节 + 瞬时速度"，如何展示交给宿主。

## 范围边界

明确**不做**：

- 软件外壳（界面 / 队列 / 托盘 / 浏览器扩展 / 通知）；
- 解析内容、识别文件类型、重命名规则；
- 代理、进程优先级、电源计划——**属宿主 / 系统职责**。

## 已知边界

- 目前只在 **Windows** 实测；Linux / macOS 未验证，Linux 需系统 OpenSSL。
- 校验和是**可选**的：宿主不给 `expected_sha256`、且服务器无验证器 / 谎报时，静默损坏无法根治。
- 分段阈值（看门狗窗口、尾巴细切粒度等）为经验值，换一批服务器可能需要调整。

## 许可证

采用 **MIT OR Apache-2.0** 双许可，使用者可任选其一：

- [`LICENSE-MIT`](LICENSE-MIT)
- [`LICENSE-APACHE`](LICENSE-APACHE)
