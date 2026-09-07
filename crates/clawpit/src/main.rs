//! clawpit hub bin：环境变量解析 + 启动。

use std::{net::SocketAddr, path::PathBuf};

use clawpit::{discovery_loop, router, DEFAULT_PORT, DEFAULT_SCAN_INTERVAL};

/// 环境变量：
/// - `CLAWPIT_PORT`：监听端口，默认 7664
/// - `CLAWPIT_PROC_ROOT`：proc 根目录，默认 /proc（测试用）
/// - `CLAWPIT_HOME`：持久化目录，默认 ~/.clawpit（events.jsonl 事件日志）
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `clawpit mcp`：不启 daemon，打印把 clawpit-mcp 接入各 agent CLI 的现成配置。
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        print_mcp_guide();
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
    tokio::spawn(discovery_loop(
        proc_root,
        claude_home,
        hub.state.clone(),
        DEFAULT_SCAN_INTERVAL,
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

/// `clawpit mcp`：一键接入指引。mcp 路径取当前可执行文件的同级 clawpit-mcp
/// （cargo build 后两者总在一起）；找不到就退化为裸名，交给 PATH。
fn print_mcp_guide() {
    let mcp = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("clawpit-mcp")))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "clawpit-mcp".into());
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
