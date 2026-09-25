# 下载核心（Download Core）

为宿主程序提供 HTTP/HTTPS 多线程下载能力的**核心库**。

**定位**：只做一件事 —— 把 URL 后面的字节，原样搬成本地文件。
不含界面、队列、浏览器扩展、托盘、通知等任何"软件外壳"。

**路线**：Go 验证 → Rust 重写 → C ABI（编译期吸收进宿主）。

**当前进度**：
- Go 验证版完成，已实测跑满服务器带宽上限（与 ABDM / curl 一致，约 9 MB/s）。
- Rust 版已实现并通过测试，产出 `staticlib`；C ABI 层待补。

## 目录结构

| 路径 | 说明 |
| --- | --- |
| `core-go/` | Go 验证版（实现 + 测试 + 测试程序），详见 `core-go/README.md` |
| `core-rs/` | Rust 版（`staticlib` + 未来 C ABI），详见 `core-rs/README.md` |

## 快速开始

**Go 验证版**

```bash
cd core-go
go test ./...
go run ./cmd/get          # 交互式下载测试程序
```

**Rust 版**

```bash
cd core-rs
cargo test
cargo build               # 产出 staticlib
```
