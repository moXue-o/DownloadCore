# 下载核心 · Rust 版

Go 验证版的 Rust 重写。目标形态：**`staticlib` + C ABI**，能被宿主编译期吸收。

定位不变：只做"把 URL 后面的字节原样搬成本地文件"，不含界面、队列等外壳。

## 构建 / 测试

需要：Rust（rustup 稳定版）+ Windows 上还需 MSVC 链接器（Visual Studio Build Tools）。

```bash
cd core-rs
cargo test          # 跑全部测试
cargo build         # 产出 target/debug/downloadcore.lib（staticlib）
cargo build --release
```

TLS 走**系统实现**（Windows 下为 Schannel），因此不需要 nasm/cmake，也不依赖 OpenSSL。

## 示例程序（演示宿主怎么调用）

```bash
cargo run --bin get              # 交互式：提示输入网址
cargo run --bin get -- <网址>     # 直接下载
cargo run --bin get -- -v         # 只看版本
cargo run --bin get -- --extreme <网址>   # 极限模式（128 连接 + 系统优先级）
cargo run --bin get -- --threads 64 <网址> # 自定义并发
# 在提示后输入 selftest 可不联网自检
```

它明确演示了宿主集成的三个动作：

1. `Config` + `Engine::new(cfg)` 造引擎；
2. 把 URL / 落盘位置装进 `Request`；
3. 挂 `Callbacks`（`on_progress` / `on_status` / `on_log`）后调用 `engine.download(...)`。

引擎只给"原始累计字节 + 瞬时速度"，**平滑与显示由宿主决定**。
详细日志写到当前目录的 `download.log`（带 UTF-8 BOM，记事本不乱码）。

编译出可执行文件：`cargo build --bin get` → `target/debug/get.exe`。

### 极限模式（Extreme）

`Config::extreme()`（或示例的 `--extreme`）：**128 连接 + 进程 HIGH 优先级 + 关闭省电节流**，用于抢网。

**它是什么**：尽最大合法努力占满共享管道——连接数开满（按流公平的瓶颈下挤占别人）、CPU 优先级拉满（CPU 争抢时先服务自己）、不自我限速、连接满员。

**它不是万能**：如果服务器对我们**按 IP 限速**（如某 CDN 只给 ~9 MB/s）、且本机链路富余，那**无法把其他应用压到 0**——这是物理限制。实测（千兆链路 + 限速服务器）：本机 8/32/64 连接几乎不影响另一进程的速度。极限模式在"小水管 / 路由器 / CPU 争抢"场景才有明显效果。

## 目录结构

| 文件 | 作用 |
| --- | --- |
| `src/engine.rs` | 引擎与调度（异步内部、阻塞外壳） |
| `src/part.rs` | 分段 + 安全区（"边下边分"的核心） |
| `src/split.rs` | 开局均匀切分 |
| `src/client.rs` | 探路、分段请求、"对暗号"（基于 reqwest） |
| `src/store.rs` | 续传记录（原子写入） |
| `src/assemble.rs` | 分段临时文件拼装 |
| `src/limiter.rs` | 全局令牌桶限速 |
| `src/errors.rs` | 错误分类：可重试 / 致命 / 取消 / 过慢 |
| `src/util.rs` | 文件名解析、路径、哈希等 |
| `tests/engine_test.rs` | 端到端测试（含一个 std 实现的测试服务器） |

## 已实现（对应 Go 验证版）

- 多线程分段下载；动态分段（挑"剩余最多"的段分裂）
- 对暗号（校验服务器返回分段起点）
- 认得出"文件变了"（大小 / ETag / 最后修改）
- 空闲超时（`reqwest` 的 `read_timeout`）
- 慢连接看门狗（过慢则重开连接）
- 错误分类、分段临时文件 + 拼装、下完才改名
- 续传（原子写入 + 取消后续传 + 换文件重下）
- 全局限速、探测分段支持、文件名解析

测试：4 个单元测试 + 6 个集成测试，全部通过。

## 设计要点

- **对外阻塞、对内异步**：`Engine::download` 是同步接口，内部用 tokio 运行时 + 异步 reqwest，
  这样既能用上"逐次读超时"，宿主又能用最简单的同步调用。
- **只用 HTTP/1.1**：多连接比多路复用更适合按连接限速的 CDN。
- **引擎不管平滑/显示**：只给"累计字节 + 瞬时速度"，平滑交给宿主。

## 下一步

- C ABI 层（不透明句柄 + 回调 + userdata）与 C 头文件（cbindgen 或手写）。
- 与 Go 版对齐的参数与行为收口。
