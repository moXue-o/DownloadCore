# 下载核心（Download Core）· Rust

一个**下载零件**：给宿主提供 HTTP/HTTPS **多线程分段下载**能力。

- **只做一件事**：把一个 URL 后面的字节，原样搬成**宿主指定的那一个文件**。
- **形态**：Rust **`staticlib` + C ABI**，在宿主编译期被吸收（不是独立软件）。
- **不做**：界面、队列、托盘、浏览器扩展、通知等任何"外壳"；不解析内容、不认文件类型。

## 交付物（发布哪些）

| 产物 | 路径 | 说明 |
| --- | --- | --- |
| **静态库** | `target/release/downloadcore.lib` | 宿主直接链接、编译期吸收 |
| **C 头文件** | `include/downloadcore.h` | 与库一一对应；结构体布局有 C/Rust 双向静态断言守护 |

> 真正交给宿主的就这两样。仓库里的 `tests/` 是自证用的，不随库交付。

## 快速开始

需要 Rust（rustup 稳定版；本项目用 edition 2024）。Windows 上还需 MSVC 链接器。

```bash
cargo test                                  # 默认：自研网络后端
cargo test --features backend-lts           # LTS 网络后端（同一套测试）
cargo build --release --lib                 # 产出 target/release/downloadcore.lib
cargo build --release --lib --features backend-lts
```

### 两个版本：写一次代码，编译期选"网络后端"

同一套引擎（分段/续传/看门狗/直写/C 接口），只换**上网那一层**：

| | 正式版（默认） | LTS 版 |
| --- | --- | --- |
| 网络后端 | 自研 `netclient`：标准库 + 系统 TLS | 现成 `reqwest` |
| 特点 | 小、依赖少 | 稳、边角全，体积大 |

两版**功能、接口、行为一致**：宿主换个链接的库即可。

TLS 走**系统实现**：Windows 用 Schannel、macOS 用系统安全框架，无需 nasm/cmake/OpenSSL；Linux 上 `native-tls` 走 OpenSSL（需系统 OpenSSL 或开发包）。

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

要点：
- `target_file` **必填**——核心不替宿主猜文件名。
- 取消/暂停：把 `Request.cancel` / `Request.pause` 两个 `Arc<AtomicBool>` 从别的线程置位。
- 防静默损坏：`Request.expected_sha256` 给了期望值，下完在**改名之前**核对，不符判失败。
- 回调（`Callbacks`）会被多个工人线程并发调用，实现需自行保证线程安全。

## C ABI 集成

对应头文件 `include/downloadcore.h`。三步：

```c
dc_config cfg = dc_config_default();          /* 按需改字段 */
dc_engine* e  = dc_engine_new(&cfg);

dc_request req; memset(&req, 0, sizeof(req));
req.url         = "https://example.com/file.bin";
req.target_file = "D:/downloads/file.bin";    /* 必填：完整路径 */
/* req.header_keys / header_values / header_count：额外请求头（可空） */
/* req.expected_sha256：可选，给了就核对 */

dc_result res; memset(&res, 0, sizeof(res));
char* err = NULL;                             /* 必须非 NULL */
int rc = dc_engine_download(e, &req, on_progress, on_status, on_log, userdata, &res, &err);
if (rc == 0) {
    /* res.path / res.size / res.speed / res.parts / res.range_ok */
} else {
    /* rc 是 dc_error；err 是文字说明 */
    dc_string_free(err);
}
dc_result_free(&res);                         /* 释放 res.path */

/* 暂停 / 恢复 / 取消（可从别的线程调用）：dc_engine_pause / resume / cancel */
dc_engine_free(e);
```

**内存释放**：`err_msg` 用 `dc_string_free`；`res.path` 用 `dc_result_free`，两者不可混用。

**错误码**（`dc_error`）：

| 码 | 含义 |
| --- | --- |
| 0 `DC_OK` | 成功 |
| 1 `DC_ERR_BUSY` | 同一句柄已有下载在进行 |
| 2 `DC_ERR_CANCELED` | 被取消 |
| 3 `DC_ERR_RETRY_EXHAUSTED` | 重试次数用尽 |
| 4 `DC_ERR_RANGE` | 服务器分段与请求不一致 |
| 5 `DC_ERR_HTTP` | 网络/服务器状态异常 |
| 6 `DC_ERR_IO` | 文件/磁盘错误 |
| 7 `DC_ERR_INVALID` | 参数无效（含 URL 非法） |
| 8 `DC_ERR_INTERNAL` | 其它内部错误 |
| 9 `DC_ERR_TARGET_BUSY` | 目标文件正被另一个下载任务占用 |
| 10 `DC_ERR_CHECKSUM` | 校验和不符 |

## 目录结构

| 路径 | 作用 |
| --- | --- |
| `src/lib.rs` | 库入口，对外导出 |
| `src/config.rs` | 可调参数 + 默认值 |
| `src/types.rs` | 请求/结果/进度/状态/日志/回调 |
| `src/errors.rs` | 错误分类（可重试/致命/取消/过慢） |
| `src/engine.rs` | 引擎：分段、调度、续传、看门狗、改名、校验和 |
| `src/backend.rs` | 网络后端抽象 + 探路 + 工具 |
| `src/netclient.rs` | 自研网络层（默认后端） |
| `src/client.rs` | LTS 网络层（`--features backend-lts`） |
| `src/part.rs` / `src/split.rs` | 分段状态 / 开局切分 |
| `src/store.rs` | 续传记录（原子写入） |
| `src/limiter.rs` | 全局限速 |
| `src/util.rs` | 定位写入、容错锁、Content-Range 解析、SHA-256 |
| `src/ffi.rs` | C ABI 本体 |
| `include/downloadcore.h` | C 头文件 |
| `tests/` | 自动化测试（可摆布的本地服务器 + 引擎/网络层/边角测试） |

## 已实现 / 设计要点

- **多线程分段** + 动态分段（收尾把剩余切细）；**一个工人一条线程**，阻塞式。
- **续传**：进度原子落盘；需服务器提供强 ETag 或 Last-Modified 才信任续传。
- **按偏移直写**单个输出文件（无"拼装"环节），下完才改名成正式文件。
- **内容一致性**：探路记下大小/验证器；每段 206 复核起点、总长、验证器；带 `If-Range`。
- **看门狗**：过慢的连接重开；**空闲超时**用套接字读超时精确判定。
- **全局限速**、**暂停/恢复/取消**（可从别的线程调用）。
- **只 HTTP/1.1**：多连接比多路复用更适合按连接限速的 CDN。
- **可选端到端校验和**：`expected_sha256`，改名前端到端核对。

## 范围边界（明确不做）

- 不做软件外壳（界面/队列/托盘/扩展/通知）。
- 不解析内容、不认文件类型、不做重命名规则。
- **代理、进程优先级、电源计划**属宿主/系统职责，核心不碰。

## 已知边界

- 目前只在 **Windows** 实测；Linux/macOS 未验证，Linux 需系统 OpenSSL。
- 校验和是**可选**的：宿主不给 `expected_sha256`、且服务器无验证器/谎报时，静默损坏无法根治。
- 分段阈值（看门狗窗口、尾巴细切粒度等）为经验值，换一批服务器可能需要调整。
