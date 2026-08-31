//! pixel-forge hub：本地 daemon。
//!
//! 职责（M0）：定时扫描 /proc 发现 agent 进程，维护注册表，
//! 通过 WS（/scene）向渲染端广播场景事件。
//!
//! 环境变量：
//! - `PF_PORT`：监听端口，默认 7664
//! - `PF_PROC_ROOT`：proc 根目录，默认 /proc（测试用）

mod registry;
mod scanner;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{State, WebSocketUpgrade},
    response::Response,
    routing::get,
    Router,
};
use pf_scene::SceneEvent;
use registry::Registry;
use tokio::sync::{broadcast, RwLock};

const DEFAULT_PORT: u16 = 7664;
const SCAN_INTERVAL: Duration = Duration::from_secs(2);

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
    let proc_root = PathBuf::from(
        std::env::var("PF_PROC_ROOT").unwrap_or_else(|_| "/proc".into()),
    );

    let (tx, _) = broadcast::channel::<SceneEvent>(64);
    let registry = Arc::new(RwLock::new(Registry::new()));

    tokio::spawn(discovery_loop(proc_root, tx.clone(), registry.clone()));

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/scene", get(scene_ws))
        .with_state(AppState { tx, registry });

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("pixel-forge hub listening on ws://{addr}/scene");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<SceneEvent>,
    registry: Arc<RwLock<Registry>>,
}

async fn discovery_loop(
    proc_root: PathBuf,
    tx: broadcast::Sender<SceneEvent>,
    registry: Arc<RwLock<Registry>>,
) {
    let mut tick = tokio::time::interval(SCAN_INTERVAL);
    loop {
        tick.tick().await;
        let found = scanner::scan(&proc_root);
        let events = registry.write().await.apply_discovered(found);
        for ev in events {
            let _ = tx.send(ev);
        }
    }
}

async fn scene_ws(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        let snapshot = SceneEvent::Snapshot {
            agents: state.registry.read().await.snapshot(),
        };
        handle_socket(socket, state.tx, snapshot).await;
    })
}

async fn handle_socket(
    mut socket: axum::extract::ws::WebSocket,
    tx: broadcast::Sender<SceneEvent>,
    snapshot: SceneEvent,
) {
    use axum::extract::ws::Message;
    use tokio::sync::broadcast::error::RecvError;

    let Ok(json) = serde_json::to_string(&snapshot) else {
        return;
    };
    if socket.send(Message::Text(json)).await.is_err() {
        return;
    }

    let mut rx = tx.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let Ok(json) = serde_json::to_string(&ev) else { continue };
                if socket.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "scene subscriber lagged");
            }
            Err(RecvError::Closed) => break,
        }
    }
}
