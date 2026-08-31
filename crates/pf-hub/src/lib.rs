//! pf-hub 库：注册表 + 扫描器 + WS 路由组装。
//! bin 只负责环境变量解析与启动；库部分供集成测试复用。

pub mod registry;
pub mod scanner;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{State, WebSocketUpgrade},
    response::Response,
    routing::get,
    Router,
};
use pf_scene::SceneEvent;
use registry::Registry;
use tokio::sync::{broadcast, RwLock};

/// 默认监听端口。
pub const DEFAULT_PORT: u16 = 7664;
/// 默认扫描周期。
pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// hub 共享状态。
#[derive(Clone)]
pub struct AppState {
    pub tx: broadcast::Sender<SceneEvent>,
    pub registry: Arc<RwLock<Registry>>,
}

impl AppState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            tx,
            registry: Arc::new(RwLock::new(Registry::new())),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// 组装 HTTP/WS 路由。
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/scene", get(scene_ws))
        .with_state(state)
}

/// 扫描循环：周期扫描 → 注册表差量 → 广播事件。
/// bin 用真实 /proc 与默认周期；测试直接调 registry 不经过这里。
pub async fn discovery_loop(proc_root: PathBuf, state: AppState, interval: Duration) {
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        let found = scanner::scan(&proc_root);
        let events = state.registry.write().await.apply_discovered(found);
        for ev in events {
            let _ = state.tx.send(ev);
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
                let Ok(json) = serde_json::to_string(&ev) else {
                    continue;
                };
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
