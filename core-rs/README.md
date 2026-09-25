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
