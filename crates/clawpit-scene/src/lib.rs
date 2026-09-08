//! 场景模型与线协议。
//!
//! hub 是唯一生产者；TUI / Web 渲染端是消费者。
//! 协议演进规则：只增不改——新增字段必须带默认值，枚举新增成员视为兼容变更。

use serde::{Deserialize, Serialize};

/// agent 提供方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    ClaudeCode,
    Codex,
    Gemini,
    Aider,
    OpenCode,
    Generic,
}

impl Provider {
    /// 进程可执行名（argv[0] 的 basename）→ provider。
    /// 只匹配明确的 agent CLI 名；node/python 这类宿主一律不匹配，避免误报。
    pub fn detect(exe: &str) -> Option<Provider> {
        let exe = exe.rsplit('/').next().unwrap_or(exe);
        match exe {
            "claude" => Some(Provider::ClaudeCode),
            "codex" => Some(Provider::Codex),
            "gemini" => Some(Provider::Gemini),
            "aider" => Some(Provider::Aider),
            "opencode" => Some(Provider::OpenCode),
            _ => None,
        }
    }

    /// 短代号，用于 agent 显示名，如 cc-12345。
    pub fn short(self) -> &'static str {
        match self {
            Provider::ClaudeCode => "cc",
            Provider::Codex => "cx",
            Provider::Gemini => "gm",
            Provider::Aider => "ad",
            Provider::OpenCode => "oc",
            Provider::Generic => "ag",
        }
    }
}

/// agent 工作状态。M0 只有 Unknown；后续 driver 填充真实状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Unknown,
    Thinking,
    Working,
    WaitingInput,
    Done,
    Error,
}

/// 默认房间：所有 agent 的起点，常驻不可删。
pub const DEFAULT_ROOM: &str = "lobby";

/// 任务状态机：queued（排队等调度）→ assigned（已派给 agent）→ done / failed。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Assigned,
    Done,
    Failed,
}

/// 车间里的一张任务（统一调度的一等公民）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    /// 一句话标题（看板/名牌用）
    pub title: String,
    /// 完整任务描述（下达给 agent 的正文）
    pub brief: String,
    pub status: TaskStatus,
    /// 承接的 agent id（queued 时为 None）
    pub assignee: Option<String>,
    /// 创建者：agent id 或 "human"
    pub created_by: String,
    /// 归属房间（默认发起者所在房）
    pub room: String,
}

/// 车间里的一个房间（隔离/分组单位）。
/// 消息投递仍按 id 全局直达——room 是展示与管理层的隔离，不是通信边界。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomInfo {
    pub id: String,
    /// 显示名（可改，不唯一约束由 registry 保证）。
    pub name: String,
    /// 归档后不出现在切换栏，但历史与成员关系保留。
    pub archived: bool,
}

/// agent 是怎么进入车间的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// 扫描发现的存量进程（M0）。
    Discovered { pid: u32 },
    /// hub 自己 spawn 的（M1 宿舍），pid 供 MCP 身份匹配与消息路由。
    Spawned { pid: u32 },
    /// agent 通过 MCP 主动注册的（M2 邮局）。
    Registered,
    /// 手动导入 session id（M3 观察站）。
    Imported,
}

/// 车间里的一句话（人→agent / agent→agent / agent→人）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    /// 发送者 agent id；来自终端的人为 "human"
    pub from: String,
    /// 显示名（human / cc-1234）
    pub from_name: String,
    /// 接收者 agent id；发给人的消息为 "human"（只上墙不投递）
    pub to: String,
    pub text: String,
    /// 发送时刻（epoch 毫秒）。旧事件无此字段时为 0；持久化与排序用。
    #[serde(default)]
    pub ts: u64,
}

/// 车间里的一只 worker。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub provider: Provider,
    pub name: String,
    pub state: AgentState,
    pub source: Source,
    /// 人类可辨识的上下文标题（当前任务一句话）。观察站从 transcript 推断，
    /// 注入式喊话也会用消息正文刷新它；None = 显示回退到 name。
    #[serde(default)]
    pub title: Option<String>,
    /// 所在房间 id（默认大厅）。隔离只影响展示分组，不影响消息路由。
    #[serde(default = "default_room")]
    pub room: String,
}

fn default_room() -> String {
    DEFAULT_ROOM.into()
}

/// WS 场景事件（JSON，tag = type）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SceneEvent {
    /// 连接建立时的全量快照，之后只发增量。
    Snapshot {
        agents: Vec<AgentInfo>,
        /// 房间清单（含大厅）。旧客户端忽略多余字段即向后兼容。
        #[serde(default)]
        rooms: Vec<RoomInfo>,
        /// 任务清单。旧客户端忽略多余字段即向后兼容。
        #[serde(default)]
        tasks: Vec<TaskInfo>,
    },
    /// 新增或变更。
    AgentUpsert { agent: AgentInfo },
    /// 消失。
    AgentGone { id: String },
    /// 房间新增或变更（改名/归档）。
    RoomUpsert { room: RoomInfo },
    /// 房间删除（成员已被挪回大厅，随后的 AgentUpsert 会逐一告知）。
    RoomGone { id: String },
    /// 任务新增或变更（派发/完成/失败）。
    TaskUpsert { task: TaskInfo },
    /// 任务删除。
    TaskGone { id: String },
    /// 车间里的一句话（气泡上墙）。
    Chat { message: ChatMessage },
}

/// 客户端 → hub 的控制面消息（M1：spawn / say / stop）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// 招一只新 worker（argv 可覆盖默认命令，主要供测试）。
    Spawn {
        provider: Provider,
        cwd: Option<String>,
        argv: Option<Vec<String>>,
    },
    /// 对指定 worker 喊话（写进它的 stdin）。
    Say { id: String, text: String },
    /// 停掉并移除 worker。
    Stop { id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_detect_by_argv0() {
        assert_eq!(
            Provider::detect("/home/u/.nvm/versions/node/v20/bin/claude"),
            Some(Provider::ClaudeCode)
        );
        assert_eq!(Provider::detect("codex"), Some(Provider::Codex));
        assert_eq!(Provider::detect("node"), None);
        assert_eq!(Provider::detect("python3"), None);
    }

    #[test]
    fn scene_event_serde_roundtrip() {
        let ev = SceneEvent::AgentUpsert {
            agent: AgentInfo {
                id: "cc-42".into(),
                provider: Provider::ClaudeCode,
                name: "cc-42".into(),
                state: AgentState::Unknown,
                source: Source::Discovered { pid: 42 },
                title: None,
                room: DEFAULT_ROOM.into(),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("\"agent_upsert\""),
            "tag 应为 snake_case: {json}"
        );
        assert_eq!(serde_json::from_str::<SceneEvent>(&json).unwrap(), ev);
    }
}
