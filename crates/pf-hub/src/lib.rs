//! pf-hub 库：注册表 + 扫描器 + 宿舍（spawn）+ WS 路由组装。
//! bin 只负责环境变量解析与启动；库部分供集成测试复用。

pub mod mail;
pub mod observe;
pub mod registry;
pub mod scanner;
pub mod spawn;
pub mod tmux;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::Response,
    routing::{delete, get, post},
    Json, Router,
};
use pf_scene::{AgentInfo, ChatMessage, SceneEvent};
use registry::Registry;
use spawn::{SayRequest, SpawnManager, SpawnRequest};
use tokio::sync::{broadcast, RwLock};

use mail::{InboxResponse, MailManager};

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
    pub mail: Arc<MailManager>,
}

impl Hub {
    pub fn new() -> Self {
        let state = AppState::new();
        let spawn = SpawnManager::new(state.clone());
        let mail = MailManager::new(state.clone(), spawn.clone());
        Self { mail, spawn, state }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

/// Web 车间（单文件像素客户端，编译期内嵌，零构建）。
const INDEX_HTML: &str = include_str!("../../../pf-web/index.html");

/// 组装 HTTP/WS 路由。
pub fn router(hub: Hub) -> Router {
    Router::new()
        .route("/", get(|| async { axum::response::Html(INDEX_HTML) }))
        .route("/health", get(|| async { "ok" }))
        .route("/scene", get(scene_ws))
        .route("/agents", get(list_agents).post(spawn_agent))
        .route("/agents/import", post(import_agent))
        .route("/agents/:id/say", post(say_agent))
        .route("/agents/:id", delete(stop_agent))
        .route("/msg", post(send_msg))
        .route("/inbox", get(inbox))
        .with_state(hub)
}

/// 扫描循环：周期扫描 → 注册表差量 → 观察真实状态 → 广播事件。
/// bin 用真实 /proc 与默认周期；测试直接调 registry 不经过这里。
pub async fn discovery_loop(
    proc_root: PathBuf,
    claude_home: PathBuf,
    state: AppState,
    interval: Duration,
) {
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        let found = scanner::scan(&proc_root);
        let mut reg = state.registry.write().await;
        let mut events = reg.apply_discovered(found);
        // 观察站：给 discovered 的 claude 会话读真实状态（transcript tail）
        let states: Vec<(String, pf_scene::AgentState)> = reg
            .snapshot()
            .iter()
            .filter_map(|a| match (a.provider, &a.source) {
                (pf_scene::Provider::ClaudeCode, pf_scene::Source::Discovered { pid }) => Some((
                    a.id.clone(),
                    observe::read_state(&proc_root, *pid, &claude_home),
                )),
                _ => None,
            })
            .collect();
        events.extend(reg.apply_states(&states));
        drop(reg);
        for ev in events {
            let _ = state.tx.send(ev);
        }
    }
}

/// 对任意 agent 喊话：hub 宿主写 stdin；外部 agent 走 tmux send-keys。
pub async fn say(hub: &Hub, id: &str, text: &str) -> anyhow::Result<()> {
    if hub.spawn.is_alive(id) {
        return hub.spawn.say(id, text);
    }
    let agent = hub
        .state
        .registry
        .read()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("agent 不存在: {id}"))?;
    let pid = match agent.source {
        pf_scene::Source::Discovered { pid } | pf_scene::Source::Spawned { pid } => pid,
        _ => anyhow::bail!("该来源不支持注入"),
    };
    let proc_root = PathBuf::from(std::env::var("PF_PROC_ROOT").unwrap_or_else(|_| "/proc".into()));
    let pane = tmux::pane_for_pid(&proc_root, pid).ok_or_else(|| {
        anyhow::anyhow!(
            "{id} 不在 tmux 里，无法注入（外部会话需运行在 tmux 中，或用 pf_send 进收件箱）"
        )
    })?;
    tmux::send_text(&pane, text)
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
    say(&hub, &id, &req.text)
        .await
        .map(|_| "ok")
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

#[derive(Debug, serde::Deserialize)]
struct ImportRequest {
    session_id: String,
    #[serde(default)]
    provider: Option<pf_scene::Provider>,
}

/// 手动导入：按 session id 把历史会话以 `claude --resume` 招进车间。
async fn import_agent(
    State(hub): State<Hub>,
    Json(req): Json<ImportRequest>,
) -> Result<Json<AgentInfo>, (StatusCode, String)> {
    let provider = req.provider.unwrap_or(pf_scene::Provider::ClaudeCode);
    hub.spawn
        .spawn(SpawnRequest {
            provider,
            cwd: None,
            argv: Some(vec!["claude".into(), "--resume".into(), req.session_id]),
        })
        .await
        .map(Json)
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

#[derive(Debug, serde::Deserialize)]
struct InboxQuery {
    pid: u32,
}

async fn send_msg(
    State(hub): State<Hub>,
    Json(req): Json<mail::SendRequest>,
) -> Result<Json<ChatMessage>, (StatusCode, String)> {
    hub.mail
        .send(req.from_pid, &req.to, &req.text)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

async fn inbox(State(hub): State<Hub>, Query(q): Query<InboxQuery>) -> Json<InboxResponse> {
    Json(hub.mail.inbox_for_pid(q.pid).await)
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
            ClientMessage::Say { id, text } => say(&hub, &id, &text).await,
            ClientMessage::Stop { id } => hub.spawn.stop(&id).await,
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "控制面调用失败");
        }
    }
    send_task.abort();
}
