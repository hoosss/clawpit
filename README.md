# pixel-forge

> 工作名，正式名字待定 · Rust · MIT/Apache-2.0 待定

本地运行的像素风 **agent 车间**：自动发现你机器上正在跑的 AI 编码 agent（Claude Code / Codex / Gemini / Aider / OpenCode），把它们变成车间里的像素工人。你看得到每只工人的状态，能点名下指令让它继续干活，agent 之间也能通过车间互相喊话。

```
cargo run -p pf-hub    # 终端 1：车间 daemon（默认 ws://127.0.0.1:7664/scene）
cargo run -p pf-tui    # 终端 2：像素车间 TUI，q 退出
```

在别的终端里跑一个 `claude` 或 `codex`，几秒内它会作为一只 worker 出现在车间里；退出即消失。

**M2 已就绪（agent 互聊）**：给 agent CLI 配上我们的 MCP server，它就能跟车间里的同事（和你）对话：

```json
{ "mcpServers": { "pixel-forge": { "command": "/path/to/pixel-forge/target/debug/pf-mcp" } } }
```

工具：`pf_list`（看同事）、`pf_send(to, text)`（发消息——hub 宿主的同事会直接收到，外面的进收件箱）、`pf_inbox`（取自己的信）。所有对话以气泡显示在 TUI 的"车间消息"墙。人也可以直接插话：

```bash
curl -X POST localhost:7664/msg -H 'content-type: application/json' \
  -d '{"from_pid":null,"to":"cc-1234","text":"复审通过，合并吧"}'
```

**M1（指挥）**：`j/k` 选中 worker，`⏎` 输入一句话回车——这句话会写进那个 agent 会话的 stdin；`n` 招一只新 claude worker，`x` 解雇选中者。也可以走 HTTP：

```bash
curl -X POST localhost:7664/agents -H 'content-type: application/json' -d '{"provider":"claude_code"}'
curl -X POST localhost:7664/agents/sp-1/say -H 'content-type: application/json' -d '{"text":"继续干活"}'
curl -X DELETE localhost:7664/agents/sp-1
```

## 架构

```
pf-tui (ratatui) ─┐                       ┌─ Web (Vue3+canvas, 规划中)
                  ├─ WS 场景协议(pf-scene) ┤
                  └───────────────────────└─ Tauri 桌面 (规划中)
                              │
                        pf-hub daemon
              AgentRegistry · 广播 · 消息总线(规划中)
                              │
              SessionDriver: 观察站(扫描) · 宿舍(pty) · 邮局(MCP)
```

- **观察站**：扫描 /proc 发现存量 agent 进程（当前）
- **宿舍**：hub 自己 spawn 的 agent，stdin 注入指挥（下一步）
- **邮局**：MCP 消息总线，agent 之间互相发指令（差异化核心）

设计文档：`docs/superpowers/specs/2026-08-31-pixel-forge-design.md` · 路线图与进度：`docs/plan.md`

## 状态

M0（地基）实施中。路线图：M0 地基 → M1 宿舍 → M2 邮局 → M3 观察站深化 → M4 Web → M5 桌面+社区。
