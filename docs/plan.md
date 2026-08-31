# 实施计划（loop tick 从这里续作）

> 规则：每个 `/loop` tick 按下面的清单继续；做完一项勾一项并 commit。
> 顺序执行，不要跳站。M0 完成后在此文件追加 M1 细化清单。

## M0 地基

- [x] 需求澄清 + 架构决策（见设计文档）
- [x] 仓库 init（main 分支）
- [x] workspace 骨架：pf-scene / pf-hub / pf-tui
- [x] 设计文档落盘：docs/superpowers/specs/2026-08-31-pixel-forge-design.md
- [ ] rustup 安装完成（后台进行中）
- [ ] `cargo check` 全绿（首次拉依赖较慢）
- [ ] `cargo test` 全绿（注册表差量 / 扫描器 fixture / 协议 roundtrip）
- [ ] 冒烟：起 hub，另开终端跑 `claude --version` 类进程（或临时伪造 PF_PROC_ROOT），TUI 能看到 worker 出现/消失
- [ ] clippy + fmt
- [ ] M0 收尾 commit + tag `v0.1.0-m0`

## M1 宿舍（M0 完成后细化）

- [ ] portable-pty 依赖引入 + SpawnDriver 骨架
- [ ] spawn 配置（provider 命令行模板）
- [ ] stdin 注入 API + TUI 选中 worker 输入
- [ ] 生命周期（start/stop/restart）+ 状态映射 Working/Done

## M2 邮局 / M3 观察站 / M4 Web / M5 桌面

后续站在各自开工前细化。
