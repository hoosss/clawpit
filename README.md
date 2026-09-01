<div align="center">

# clawpit

**像素风本地 agent 车间 —— 发现、指挥、对话你机器上的 AI 编码 agent**

*A pixel-art pit crew for the AI coding agents already running on your machine.*

[![CI](https://github.com/hoosss/clawpit/actions/workflows/ci.yml/badge.svg)](https://github.com/hoosss/clawpit/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-stable-b7410e.svg)](https://www.rust-lang.org)

![demo](docs/demo.gif)

*画面里全是真实事件：紫色小人 = 正在运行的 Claude Code 会话（⚒=干活 / zZ=等输入），灰色的 sp-1 是演示中现场招的新 worker，白气泡是真实投递的消息。*

</div>

---

## 它解决什么问题

你开着好几个终端跑 Claude Code / Codex / Gemini，但它们互相看不见、你也没法一眼看清谁在干嘛。clawpit 是一个本地 daemon：

| 能力 | 说明 |
|------|------|
| 🔍 **发现** | 自动感知本机正在运行的 agent 会话（/proc 扫描），手动按 session id 导入历史会话也行 |
| 👁 **观察** | 解析 Claude Code transcript 推断**真实状态**：在想 / 在调工具 / 在等你说话 |
| 🗣 **指挥** | 对任意 agent 喊一句话——hub 宿主的写 stdin，外部 tmux 会话走 send-keys，真身继续干活 |
| 💬 **中转** | agent 之间通过内置 MCP server 互相发消息（reviewer↔fixer 闭环的地基），对话以气泡上墙 |
| 🖥 **三端** | TUI（ratatui）· Web 像素车间（浏览器开箱即用）· HTTP API，同一套场景协议 |

## 快速开始

```bash
git clone https://github.com/hoosss/clawpit && cd clawpit
cargo run -p clawpit      # 终端 1：daemon（TUI + Web + API 三合一）
cargo run -p clawpit-tui  # 终端 2：像素车间，q 退出
```

浏览器打开 **http://localhost:7664** 就是上面的像素车间。在任意终端跑一个 `claude`，几秒内它会作为一只 worker 走进画面，退出即消失。

**TUI 按键**：`j/k` 选人 · `⏎` 喊话 · `n` 招工 · `x` 解雇 · `q` 退出

**HTTP**：

```bash
curl -X POST localhost:7664/agents -H 'content-type: application/json' \
  -d '{"provider":"claude_code"}'                # 招一只 claude
curl -X POST localhost:7664/agents/sp-1/say -H 'content-type: application/json' \
  -d '{"text":"继续干活"}'                        # 对它喊话
curl -X POST localhost:7664/msg -H 'content-type: application/json' \
  -d '{"from_pid":null,"to":"cc-1234","text":"复审通过"}'   # 走邮局（带气泡）
```

## 让 agent 互相聊天（MCP）

给你的 agent CLI 配一次 clawpit 的 MCP server，它就获得 `clawpit_list` / `clawpit_send` / `clawpit_inbox` 三个工具：

```json
{
  "mcpServers": {
    "clawpit": { "command": "/path/to/clawpit/target/debug/clawpit-mcp" }
  }
}
```

身份零配置：MCP 进程用父 pid 自报——谁拉起我，我就是谁。之后 reviewer 完成后一句 `clawpit_send("fixer", "清单已出，开工")`，消息直接出现在对方会话里（或收件箱），气泡同时上墙。

## 架构

```
clawpit-tui (ratatui) ─┐                      ┌─ HTTP API
                      ├─ WS 场景协议 ─────────┤
clawpit-web (canvas) ──┘  (clawpit-scene)     └─ clawpit-mcp (MCP server)
                              │
                        clawpit daemon
        注册表 · 扫描器 · pty 宿主 · 消息总线 · tmux 桥 · 观察站
```

| crate | 职责 |
|-------|------|
| `clawpit-scene` | 场景模型与 WS 线协议（只增不改） |
| `clawpit` | daemon：`clawpit`（hub）+ `clawpit-mcp`（MCP server）两个二进制 |
| `clawpit-tui` | 终端渲染端 |
| `clawpit-web/` | 单文件 canvas 像素车间，`include_str!` 编译期内嵌，零构建 |

注入路由：hub 宿主 → 写 stdin；外部会话在 tmux 里 → send-keys；其余 → 收件箱（取走即清）。

## 路线图

- [x] **M0** 发现 + 状态墙 + 场景协议
- [x] **M1** pty 宿主 spawn + stdin 指挥
- [x] **M2** MCP 消息总线，agent 互聊
- [x] **M3** 真实状态解析（transcript）+ tmux 注入 + session 导入
- [x] **M4** Web 像素车间
- [x] **M5** 开源化（双协议 / CI / 贡献指南）
- [ ] Tauri 桌面壳（按需）· crates.io 发布 · 更多 provider 的状态解析

## 贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)——包括新 provider 的适配三步法。

## License

MIT OR Apache-2.0，双协议任选，见 [LICENSE-MIT](LICENSE-MIT) / [LICENSE-APACHE](LICENSE-APACHE)。
