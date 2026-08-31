# pixel-forge 设计文档

> 工作名 pixel-forge，可随时整体改名（crate 前缀 `pf-` 一并替换）。
> 状态：M0 实施中。本文是产品的唯一权威设计来源，路线图每站开工前先更新这里。

## 一句话

本地运行的像素风「agent 车间」：自动发现你机器上正在跑的 AI 编码 agent，把它们变成车间里的像素工人——你看得到每只工人的状态，能点名下指令让它继续干活，agent 之间也能通过车间互相喊话。

## 定位与灵魂（已确认的决策）

| 决策点 | 结论 |
|--------|------|
| 产品魂 | **游戏化体验优先**：看着小人干活本身就是产品，编排能力最小可用 |
| 真实性 | **真实 agent 驱动**：小人搬的每个箱子都是真实的文件改动/提交，不是演出 |
| 场景 | 本地优先：发现正在运行的终端 agent + 手动导入 session id |
| 形态 | **TUI 先行**；Web、桌面是既定路线，架构从第一天就分层 |
| provider | **多 provider 从一开始**（Claude Code / Codex / Gemini / Aider / OpenCode） |
| 语言 | **Rust** |
| 架构 | **混合三 driver**：邮局（MCP 消息核心）+ 宿舍（pty 宿主）+ 观察站（扫描） |

## 总体架构

```
┌─────────────────────────── 渲染端（可插拔） ───────────────────────────┐
│  pf-tui (ratatui, M0)   │   Web (Vue3+canvas, M4)   │  Tauri (M5)    │
└───────────────┬──────────────────────────────────────────┬────────────┘
                │              WS 场景协议 (pf-scene)        │
┌───────────────┴──────────────────────────────────────────┴────────────┐
│                        pf-hub（本地 daemon）                           │
│  ┌────────────┐  ┌──────────────┐  ┌──────────────────────────────┐  │
│  │ AgentRegistry│  │ 场景广播(WS) │  │        消息总线 (M2)          │  │
│  └──────┬─────┘  └──────────────┘  │  MCP server: register/send/  │  │
│         │                          │  inbox/list                  │  │
│  ┌──────┴───────────────────────────────────────────────────────┐  │
│  │                    SessionDriver trait                        │  │
│  ├──────────────┬──────────────────┬────────────────────────────┤  │
│  │ 观察站(M3)    │ 宿舍(M1)          │ 邮局(M2)                    │  │
│  │ procfs 扫描   │ pty spawn        │ agent 侧装 MCP 主动注册      │  │
│  │ transcript尾  │ stdin 注入        │ 收件箱拉取                   │  │
│  │ tmux 注入     │ 生命周期管理       │ agent↔agent 中转             │  │
│  └──────────────┴──────────────────┴────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```

### 核心概念

- **Agent**：一个正在运行的 agent 会话，有全局 id、provider、显示名、状态、来源。
- **Provider**：ClaudeCode / Codex / Gemini / Aider / OpenCode / Generic。
- **Source**：进入车间的方式——`Discovered`(扫描) / `Spawned`(hub 宿主) / `Registered`(MCP 自报) / `Imported`(手动导入)。**注册表差量规则：一个 driver 只能增删改自己来源的条目。**
- **AgentState**：Unknown / Thinking / Working / WaitingInput / Done / Error。
- **场景协议**：hub→渲染端的单向 WS JSON 事件流（`pf-scene` crate 定义），连接时 `Snapshot` 全量，之后 `AgentUpsert` / `AgentGone` 增量。演进规则：只增不改。

### SessionDriver 能力矩阵（目标态）

| driver | discover | observe | inject | message | 覆盖场景 |
|--------|----------|---------|--------|---------|----------|
| 观察站 | ✅ procfs+tmux | ⚠️ transcript 解析 | ⚠️ 仅 tmux | — | 存量正在跑的会话 |
| 宿舍 | ✅ 自己 spawn 的 | ✅ 读 stdout 流 | ✅ 写 stdin | ✅ | hub 编排（reviewer/fixer 闭环） |
| 邮局 | ⚠️ 装 MCP 的才可见 | ✅ agent 自报 | ⚠️ 拉取式有延迟 | ✅ 最强 | 跨 provider 公共面 |

## 路线图

每站独立交付价值；M1/M2/M3 只依赖 M0，可调序。

- **M0 地基（当前）**：workspace + 注册表 + 场景协议 v0 + ratatui 状态墙 + procfs 扫描 v0（argv[0] 识别，状态 Unknown）。验收：起 hub + TUI，能看到本机正在跑的 claude/codex 进程出现/消失。
- **M1 宿舍**：portable-pty spawn、stdin 注入、TUI 选中 worker 输入指令。
- **M2 邮局**：MCP server + 消息总线 + agent↔agent 中转 + 对话气泡进场景协议。
- **M3 观察站**：transcript tail 解析（真状态）、tmux 清点与 send-keys 注入、session id 导入。
- **M4 Web 端**：同一场景协议驱动 Vue3 + canvas 真像素美术。
- **M5 桌面+社区**：Tauri 壳、provider 插件指南、文档、发版。

## M0 细节

- **端口**：默认 7664（`PF_PORT` 覆盖）；`PF_PROC_ROOT` 可换 proc 根供测试。
- **扫描**：2s 一轮；只认 cmdline argv[0] basename 精确命中（claude/codex/gemini/aider/opencode）；node/python 宿主不认，防误报。id 规则 `{provider.short}-{pid}`。
- **错误处理**：扫描失败静默返回空（下轮重试）；WS 订阅者 lag 只告警不断线；hub 不因单个渲染端崩溃退出。
- **测试**：注册表差量逻辑（upsert/quiet/gone/不越权）、扫描器用 tempdir 伪造 /proc fixture、协议 serde roundtrip。集成冒烟：起 hub 连 WS 断言事件。

## 非目标（当前版本明确不做）

- 远程/多机 agent、账号系统、协作层
- 调度策略/任务队列（编排只做"人下指令 + agent 互发"，不做自动派单）
- 修改任何 agent 的内部行为

## 开放问题

- [ ] 正式命名（现用 pixel-forge）
- [ ] 开源协议（建议 MIT 或 Apache-2.0，待定）
- [ ] Web 端像素美术风格（M4 前定）
