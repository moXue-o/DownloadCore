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

## 测试程序一：Rust 版 `get`（直接用核心）

> 本项目只保留**两个能下载的测试程序**。这是其一（Rust 侧）。其二是 C 版 `demo`（见下节 C ABI）。

```bash
cargo run --bin get              # 交互式：提示输入网址
cargo run --bin get -- <网址>     # 直接下载
cargo run --bin get -- -v         # 只看版本
# 在提示后输入 selftest 可不联网自检
```

它明确演示了宿主集成的三个动作：

1. `Config` + `Engine::new(cfg)` 造引擎；
2. 把 URL / 落盘位置装进 `Request`；
3. 挂 `Callbacks`（`on_progress` / `on_status` / `on_log`）后调用 `engine.download(...)`。

引擎只给"原始累计字节 + 瞬时速度"，**平滑与显示由宿主决定**。
详细日志写到当前目录的 `download.log`（带 UTF-8 BOM，记事本不乱码）。

编译出可执行文件：`cargo build --bin get` → `target/debug/get.exe`。

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

## C ABI（给非 Rust 宿主编译期吸收）

> 对应**测试程序二**：`cdemo\demo.exe` —— 链接静态库 `downloadcore.lib`，验证"库能被 C 程序装进去用"。

产物：`target/release/downloadcore.lib`（staticlib）+ `include/downloadcore.h`。

宿主集成三步（完整例子见 `cdemo/demo.c`）：

```c
dc_config cfg = dc_config_default();      /* 按需改字段 */
dc_engine* e = dc_engine_new(&cfg);
dc_request req; memset(&req, 0, sizeof(req));
req.url = "https://...";
req.target_dir = ".";                      /* 不给文件名就自动取名 */

dc_result res; memset(&res, 0, sizeof(res));
char* err = NULL;
int rc = dc_engine_download(e, &req, on_progress, on_status, on_log, userdata, &res, &err);
if (rc == 0) {
    /* res.path / res.size / res.speed / res.parts / res.range_ok */
} else {
    /* rc 是 dc_error；err 是文字说明 */
    dc_string_free(err);
}
dc_result_free(&res);                      /* 释放 res.path */

/* 暂停/恢复/取消（可从别的线程调用）：dc_engine_pause/resume/cancel */
dc_engine_free(e);
```

链接时必须带上系统库：
`ws2_32 userenv bcrypt ntdll advapi32 ole32 shell32 crypt32`。

编译并实跑 C 示例：

```bash
cd core-rs
cargo build --release
cd cdemo && .\build.bat          # 用 MSVC 编译并链接 downloadcore.lib
# 另开一个窗口起本地服务器：
#   ..\target\release\serve.exe 64 2121
.\demo.exe http://127.0.0.1:2121/file.bin .
```

实测：`demo.exe`（2.4 MB，内含静态链入的核心）成功下载 64 MB 并逐字节正确。

## 下一步

- 与 Go 版对齐的参数与行为收口。
- 小本本"后续更新"里的能力（代理 / 凭据 / 校验和 / 镜像 / 自适应并发）。
- 跨平台（Linux/macOS）验证。
