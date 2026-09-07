//! 持久化：追加式事件日志 + 启动重放。
//!
//! `<CLAWPIT_HOME>/events.jsonl` 一行一个 SceneEvent（RoomUpsert / RoomGone /
//! AgentUpsert / AgentGone / Chat；Snapshot 是派生快照不落盘）。
//! 启动重放重建：房间结构、Discovered 成员的房间归属、最近聊天（有界环）。
//! Spawned / Registered / Imported 不重放——它们的进程随上次 hub 一起死了，
//! Discovered 的死条目由首轮扫描差量自然清退。
//! 写失败只 warn 不阻断（本地工具优先可用）。

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use clawpit_scene::{ChatMessage, SceneEvent, Source};

use crate::registry::Registry;

/// 内存里保留的聊天条数上限（档案窗口，也是重放尾部截断长度）。
pub const CHAT_KEEP: usize = 200;

pub struct Store {
    file: Mutex<File>,
    /// 重放得到的 + 运行期追加的聊天（尾部有界）
    chat_log: Mutex<Vec<ChatMessage>>,
    /// 日志里见过的最大 msg 序号（重放续号，防重启后 id 撞车）
    pub next_msg: AtomicU64,
}

impl Store {
    /// 打开（或创建）日志，把已有事件重放进 registry。
    pub fn open(dir: &Path, registry: &mut Registry) -> anyhow::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("events.jsonl");
        let mut chat = Vec::new();
        let mut next_msg = 0u64;
        if let Ok(f) = File::open(&path) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let Ok(ev) = serde_json::from_str::<SceneEvent>(&line) else {
                    continue; // 损坏行（半写）跳过：追加日志天然容忍尾部残缺
                };
                match ev {
                    SceneEvent::RoomUpsert { room } => registry.upsert_room(room),
                    SceneEvent::RoomGone { id } => {
                        let _ = registry.delete_room(&id); // 成员回大厅，后续 upsert 会各自落位
                    }
                    SceneEvent::AgentUpsert { agent } => {
                        if matches!(agent.source, Source::Discovered { .. }) {
                            registry.upsert(agent);
                        }
                    }
                    SceneEvent::AgentGone { id } => {
                        registry.remove(&id);
                    }
                    SceneEvent::Chat { message } => {
                        if let Some(n) = message
                            .id
                            .strip_prefix("msg-")
                            .and_then(|s| s.parse::<u64>().ok())
                        {
                            next_msg = next_msg.max(n);
                        }
                        chat.push(message);
                    }
                    SceneEvent::Snapshot { .. } => {}
                }
            }
        }
        if chat.len() > CHAT_KEEP {
            chat.drain(..chat.len() - CHAT_KEEP);
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Store {
            file: Mutex::new(file),
            chat_log: Mutex::new(chat),
            next_msg: AtomicU64::new(next_msg),
        })
    }

    /// 广播前顺手落盘 + 维护聊天环。IO 失败静默（本地工具不因磁盘小事故停摆）。
    pub fn record(&self, ev: &SceneEvent) {
        match ev {
            SceneEvent::Snapshot { .. } => return,
            SceneEvent::Chat { message } => {
                if let Some(n) = message
                    .id
                    .strip_prefix("msg-")
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    self.next_msg.fetch_max(n, Ordering::SeqCst);
                }
                let mut log = self.chat_log.lock().unwrap();
                log.push(message.clone());
                let overflow = log.len().saturating_sub(CHAT_KEEP);
                if overflow > 0 {
                    log.drain(..overflow);
                }
            }
            _ => {}
        }
        let line = serde_json::to_string(ev).unwrap_or_default();
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }

    /// 最近聊天（尾部有界），档案视图的数据源。
    pub fn recent_chat(&self) -> Vec<ChatMessage> {
        self.chat_log.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawpit_scene::{AgentInfo, AgentState, Provider, RoomInfo};

    fn disc_agent(id: &str, room: &str) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            provider: Provider::ClaudeCode,
            name: id.into(),
            state: AgentState::WaitingInput,
            source: Source::Discovered { pid: 42 },
            title: Some("干活".into()),
            room: room.into(),
        }
    }

    fn chat(n: u64, text: &str) -> SceneEvent {
        SceneEvent::Chat {
            message: ChatMessage {
                id: format!("msg-{n}"),
                from: "human".into(),
                from_name: "human".into(),
                to: "cc-42".into(),
                text: text.into(),
                ts: n,
            },
        }
    }

    #[test]
    fn record_then_replay_rebuilds_state() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let room = RoomInfo {
            id: "rm-1".into(),
            name: "重构间".into(),
            archived: false,
        };
        {
            let mut reg = Registry::new();
            let store = Store::open(dir.path(), &mut reg)?;
            store.record(&SceneEvent::RoomUpsert { room: room.clone() });
            store.record(&SceneEvent::AgentUpsert {
                agent: disc_agent("cc-42", "rm-1"),
            });
            store.record(&chat(1, "在吗"));
            store.record(&chat(2, "干活"));
        }
        // 重启：新 registry + 重放
        let mut reg2 = Registry::new();
        let store2 = Store::open(dir.path(), &mut reg2)?;
        assert!(reg2
            .rooms()
            .iter()
            .any(|r| r.id == "rm-1" && r.name == "重构间"));
        assert_eq!(reg2.get("cc-42").map(|a| a.room.as_str()), Some("rm-1"));
        assert_eq!(
            reg2.get("cc-42").and_then(|a| a.title.clone()),
            Some("干活".into())
        );
        assert_eq!(store2.recent_chat().len(), 2);
        assert_eq!(
            store2.next_msg.load(Ordering::SeqCst),
            2,
            "msg 序号要续上，重启后不得撞车"
        );
        Ok(())
    }

    #[test]
    fn replay_drops_spawned_and_keeps_discovered() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        {
            let mut reg = Registry::new();
            let store = Store::open(dir.path(), &mut reg)?;
            let spawned = AgentInfo {
                source: Source::Spawned { pid: 7 },
                ..disc_agent("sp-1", "lobby")
            };
            store.record(&SceneEvent::AgentUpsert { agent: spawned });
            store.record(&SceneEvent::AgentUpsert {
                agent: disc_agent("cc-42", "lobby"),
            });
        }
        let mut reg2 = Registry::new();
        Store::open(dir.path(), &mut reg2)?;
        assert!(
            reg2.get("sp-1").is_none(),
            "Spawned 的 pty 随上次 hub 死了，不得复活"
        );
        assert!(reg2.get("cc-42").is_some());
        Ok(())
    }

    #[test]
    fn chat_ring_bounded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut reg = Registry::new();
        let store = Store::open(dir.path(), &mut reg)?;
        for i in 0..(CHAT_KEEP + 30) as u64 {
            store.record(&chat(i, "水"));
        }
        assert_eq!(store.recent_chat().len(), CHAT_KEEP);
        assert_eq!(
            store.recent_chat().last().unwrap().id,
            format!("msg-{}", CHAT_KEEP + 29)
        );
        assert_eq!(
            store.next_msg.load(Ordering::SeqCst),
            (CHAT_KEEP + 29) as u64
        );
        Ok(())
    }

    #[test]
    fn truncated_tail_line_is_tolerated() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        {
            let mut reg = Registry::new();
            let store = Store::open(dir.path(), &mut reg)?;
            store.record(&chat(1, "好"));
        }
        // 模拟崩溃时的半行
        let path = dir.path().join("events.jsonl");
        let mut content = std::fs::read_to_string(&path)?;
        content.push_str("{\"type\":\"chat\",\"message\":{\"id\":\"msg-2\",\"from\":"); // 无结尾换行的残行
        std::fs::write(&path, content)?;
        let mut reg2 = Registry::new();
        let store2 = Store::open(dir.path(), &mut reg2)?;
        assert_eq!(store2.recent_chat().len(), 1, "残行跳过，好的那条保留");
        Ok(())
    }
}
