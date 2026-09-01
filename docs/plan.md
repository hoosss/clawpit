# 实施计划（loop tick 从这里续作）

> 规则：每个 `/loop` tick 按下面的清单继续；做完一项勾一项并 commit。
> **2026-08-31：M0-M5 代码工作全部完成，循环已停（剩余事项需用户决策，见 M5）。**

## M5 桌面+社区（代码工作 ✅ 完成，v0.5.0-m5）

- [x] 开源硬化：LICENSE-MIT + LICENSE-APACHE（双协议）、Cargo 元数据（license/description/version 0.5.0）
- [x] CI：GitHub Actions（fmt + clippy -D warnings + test）
- [x] CONTRIBUTING.md（环境/结构/约定/provider 适配指南）
- [x] README 双语化
- [ ] **等用户**：① 正式项目名（现工作名 clawpit）② 确认双协议 ③ GitHub 建仓发布（`git remote add` + push 需你操作或授权）
- [ ] Tauri 桌面壳：Web 端已内嵌，桌面化价值边际较低且需 npm 工具链——**建议降级为按需项**，想要再说
- [ ] crates.io 发布：等定名后 `cargo publish`（工作名下发布不可逆，绝不先斩后奏）

## M0 地基

- [x] 需求澄清 + 架构决策（见设计文档）
- [x] 仓库 init（main 分支）
- [x] workspace 骨架：clawpit-scene / clawpit / clawpit-tui
- [x] 设计文档落盘：docs/superpowers/specs/2026-08-31-clawpit-design.md
- [x] rustup 安装（rsproxy.cn 镜像；crates.io 镜像见 ~/.cargo/config.toml）
- [x] clawpit 重构 lib+bin（集成测试可复用）+ WS 全链路集成测试已写
- [x] 编译链打通：系统无 gcc → 用户态 zig cc（~/tools/zig-cc，CC + CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER）
- [x] `cargo check` 全绿（注意：查输出用 pipefail，别让 tail 吃掉错误码）
- [x] `cargo test` 全绿：7/7（注册表差量 3 / 扫描器 fixture 1 / 协议 roundtrip 2 / WS 集成冒烟 1）
- [x] clippy 0 警告 + fmt 通过
- [x] hub 二进制冒烟：假 /proc + 自定义端口，启动/监听//health/干净退出 OK
- [x] M0 收尾 commit + tag `v0.1.0-m0`
- [ ] 待用户：真终端里 `cargo run -p clawpit` + `cargo run -p clawpit-tui`，开个 claude 进程看 worker 出现/消失（TUI 需要真实 TTY，无法自动化验证）

## M4 Web 像素车间（✅ 代码完成，v0.5.0-m4）

- [x] clawpit-web/index.html：单文件零构建 canvas 客户端（vanilla JS，编译期内嵌进 hub，`cargo run -p clawpit` 后浏览器开 http://localhost:7664 即用）
- [x] 像素感：480x270 低分辨率画布 + CSS pixelated 放大；工人 = 程序化 16px 小人（provider 配色安全帽、干活时挥手弹跳、睡觉 zZ、出错 X!）
- [x] 同一 WS 场景协议；点击画布/列表选人、说话（POST /msg，气泡上墙+接收者连线）、招工、解雇
- [x] Vue3 工程化（vite）明确推迟：UI 复杂后再迁移，单文件当前最优（YAGNI）
- [x] JS 语法校验通过；hub 实测在服
- [ ] 待用户：Windows 浏览器打开 http://localhost:7664 视觉验收（WSL2 localhost 自动转发）；自动化浏览器验证被系统缺 libnss3 挡住（与 gcc 同一堵 sudo 墙）

## M3 观察站（✅ 完成，v0.4.0-m3）

- [x] observe.rs 真实状态：fd 链接定位 + cwd→slug 推导（真机验证：claude 不保持 fd 打开，cwd 兜底是主路径）+ tail 8KB 事件解析（user=Thinking / tool_use=Working / end_turn=WaitingInput）
- [x] 已知局限：同 cwd 并发多会话无法区分（都显示最新那份的状态）——记录在案，后续用进程启动时间对齐
- [x] tmux.rs 外部注入：/proc 父链（PPid）匹配 pane shell → send-keys -l；tmux 缺失自动跳过
- [x] 统一 say 路由：宿主写 stdin / 外部 tmux / 其余进收件箱（mail 与 /say、WS 同路）
- [x] POST /agents/import：session id → claude --resume 招回历史会话
- [x] 测试 21/21 + clippy 0 + **真机终验**（5 个运行中 claude：4 working / 1 waiting_input，实时准确）

## M2 邮局（✅ 完成，v0.3.0-m2）

- [x] mail.rs 消息总线：活 worker 直接注入（[from X] 前缀）/ 外部或已退出进收件箱（取走即清）/ 发 human 只上墙
- [x] clawpit-mcp 二进制：手写 MCP stdio 协议（ndjson JSON-RPC，零新依赖），clawpit_list/clawpit_send/clawpit_inbox 三工具，父 pid 自报身份（discovered/spawned 通吃）
- [x] 协议：ChatMessage + SceneEvent::Chat（气泡）；Source::Spawned 携带 pid
- [x] API：POST /msg、GET /inbox?pid=
- [x] TUI：车间消息墙（最近气泡）
- [x] 测试 16/16 + clippy 0 警告 + MCP 真协议冒烟（initialize/tools/call 全通过）
- [ ] 待用户：给 claude 配 MCP（`{"mcpServers":{"clawpit":{"command":"<path>/clawpit-mcp"}}}`）后让两只 agent 互发消息真实验收

## M1 宿舍（✅ 完成，v0.2.0-m1）

- [x] portable-pty 宿主 spawn（stdout 排水防堵死、stdin 注入 `\r` 提交）
- [x] spawn 配置：provider_command 默认命令 + argv 覆盖（测试/特殊场景）
- [x] 注入 API 双通道：WS ClientMessage（spawn/say/stop）+ HTTP（POST /agents、POST /agents/:id/say、DELETE /agents/:id、GET /agents）
- [x] 生命周期 + 状态映射：活着=Working，退出码 0=Done / 非 0=Error；退出后条目保留、显式 stop 才移除
- [x] TUI：j/k 选人（▶高亮）、⏎ 喊话（底部输入行）、n 招工(claude)、x 解雇
- [x] 测试 11/11 + clippy 0 警告 + 二进制冒烟（真实发现 5 个 claude 进程 + spawn/say/stop 全链路）
- [x] SessionDriver trait 正式抽象 → **推迟到 M3**（与 transcript 观察一起统一，避免过早抽象；当前 SpawnManager/扫描器各自清晰）
- [ ] 待用户真终端验收：n 招一只真 claude worker，⏎ 发指令看它干活

## M2 邮局 / M3 观察站 / M4 Web / M5 桌面

后续站在各自开工前细化。

## 评审循环终报（2026-09-01，循环已停 ca63f0af）

- 三轮全维度评审（并发注入 / MCP+Web / TUI）：36 项发现 → 27 项为真全部修复，5 项低危记录取舍，4 项复核不成立
- 两轮变异验证：7 个突变体 6 杀 1 冗余（from_utf8_lossy 与行对齐 drain 重复防御，无害）
- 测试 22 → 30（含 WS Lagged 自愈洪水测试）；clippy 持续 0；CI 全绿
- 已知未自动化：Web 端 JS（人工+GIF 验证）；WS 上行 sink.send 失败丢队列消息（低危）；本地 API 无鉴权（local-first 取舍）
- 恢复方式：`/loop 5m 不断测试代码 和code-review`——从本文件续作
