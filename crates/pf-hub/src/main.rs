//! pixel-forge hub bin：环境变量解析 + 启动。

use std::{net::SocketAddr, path::PathBuf};

use pf_hub::{discovery_loop, router, AppState, DEFAULT_PORT, DEFAULT_SCAN_INTERVAL};

/// 环境变量：
/// - `PF_PORT`：监听端口，默认 7664
/// - `PF_PROC_ROOT`：proc 根目录，默认 /proc（测试用）
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pf_hub=info".into()),
        )
        .init();

    let port: u16 = std::env::var("PF_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let proc_root = PathBuf::from(std::env::var("PF_PROC_ROOT").unwrap_or_else(|_| "/proc".into()));

    let state = AppState::new();
    tokio::spawn(discovery_loop(
        proc_root,
        state.clone(),
        DEFAULT_SCAN_INTERVAL,
    ));

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("pixel-forge hub listening on ws://{addr}/scene");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
