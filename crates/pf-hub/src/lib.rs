//! pf-hub 库：注册表 + 扫描器 + 宿舍（spawn）+ WS 路由组装。
//! bin 只负责环境变量解析与启动；库部分供集成测试复用。

pub mod registry;
pub mod scanner;
pub mod spawn;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{Path, State, WebSocketUpgrade},
    http::StatusCode,
    response::Response,
    routing::{delete, get, post},
    Json, Router,
};
use pf_scene::{AgentInfo, SceneEvent};
use registry::Registry;
use spawn::{SayRequest, SpawnManager, SpawnRequest};
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

/// 路由与控制面的完整依赖集。
#[derive(Clone)]
pub struct Hub {
    pub state: AppState,
    pub spawn: Arc<SpawnManager>,
}

impl Hub {
    pub fn new() -> Self {
        let state = AppState::new();
        Self {
            spawn: SpawnManager::new(state.clone()),
            state,
        }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

/// 组装 HTTP/WS 路由。
pub fn router(hub: Hub) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/scene", get(scene_ws))
        .route("/agents", get(list_agents).post(spawn_agent))
        .route("/agents/:id/say", post(say_agent))
        .route("/agents/:id", delete(stop_agent))
        .with_state(hub)
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

async fn list_agents(State(hub): State<Hub>) -> Json<Vec<AgentInfo>> {
    Json(hub.state.registry.read().await.snapshot())
}

async fn spawn_agent(
    State(hub): State<Hub>,
    Json(req): Json<SpawnRequest>,
) -> Result<Json<AgentInfo>, (StatusCode, String)> {
    hub.spawn
        .spawn(req)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

async fn say_agent(
    State(hub): State<Hub>,
    Path(id): Path<String>,
    Json(req): Json<SayRequest>,
) -> Result<&'static str, (StatusCode, String)> {
    hub.spawn
        .say(&id, &req.text)
        .map(|_| "ok")
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

async fn stop_agent(
    State(hub): State<Hub>,
    Path(id): Path<String>,
) -> Result<&'static str, (StatusCode, String)> {
    hub.spawn
        .stop(&id)
        .await
        .map(|_| "ok")
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

async fn scene_ws(State(hub): State<Hub>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        let snapshot = SceneEvent::Snapshot {
            agents: hub.state.registry.read().await.snapshot(),
        };
        handle_socket(socket, hub, snapshot).await;
    })
}

/// WS 会话：下行=场景事件广播；上行=控制面（spawn/say/stop）。
async fn handle_socket(socket: axum::extract::ws::WebSocket, hub: Hub, snapshot: SceneEvent) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    use pf_scene::ClientMessage;
    use tokio::sync::broadcast::error::RecvError;

    let (mut sender, mut receiver) = socket.split();
    let mut rx = hub.state.tx.subscribe();

    // 下行任务：先快照，再转发广播
    let send_task = tokio::spawn(async move {
        if let Ok(json) = serde_json::to_string(&snapshot) {
            if sender.send(Message::Text(json)).await.is_err() {
                return;
            }
        }
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let Ok(json) = serde_json::to_string(&ev) else {
                        continue;
                    };
                    if sender.send(Message::Text(json)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "scene subscriber lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    // 上行循环：控制面
    while let Some(Ok(msg)) = receiver.next().await {
        let Ok(text) = msg.into_text() else {
            continue;
        };
        let Ok(cm) = serde_json::from_str::<ClientMessage>(&text) else {
            continue;
        };
        let result = match cm {
            ClientMessage::Spawn {
                provider,
                cwd,
                argv,
            } => hub
                .spawn
                .spawn(SpawnRequest {
                    provider,
                    cwd,
                    argv,
                })
                .await
                .map(|_| ()),
            ClientMessage::Say { id, text } => hub.spawn.say(&id, &text),
            ClientMessage::Stop { id } => hub.spawn.stop(&id).await,
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "控制面调用失败");
        }
    }
    send_task.abort();
}
