//! 邮局（M2）：车间消息总线。
//!
//! 路由规则：
//! - 发给 hub 宿主且活着的 worker → 直接注入它的 stdin（带 `[from X]` 前缀）
//! - 发给外部/已退出的 agent → 进收件箱，等它的 MCP 工具来取（取走即清）
//! - 发给 "human" → 只上墙（TUI 气泡可见），不投递
//!
//! 身份：MCP 工具进程用父 pid 自报 → 注册表按 pid 匹配（discovered/spawned 通吃）。

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use pf_scene::{AgentInfo, ChatMessage, SceneEvent};

use crate::{spawn::SpawnManager, AppState};

pub const HUMAN: &str = "human";

#[derive(Debug, serde::Deserialize)]
pub struct SendRequest {
    /// 发送者进程 pid（MCP 工具传父 pid）；None 或未匹配 = 人（human）
    pub from_pid: Option<u32>,
    pub to: String,
    pub text: String,
}

#[derive(Debug, serde::Serialize)]
pub struct InboxResponse {
    /// pid 匹配到的 agent（None=匿名，消息照发但身份是 human）
    pub agent: Option<AgentInfo>,
    /// 取走即清空的待收消息
    pub messages: Vec<ChatMessage>,
}

pub struct MailManager {
    state: AppState,
    spawn: Arc<SpawnManager>,
    inboxes: Mutex<HashMap<String, VecDeque<ChatMessage>>>,
    next: AtomicU64,
}

impl MailManager {
    pub fn new(state: AppState, spawn: Arc<SpawnManager>) -> Arc<Self> {
        Arc::new(Self {
            state,
            spawn,
            inboxes: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    /// 发一句话：解析身份 → 路由投递 → 上墙广播。
    pub async fn send(
        &self,
        from_pid: Option<u32>,
        to: &str,
        text: &str,
    ) -> anyhow::Result<ChatMessage> {
        if to.is_empty() || text.trim().is_empty() {
            anyhow::bail!("to 和 text 不能为空");
        }
        let from_agent = match from_pid {
            Some(pid) => self.state.registry.read().await.find_by_pid(pid),
            None => None,
        };
        let (from, from_name) = match &from_agent {
            Some(a) => (a.id.clone(), a.name.clone()),
            None => (HUMAN.to_string(), HUMAN.to_string()),
        };

        let msg = ChatMessage {
            id: format!("msg-{}", self.next.fetch_add(1, Ordering::SeqCst)),
            from,
            from_name,
            to: to.to_string(),
            text: text.trim().to_string(),
        };

        if to != HUMAN {
            // 收件人必须存在（拿不到锁/查无此人都算失败）
            if self.state.registry.read().await.get(to).is_none() {
                anyhow::bail!("收件人不存在: {to}（用 pf_list 查车间成员）");
            }
            // hub 宿主且活着 → 直接注入；否则入收件箱
            let injected = if self.spawn.is_alive(to) {
                self.spawn
                    .say(to, &format!("[from {}] {}", msg.from_name, msg.text))
                    .is_ok()
            } else {
                false
            };
            if !injected {
                self.inboxes
                    .lock()
                    .unwrap()
                    .entry(to.to_string())
                    .or_default()
                    .push_back(msg.clone());
            }
        }

        let _ = self.state.tx.send(SceneEvent::Chat {
            message: msg.clone(),
        });
        Ok(msg)
    }

    /// 按 pid 取收件箱（取走即清）。返回 (匹配到的 agent, 消息)。
    pub async fn inbox_for_pid(&self, pid: u32) -> InboxResponse {
        let agent = self.state.registry.read().await.find_by_pid(pid);
        let messages = match &agent {
            Some(a) => self.drain(&a.id),
            None => Vec::new(),
        };
        InboxResponse { agent, messages }
    }

    fn drain(&self, id: &str) -> Vec<ChatMessage> {
        self.inboxes
            .lock()
            .unwrap()
            .get_mut(id)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::SpawnRequest;
    use pf_scene::Provider;

    fn hub() -> Arc<MailManager> {
        let state = AppState::new();
        let spawn = SpawnManager::new(state.clone());
        MailManager::new(state, spawn)
    }

    #[tokio::test]
    async fn human_to_human_only_walls() -> anyhow::Result<()> {
        let mail = hub();
        let msg = mail.send(None, HUMAN, "测试一句话").await?;
        assert_eq!(msg.from, HUMAN);
        assert!(
            mail.inboxes.lock().unwrap().is_empty(),
            "发给 human 不入收件箱"
        );
        Ok(())
    }

    #[tokio::test]
    async fn to_unknown_agent_rejected() {
        let mail = hub();
        assert!(mail.send(None, "cc-404", "hi").await.is_err());
    }

    #[tokio::test]
    async fn to_spawned_alive_injects_not_queues() -> anyhow::Result<()> {
        let state = AppState::new();
        let spawn = SpawnManager::new(state.clone());
        let mail = MailManager::new(state.clone(), spawn.clone());
        let agent = spawn
            .spawn(SpawnRequest {
                provider: Provider::Generic,
                cwd: None,
                argv: Some(vec!["cat".into()]),
            })
            .await?;
        // cat 活着 → 注入成功，不入收件箱
        let msg = mail.send(None, &agent.id, "干活！").await?;
        assert_eq!(msg.to, agent.id);
        assert!(
            !mail.inboxes.lock().unwrap().contains_key(&agent.id),
            "活 worker 应走注入而非队列"
        );
        spawn.stop(&agent.id).await?;
        Ok(())
    }

    #[tokio::test]
    async fn to_discovered_agent_queues_then_drains() -> anyhow::Result<()> {
        let state = AppState::new();
        let spawn = SpawnManager::new(state.clone());
        let mail = MailManager::new(state.clone(), spawn);
        // 伪造一个 discovered agent（pid 4242）
        let hit = crate::scanner::ProcHit {
            pid: 4242,
            provider: Provider::ClaudeCode,
        };
        state.registry.write().await.apply_discovered(vec![hit]);

        let _ = mail.send(None, "cc-4242", "你是谁？").await?;
        let _ = mail.send(None, "cc-4242", "在吗").await?;
        // 未匹配 pid → 空收件箱
        assert!(mail.inbox_for_pid(1).await.messages.is_empty());
        // 正主来取 → 两条全到、取走即清
        let got = mail.inbox_for_pid(4242).await;
        assert_eq!(got.agent.as_ref().unwrap().id, "cc-4242");
        assert_eq!(got.messages.len(), 2);
        assert!(mail.inbox_for_pid(4242).await.messages.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn from_pid_resolves_identity() -> anyhow::Result<()> {
        let state = AppState::new();
        let spawn = SpawnManager::new(state.clone());
        let mail = MailManager::new(state.clone(), spawn);
        let hit = crate::scanner::ProcHit {
            pid: 777,
            provider: Provider::Codex,
        };
        state.registry.write().await.apply_discovered(vec![hit]);
        let msg = mail.send(Some(777), HUMAN, "我是 codex").await?;
        assert_eq!(msg.from, "cx-777");
        assert_eq!(msg.from_name, "cx-777");
        Ok(())
    }
}
