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

/// agent 是怎么进入车间的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// 扫描发现的存量进程（M0）。
    Discovered { pid: u32 },
    /// hub 自己 spawn 的（M1 宿舍）。
    Spawned,
    /// agent 通过 MCP 主动注册的（M2 邮局）。
    Registered,
    /// 手动导入 session id（M3 观察站）。
    Imported,
}

/// 车间里的一只 worker。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub provider: Provider,
    pub name: String,
    pub state: AgentState,
    pub source: Source,
}

/// WS 场景事件（JSON，tag = type）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SceneEvent {
    /// 连接建立时的全量快照，之后只发增量。
    Snapshot { agents: Vec<AgentInfo> },
    /// 新增或变更。
    AgentUpsert { agent: AgentInfo },
    /// 消失。
    AgentGone { id: String },
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
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"agent_upsert\""), "tag 应为 snake_case: {json}");
        assert_eq!(serde_json::from_str::<SceneEvent>(&json).unwrap(), ev);
    }
}
