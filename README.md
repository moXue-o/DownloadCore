# 下载核心（Download Core）

为宿主程序提供 HTTP/HTTPS 多线程下载能力的**核心库**。

**定位**：只做一件事 —— 把 URL 后面的字节，原样搬成本地文件。
不含界面、队列、浏览器扩展、托盘、通知等任何"软件外壳"。

**路线**：Go 验证 → Rust 重写 → C ABI（编译期吸收进宿主）。
当前处于 **Go 验证完成** 阶段，并已实测跑满服务器带宽上限。

## 目录结构

| 路径 | 说明 |
| --- | --- |
| `core-go/` | Go 验证版（实现 + 测试 + 测试程序），详见 `core-go/README.md` |

## 快速开始

```bash
cd core-go
go test ./...            # 跑测试
go run ./cmd/get         # 交互式下载测试程序
```

编译可执行程序（含版本号注入）：

```powershell
cd core-go
.\build.ps1
```
