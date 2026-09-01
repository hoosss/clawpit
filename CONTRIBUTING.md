# 贡献指南 / Contributing

clawpit 是一个本地优先的像素风 agent 车间：发现你机器上正在跑的 AI 编码 agent，可视化、可指挥、可互发消息。

## 开发环境

```bash
# Rust stable + 构建工具链（Linux 需 gcc；无系统 gcc 时可用用户态 zig cc）
rustup show
cargo build --workspace
cargo test --workspace        # 21+ 个测试（含 pty spawn 与 tmux 集成，tmux 缺失自动跳过）
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

运行：

```bash
cargo run -p clawpit    # daemon：TUI/Web/API 三合一（Web 在 http://localhost:7664）
cargo run -p clawpit-tui    # 终端像素车间
```

## 代码结构

| crate | 职责 |
|-------|------|
| `clawpit-scene` | 场景模型与 WS 线协议（唯一权威定义，只增不改） |
| `clawpit` | daemon：注册表 / 扫描器 / pty 宿主 / 消息总线 / tmux 桥 / MCP server（`clawpit-mcp`） |
| `clawpit-tui` | 终端渲染端 |
| `clawpit-web/` | 单文件 canvas 像素客户端（include_str! 内嵌） |

## 约定

- **协议演进只增不改**：新增字段给默认值，枚举新增成员视为兼容
- **注册表差量规则**：一个 driver 只能增删改自己 `Source` 来源的条目
- 新功能先在 `docs/superpowers/specs/` 更新设计文档，再动代码
- 提交信息用中文 conventional commits（feat/fix/refactor/test/docs）

## 新 provider 适配

1. `clawpit-scene::Provider` 加成员 + `detect()`（argv0 basename 精确匹配，宿主进程名不认，防误报）
2. `clawpit/src/spawn.rs::provider_command` 加默认启动命令
3. （可选）`observe.rs` 加该 provider 的 transcript 状态解析
