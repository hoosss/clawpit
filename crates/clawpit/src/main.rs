//! clawpit hub bin：环境变量解析 + 启动。

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use clawpit::{discovery_loop, router, DEFAULT_PORT, DEFAULT_SCAN_INTERVAL};

/// 环境变量：
/// - `CLAWPIT_PORT`：监听端口，默认 7664
/// - `CLAWPIT_PROC_ROOT`：proc 根目录，默认 /proc（测试用）
/// - `CLAWPIT_HOME`：持久化目录，默认 ~/.clawpit（events.jsonl 事件日志）
/// - `CLAWPIT_AGENTS`：自定义 agent 可执行名（逗号分隔），如 `zcode,qwen-code`
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `clawpit mcp`：不启 daemon。`clawpit mcp install` 一键接入（用户级配置，零 sudo）；
    // 其余子命令打印手动配置片段。
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        match std::env::args().nth(2).as_deref() {
            Some("install") => {
                if let Err(e) = mcp_install() {
                    eprintln!("clawpit mcp install: {e}");
                    std::process::exit(1);
                }
            }
            _ => print_mcp_guide(),
        }
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clawpit=info".into()),
        )
        .init();

    let port: u16 = std::env::var("CLAWPIT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let proc_root =
        PathBuf::from(std::env::var("CLAWPIT_PROC_ROOT").unwrap_or_else(|_| "/proc".into()));
    let claude_home = PathBuf::from(
        std::env::var("CLAWPIT_CLAUDE_HOME").unwrap_or_else(|_| format!("{}/.claude", home_dir())),
    );

    // 持久化：CLAWPIT_HOME 覆盖（测试用），默认 ~/.clawpit；
    // 打开失败（磁盘/权限）降级为内存模式，本地工具不因小事故罢工
    let store_dir = PathBuf::from(
        std::env::var("CLAWPIT_HOME").unwrap_or_else(|_| format!("{}/.clawpit", home_dir())),
    );
    let hub = match clawpit::Hub::with_persistence(&store_dir) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, home = %store_dir.display(), "持久化不可用，降级为内存模式");
            clawpit::Hub::new()
        }
    };
    // CLAWPIT_AGENTS：自定义 agent 可执行名名单（逗号分隔）——
    // zcode / qwen-code 这类长尾 CLI 零改码即可被发现收编（显示名 zc-<pid>）
    let extra_agents: Vec<String> = std::env::var("CLAWPIT_AGENTS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    tokio::spawn(discovery_loop(
        proc_root,
        claude_home,
        hub.state.clone(),
        DEFAULT_SCAN_INTERVAL,
        extra_agents,
    ));

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("clawpit hub listening on ws://{addr}/scene");
    axum::serve(listener, router(hub)).await?;
    Ok(())
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".into())
}

/// `clawpit mcp install`：把 clawpit-mcp 接进本机各家 agent CLI 的用户级配置。
/// 全程用户态（写自己的配置文件 / 调 claude 自己的 mcp add），不需要任何特权——
/// 这是"任意平台可用"的投递通道：收件箱拉模式 + 名册/发信工具。
fn mcp_install() -> anyhow::Result<()> {
    let mcp = mcp_bin_path();
    let home = home_dir();

    // ① Claude Code：走它自己的 CLI（用户作用域，幂等由 claude 处理）
    match std::process::Command::new("claude")
        .args(["mcp", "add", "-s", "user", "clawpit", "--", &mcp])
        .output()
    {
        Ok(out) if out.status.success() => {
            println!("✓ Claude Code：clawpit 已加入用户级 MCP（新开的 claude 会话生效）")
        }
        Ok(out) => println!(
            "· Claude Code：claude mcp add 返回非零（可能已配置过）：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(_) => {
            println!("· 未找到 claude CLI，跳过；手动：claude mcp add -s user clawpit -- {mcp}")
        }
    }

    // ② Codex：往 ~/.codex/config.toml 追加一节（已有则不动，幂等）
    let codex_cfg = PathBuf::from(&home).join(".codex/config.toml");
    if ensure_codex_config(&codex_cfg, &mcp)? {
        println!(
            "✓ Codex：~/.codex/config.toml 已写入 [mcp_servers.clawpit]（新开的 codex 会话生效）"
        );
    } else {
        println!("· Codex：config.toml 里已有 clawpit，跳过");
    }

    println!(
        "\n收件箱是拉模式：agent 调 clawpit_inbox 时取信。即发即达走「招工」（hub 直管）或 tmux。"
    );
    Ok(())
}

/// 幂等地往 codex 配置追加 clawpit MCP 节；返回是否实际写入（纯文件操作，好测）。
fn ensure_codex_config(cfg: &Path, mcp: &str) -> anyhow::Result<bool> {
    let marker = "[mcp_servers.clawpit]";
    let existing = std::fs::read_to_string(cfg).unwrap_or_default();
    if existing.contains(marker) {
        return Ok(false);
    }
    if let Some(dir) = cfg.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let section = format!("\n{marker}\ncommand = \"{mcp}\"\n");
    let mut content = existing;
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&section);
    std::fs::write(cfg, content)?;
    Ok(true)
}

/// mcp 路径取当前可执行文件的同级 clawpit-mcp（cargo build 后两者总在一起）；
/// 找不到就退化为裸名，交给 PATH。
fn mcp_bin_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("clawpit-mcp")))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "clawpit-mcp".into())
}

/// `clawpit mcp`：一键接入指引。
fn print_mcp_guide() {
    let mcp = mcp_bin_path();
    println!("把 clawpit 接入你的 agent CLI（配一次，新开的会话即有 clawpit_list/send/inbox）：\n");
    println!("  Claude Code:");
    println!("    claude mcp add clawpit -- {mcp}\n");
    println!("  Codex (~/.codex/config.toml):");
    println!("    [mcp_servers.clawpit]\n    command = \"{mcp}\"\n");
    println!("  通用 MCP（stdio）:");
    println!("    {{\"mcpServers\":{{\"clawpit\":{{\"command\":\"{mcp}\"}}}}}}\n");
    println!("注意：收件箱是拉模式——agent 调 clawpit_inbox 才取到信。要消息即发即达，");
    println!("把终端跑在 tmux 里（send-keys 注入）或用「招工」让 hub 直管（stdin 注入）。");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// codex 配置追加必须幂等：已有节不重写，没有则规范追加（保留原有内容）
    #[test]
    fn codex_config_append_is_idempotent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cfg = dir.path().join(".codex/config.toml");

        // 首次：空文件 → 写入节
        assert!(ensure_codex_config(&cfg, "/x/clawpit-mcp")?);
        let content = std::fs::read_to_string(&cfg)?;
        assert!(content.contains("[mcp_servers.clawpit]"));
        assert!(content.contains("command = \"/x/clawpit-mcp\""));

        // 二次：已存在 → 不动（内容不变）
        assert!(!ensure_codex_config(&cfg, "/x/clawpit-mcp")?);
        assert_eq!(std::fs::read_to_string(&cfg)?, content);

        // 已有其他配置的文件：原内容完整保留，追加在尾部
        let cfg2 = dir.path().join(".codex2/config.toml");
        std::fs::create_dir_all(cfg2.parent().unwrap())?;
        std::fs::write(&cfg2, "model = \"gpt-5\"\n")?;
        assert!(ensure_codex_config(&cfg2, "/x/clawpit-mcp")?);
        let c2 = std::fs::read_to_string(&cfg2)?;
        assert!(c2.starts_with("model = \"gpt-5\"\n"));
        assert!(c2.contains("[mcp_servers.clawpit]"));
        Ok(())
    }
}
