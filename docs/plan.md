# 实施计划（loop tick 从这里续作）

> 规则：每个 `/loop` tick 按下面的清单继续；做完一项勾一项并 commit。
> 顺序执行，不要跳站。M0 完成后在此文件追加 M1 细化清单。

## M0 地基

- [x] 需求澄清 + 架构决策（见设计文档）
- [x] 仓库 init（main 分支）
- [x] workspace 骨架：pf-scene / pf-hub / pf-tui
- [x] 设计文档落盘：docs/superpowers/specs/2026-08-31-pixel-forge-design.md
- [x] rustup 安装（rsproxy.cn 镜像；crates.io 镜像见 ~/.cargo/config.toml）
- [x] pf-hub 重构 lib+bin（集成测试可复用）+ WS 全链路集成测试已写
- [x] 编译链打通：系统无 gcc → 用户态 zig cc（~/tools/zig-cc，CC + CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER）
- [x] `cargo check` 全绿（注意：查输出用 pipefail，别让 tail 吃掉错误码）
- [x] `cargo test` 全绿：7/7（注册表差量 3 / 扫描器 fixture 1 / 协议 roundtrip 2 / WS 集成冒烟 1）
- [x] clippy 0 警告 + fmt 通过
- [x] hub 二进制冒烟：假 /proc + 自定义端口，启动/监听//health/干净退出 OK
- [x] M0 收尾 commit + tag `v0.1.0-m0`
- [ ] 待用户：真终端里 `cargo run -p pf-hub` + `cargo run -p pf-tui`，开个 claude 进程看 worker 出现/消失（TUI 需要真实 TTY，无法自动化验证）

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
