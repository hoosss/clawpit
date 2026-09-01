//! Agent 注册表：车间的花名册。
//!
//! 差量规则：`apply_discovered` 只增删改 `Source::Discovered` 来源的条目，
//! Spawned/Registered/Imported 的条目归各自 driver 管理，扫描轮不得触碰。

use std::collections::{HashMap, HashSet};

use clawpit_scene::{AgentInfo, AgentState, SceneEvent, Source};

use crate::scanner::ProcHit;

#[derive(Default)]
pub struct Registry {
    agents: HashMap<String, AgentInfo>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    /// 全量快照（按 id 排序，保证渲染端稳定）。
    pub fn snapshot(&self) -> Vec<AgentInfo> {
        let mut v: Vec<AgentInfo> = self.agents.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// 供 Spawn/Post/Mailbox driver 直接增改自己来源的条目。
    pub fn upsert(&mut self, agent: AgentInfo) {
        self.agents.insert(agent.id.clone(), agent);
    }

    pub fn remove(&mut self, id: &str) {
        self.agents.remove(id);
    }

    /// 更新状态，返回条目是否存在。
    pub fn set_state(&mut self, id: &str, state: AgentState) -> bool {
        if let Some(a) = self.agents.get_mut(id) {
            a.state = state;
            true
        } else {
            false
        }
    }

    pub fn get(&self, id: &str) -> Option<&AgentInfo> {
        self.agents.get(id)
    }

    /// 按进程 pid 找 agent（discovered/spawned 都带 pid），供 MCP 身份匹配。
    pub fn find_by_pid(&self, pid: u32) -> Option<AgentInfo> {
        self.agents
            .values()
            .find(|a| match a.source {
                Source::Discovered { pid: p } | Source::Spawned { pid: p } => p == pid,
                _ => false,
            })
            .cloned()
    }

    /// 批量更新 Discovered 条目的状态（观察站产出），返回变更事件。
    pub fn apply_states(&mut self, states: &[(String, AgentState)]) -> Vec<SceneEvent> {
        let mut events = Vec::new();
        for (id, st) in states {
            if let Some(a) = self.agents.get_mut(id) {
                if matches!(a.source, Source::Discovered { .. }) && a.state != *st {
                    a.state = *st;
                    events.push(SceneEvent::AgentUpsert { agent: a.clone() });
                }
            }
        }
        events
    }

    /// 应用一轮扫描结果，返回需要广播的差量事件。
    pub fn apply_discovered(&mut self, found: Vec<ProcHit>) -> Vec<SceneEvent> {
        let mut events = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        // hub 宿主（Spawned）的进程也在 /proc 里，扫描不得把它们重复登记成 cc-<pid> 双胞胎
        let spawned_pids: HashSet<u32> = self
            .agents
            .values()
            .filter_map(|a| match a.source {
                Source::Spawned { pid } => Some(pid),
                _ => None,
            })
            .collect();

        for hit in &found {
            if spawned_pids.contains(&hit.pid) {
                continue;
            }
            let id = format!("{}-{}", hit.provider.short(), hit.pid);
            seen.insert(id.clone());
            let agent = AgentInfo {
                id: id.clone(),
                provider: hit.provider,
                name: id.clone(),
                state: AgentState::Unknown,
                source: Source::Discovered { pid: hit.pid },
            };
            let unchanged = self.agents.get(&id).is_some_and(|old| *old == agent);
            if !unchanged {
                self.agents.insert(id.clone(), agent.clone());
                events.push(SceneEvent::AgentUpsert { agent });
            }
        }

        // 消失的：只清理 Discovered 来源
        let gone: Vec<String> = self
            .agents
            .iter()
            .filter(|(id, a)| matches!(a.source, Source::Discovered { .. }) && !seen.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            self.agents.remove(&id);
            events.push(SceneEvent::AgentGone { id });
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawpit_scene::Provider;

    fn hit(pid: u32, provider: Provider) -> ProcHit {
        ProcHit { pid, provider }
    }

    #[test]
    fn new_agent_emits_upsert_then_quiet() {
        let mut reg = Registry::new();
        let first = reg.apply_discovered(vec![hit(42, Provider::ClaudeCode)]);
        assert_eq!(first.len(), 1);
        assert!(matches!(first[0], SceneEvent::AgentUpsert { .. }));

        // 内容没变就不再广播
        let second = reg.apply_discovered(vec![hit(42, Provider::ClaudeCode)]);
        assert!(second.is_empty(), "无变化不应产生事件");
    }

    #[test]
    fn vanished_process_emits_gone() {
        let mut reg = Registry::new();
        reg.apply_discovered(vec![hit(42, Provider::ClaudeCode)]);
        let events = reg.apply_discovered(vec![]);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SceneEvent::AgentGone { id } if id == "cc-42"));
        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn discovery_never_touches_spawned() {
        let mut reg = Registry::new();
        reg.agents.insert(
            "cc-999".into(),
            AgentInfo {
                id: "cc-999".into(),
                provider: Provider::ClaudeCode,
                name: "cc-999".into(),
                state: AgentState::Unknown,
                source: Source::Spawned { pid: 999 },
            },
        );
        let events = reg.apply_discovered(vec![]);
        assert!(events.is_empty(), "Spawned 条目不归扫描管");
        assert_eq!(reg.snapshot().len(), 1);
    }

    #[test]
    fn spawned_pid_not_double_registered_by_scan() {
        let mut reg = Registry::new();
        reg.agents.insert(
            "sp-1".into(),
            AgentInfo {
                id: "sp-1".into(),
                provider: Provider::ClaudeCode,
                name: "sp-1".into(),
                state: AgentState::Working,
                source: Source::Spawned { pid: 4242 },
            },
        );
        // 扫描器在同一 pid 上命中 claude → 不得生成 cc-4242 双胞胎
        let events = reg.apply_discovered(vec![crate::scanner::ProcHit {
            pid: 4242,
            provider: Provider::ClaudeCode,
        }]);
        assert!(events.is_empty(), "宿主进程不重复登记");
        assert!(reg.get("cc-4242").is_none());
        assert!(reg.get("sp-1").is_some());
    }
}
