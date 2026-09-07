//! clawpit 库：注册表 + 扫描器 + 宿舍（spawn）+ WS 路由组装。
//! bin 只负责环境变量解析与启动；库部分供集成测试复用。

pub mod mail;
pub mod observe;
pub mod registry;
pub mod scanner;
pub mod spawn;
pub mod store;
pub mod tmux;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::Response,
    routing::{delete, get, patch, post},
    Json, Router,
};
use clawpit_scene::{AgentInfo, ChatMessage, RoomInfo, SceneEvent};
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
    /// 事件日志（开了就随 emit 落盘 + 重放续号）；测试默认 None 保持无 IO。
    pub store: Option<Arc<store::Store>>,
}

impl AppState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            tx,
            registry: Arc::new(RwLock::new(Registry::new())),
            store: None,
        }
    }

    /// 广播一条场景事件；开了持久化就顺手落盘。
    pub fn emit(&self, ev: SceneEvent) {
        if let Some(s) = &self.store {
            s.record(&ev);
        }
        let _ = self.tx.send(ev);
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

    /// 带持久化的 hub：重放 `<dir>/events.jsonl` 重建房间结构与最近聊天，
    /// 之后所有场景事件随广播落盘。bin 用；测试默认走 `new()`（无 IO）。
    pub fn with_persistence(dir: &std::path::Path) -> anyhow::Result<Self> {
        let (tx, _) = broadcast::channel(64);
        let mut registry = Registry::new();
        let store = Arc::new(store::Store::open(dir, &mut registry)?);
        let state = AppState {
            tx,
            registry: Arc::new(RwLock::new(registry)),
            store: Some(store.clone()),
        };
        let spawn = SpawnManager::new(state.clone());
        let mail = MailManager::new(state.clone(), spawn.clone());
        mail.init_msg_counter(store.next_msg.load(std::sync::atomic::Ordering::SeqCst));
        Ok(Self { mail, spawn, state })
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

/// Web 车间（单文件像素客户端，编译期内嵌，零构建）。
const INDEX_HTML: &str = include_str!("../../../clawpit-web/index.html");

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
        .route("/agents/:id/move", post(move_agent))
        .route("/agents/:id/history", get(agent_history))
        .route("/rooms", get(list_rooms).post(create_room))
        .route("/rooms/:id", patch(update_room).delete(delete_room))
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
    extra_agents: Vec<String>,
) {
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        let found = scanner::scan(&proc_root, &extra_agents);
        // 锁内只做登记和取清单；阻塞文件 I/O（observe）放锁外，否则整个 API 被扫描串住
        let (mut events, discovered) = {
            let mut reg = state.registry.write().await;
            let events = reg.apply_discovered(found);
            let discovered: Vec<(String, u32, clawpit_scene::Provider)> = reg
                .snapshot()
                .iter()
                .filter_map(|a| match (a.provider, &a.source) {
                    (
                        p @ (clawpit_scene::Provider::ClaudeCode | clawpit_scene::Provider::Codex),
                        clawpit_scene::Source::Discovered { pid },
                    ) => Some((a.id.clone(), *pid, p)),
                    _ => None,
                })
                .collect();
            (events, discovered)
        };
        // 观察站：给 discovered 的 claude/codex 会话读真实状态与标题（一次 tail 两得）
        let observations: Vec<(String, observe::Observation)> = discovered
            .into_iter()
            .map(|(id, pid, provider)| {
                let o = match provider {
                    clawpit_scene::Provider::Codex => {
                        observe::read_codex_observation(&proc_root, pid)
                    }
                    _ => observe::read_observation(&proc_root, pid, &claude_home),
                };
                (id, o)
            })
            .collect();
        let states: Vec<(String, clawpit_scene::AgentState)> = observations
            .iter()
            .map(|(id, o)| (id.clone(), o.state))
            .collect();
        let titles: Vec<(String, Option<String>)> = observations
            .iter()
            .map(|(id, o)| (id.clone(), o.title.clone()))
            .collect();
        {
            let mut reg = state.registry.write().await;
            events.extend(reg.apply_states(&states));
            events.extend(reg.apply_titles(&titles));
        }
        for ev in events {
            state.emit(ev);
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
        clawpit_scene::Source::Discovered { pid } | clawpit_scene::Source::Spawned { pid } => pid,
        _ => anyhow::bail!("该来源不支持注入"),
    };
    let proc_root =
        PathBuf::from(std::env::var("CLAWPIT_PROC_ROOT").unwrap_or_else(|_| "/proc".into()));
    // pid 复用防线：注册表里的 pid 可能已被系统分给无关进程，注入前必须复核
    anyhow::ensure!(
        tmux::pid_still_agent(&proc_root, pid, agent.provider),
        "{id} 的进程 {pid} 已不存在（pid 可能被复用），拒绝注入"
    );
    let pane = tmux::pane_for_pid(&proc_root, pid).ok_or_else(|| {
        anyhow::anyhow!(
            "{id} 不在 tmux 里，无法注入（外部会话需运行在 tmux 中，或用 clawpit_send 进收件箱）"
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
    provider: Option<clawpit_scene::Provider>,
}

/// 手动导入：按 session id 把历史会话以 `claude --resume` 招进车间。
async fn import_agent(
    State(hub): State<Hub>,
    Json(req): Json<ImportRequest>,
) -> Result<Json<AgentInfo>, (StatusCode, String)> {
    let provider = req.provider.unwrap_or(clawpit_scene::Provider::ClaudeCode);
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

// ── 房间管理：隔离是展示/分组层，消息路由仍按 id 全局直达 ──────────────

async fn list_rooms(State(hub): State<Hub>) -> Json<Vec<RoomInfo>> {
    Json(hub.state.registry.read().await.rooms())
}

#[derive(Debug, serde::Deserialize)]
struct CreateRoomRequest {
    name: String,
}

async fn create_room(
    State(hub): State<Hub>,
    Json(req): Json<CreateRoomRequest>,
) -> Result<Json<RoomInfo>, (StatusCode, String)> {
    let room = hub
        .state
        .registry
        .write()
        .await
        .create_room(&req.name)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    hub.state
        .emit(SceneEvent::RoomUpsert { room: room.clone() });
    Ok(Json(room))
}

#[derive(Debug, serde::Deserialize)]
struct UpdateRoomRequest {
    name: Option<String>,
    archived: Option<bool>,
}

async fn update_room(
    State(hub): State<Hub>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRoomRequest>,
) -> Result<Json<RoomInfo>, (StatusCode, String)> {
    let room = hub
        .state
        .registry
        .write()
        .await
        .update_room(&id, req.name.as_deref(), req.archived)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    hub.state
        .emit(SceneEvent::RoomUpsert { room: room.clone() });
    Ok(Json(room))
}

async fn delete_room(
    State(hub): State<Hub>,
    Path(id): Path<String>,
) -> Result<&'static str, (StatusCode, String)> {
    let events = hub
        .state
        .registry
        .write()
        .await
        .delete_room(&id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    for ev in events {
        hub.state.emit(ev);
    }
    Ok("ok")
}

#[derive(Debug, serde::Deserialize)]
struct MoveRequest {
    to: String,
}

/// 档案窗口：从事件日志里取该 agent 的收发历史（纯函数，好测）。
fn history_for(log: Vec<ChatMessage>, id: &str, limit: usize) -> Vec<ChatMessage> {
    let mut hits: Vec<ChatMessage> = log
        .into_iter()
        .filter(|m| m.from == id || m.to == id)
        .collect();
    let overflow = hits.len().saturating_sub(limit);
    if overflow > 0 {
        hits.drain(..overflow); // 保留最近的 limit 条，时间正序给渲染端
    }
    hits
}

#[derive(Debug, serde::Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

async fn agent_history(
    State(hub): State<Hub>,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Json<Vec<ChatMessage>> {
    let limit = q.limit.unwrap_or(50).min(store::CHAT_KEEP);
    let log = hub
        .state
        .store
        .as_ref()
        .map(|s| s.recent_chat())
        .unwrap_or_default();
    Json(history_for(log, &id, limit))
}

async fn move_agent(
    State(hub): State<Hub>,
    Path(id): Path<String>,
    Json(req): Json<MoveRequest>,
) -> Result<Json<AgentInfo>, (StatusCode, String)> {
    let agent = hub
        .state
        .registry
        .write()
        .await
        .move_agent(&id, &req.to)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    hub.state.emit(SceneEvent::AgentUpsert {
        agent: agent.clone(),
    });
    Ok(Json(agent))
}

#[derive(Debug, serde::Deserialize)]
struct InboxQuery {
    pid: u32,
}

/// /msg 响应：消息本体 + 实际投递去向（收件箱=死信候选，必须可见）。
#[derive(Debug, serde::Serialize)]
struct SendResponse {
    message: ChatMessage,
    delivered: mail::Delivery,
}

async fn send_msg(
    State(hub): State<Hub>,
    Json(req): Json<mail::SendRequest>,
) -> Result<Json<SendResponse>, (StatusCode, String)> {
    hub.mail
        .send(req.from_pid, &req.to, &req.text)
        .await
        .map(|(message, delivered)| Json(SendResponse { message, delivered }))
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

async fn inbox(State(hub): State<Hub>, Query(q): Query<InboxQuery>) -> Json<InboxResponse> {
    Json(hub.mail.inbox_for_pid(q.pid).await)
}

async fn scene_ws(State(hub): State<Hub>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        let snapshot = {
            let reg = hub.state.registry.read().await;
            SceneEvent::Snapshot {
                agents: reg.snapshot(),
                rooms: reg.rooms(),
            }
        };
        handle_socket(socket, hub, snapshot).await;
    })
}

/// WS 会话：下行=场景事件广播；上行=控制面（spawn/say/stop）。
async fn handle_socket(socket: axum::extract::ws::WebSocket, hub: Hub, snapshot: SceneEvent) {
    use axum::extract::ws::Message;
    use clawpit_scene::ClientMessage;
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::broadcast::error::RecvError;

    let (mut sender, mut receiver) = socket.split();
    let mut rx = hub.state.tx.subscribe();

    // 下行任务：先快照，再转发广播
    let send_state = hub.state.clone();
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
                    // 慢客户端丢帧：补一份全量快照自愈，避免幽灵 agent 永久残留
                    tracing::warn!(missed = n, "scene subscriber lagged, resync snapshot");
                    let snap = {
                        let reg = send_state.registry.read().await;
                        SceneEvent::Snapshot {
                            agents: reg.snapshot(),
                            rooms: reg.rooms(),
                        }
                    };
                    if let Ok(json) = serde_json::to_string(&snap) {
                        if sender.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clawpit_scene::ChatMessage;

    fn msg(n: u64, from: &str, to: &str) -> ChatMessage {
        ChatMessage {
            id: format!("msg-{n}"),
            from: from.into(),
            from_name: from.into(),
            to: to.into(),
            text: format!("t{n}"),
            ts: n,
        }
    }

    /// 档案窗口：只含该 agent 的收发、有界、保持时间正序
    #[test]
    fn history_filters_and_bounds() {
        let log = vec![
            msg(1, "human", "cc-1"),
            msg(2, "human", "cc-2"),
            msg(3, "cc-1", "human"),
            msg(4, "cc-2", "cc-1"),
            msg(5, "human", "cc-1"),
        ];
        let h = history_for(log.clone(), "cc-1", 50);
        assert_eq!(
            h.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["msg-1", "msg-3", "msg-4", "msg-5"]
        );
        let tail = history_for(log, "cc-1", 2);
        assert_eq!(
            tail.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["msg-4", "msg-5"],
            "只留最近 N 条且保持正序"
        );
        assert!(history_for(vec![], "cc-404", 10).is_empty());
    }
}
