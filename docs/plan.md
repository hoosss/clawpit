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

## M1 宿舍（M0 完成后开工）

- [ ] portable-pty 依赖引入 + SpawnDriver 骨架（实现 SessionDriver trait 的第二个实例）
- [ ] SessionDriver trait 正式抽象（M0 的扫描逻辑收编为 ObserveDriver）
- [ ] spawn 配置（provider 命令行模板，如 claude → `claude --no-session-persistence` 类启动参数）
- [ ] stdin 注入 API（HTTP POST /agents/{id}/say + TUI 选中 worker 输入框）
- [ ] 生命周期（start/stop/restart）+ 状态映射（进程活着=Working，退出码=Done/Error）
- [ ] 验收：TUI 里 spawn 一只 claude worker，输入指令，看到它真的干活

## M2 邮局 / M3 观察站 / M4 Web / M5 桌面

后续站在各自开工前细化。
