//! Agent 注册表：车间的花名册。
//!
//! 差量规则：`apply_discovered` 只增删改 `Source::Discovered` 来源的条目，
//! Spawned/Registered/Imported 的条目归各自 driver 管理，扫描轮不得触碰。

use std::collections::{HashMap, HashSet};

use clawpit_scene::{AgentInfo, AgentState, RoomInfo, SceneEvent, Source, DEFAULT_ROOM};

use crate::scanner::ProcHit;

#[derive(Default)]
pub struct Registry {
    agents: HashMap<String, AgentInfo>,
    rooms: HashMap<String, RoomInfo>,
    next_room: u64,
}

impl Registry {
    pub fn new() -> Self {
        let mut r = Self {
            agents: HashMap::new(),
            rooms: HashMap::new(),
            next_room: 0,
        };
        // 大厅常驻：所有 agent 的默认落点，也是删房时成员的退路
        r.rooms.insert(
            DEFAULT_ROOM.into(),
            RoomInfo {
                id: DEFAULT_ROOM.into(),
                name: "大厅".into(),
                archived: false,
            },
        );
        r
    }

    /// 全量快照（按 id 排序，保证渲染端稳定）。
    pub fn snapshot(&self) -> Vec<AgentInfo> {
        let mut v: Vec<AgentInfo> = self.agents.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// 房间快照（按 id 排序）。
    pub fn rooms(&self) -> Vec<RoomInfo> {
        let mut v: Vec<RoomInfo> = self.rooms.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// 建房：名字不可与现存（含归档）重复，id 顺序分配 rm-N。
    pub fn create_room(&mut self, name: &str) -> anyhow::Result<RoomInfo> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "房间名不能为空");
        anyhow::ensure!(
            !self.rooms.values().any(|r| r.name == name),
            "房间名已存在: {name}"
        );
        self.next_room += 1;
        let room = RoomInfo {
            id: format!("rm-{}", self.next_room),
            name: name.to_string(),
            archived: false,
        };
        self.rooms.insert(room.id.clone(), room.clone());
        Ok(room)
    }

    /// 改名 / 归档切换（大厅可改名不可归档）。返回最新房间信息。
    pub fn update_room(
        &mut self,
        id: &str,
        name: Option<&str>,
        archived: Option<bool>,
    ) -> anyhow::Result<RoomInfo> {
        anyhow::ensure!(self.rooms.contains_key(id), "房间不存在: {id}");
        let new_name = name.map(str::trim).filter(|n| !n.is_empty());
        if let Some(n) = &new_name {
            anyhow::ensure!(
                !self.rooms.values().any(|r| r.id != id && r.name == *n),
                "房间名已存在: {n}"
            );
        }
        if archived == Some(true) {
            anyhow::ensure!(id != DEFAULT_ROOM, "大厅不可归档");
        }
        let room = self.rooms.get_mut(id).expect("上面 ensure 过存在");
        if let Some(n) = new_name {
            room.name = n.to_string();
        }
        if let Some(a) = archived {
            room.archived = a;
        }
        Ok(room.clone())
    }

    /// 删房：成员挪回大厅（返回对应 upsert 事件），大厅本身不可删。
    pub fn delete_room(&mut self, id: &str) -> anyhow::Result<Vec<SceneEvent>> {
        anyhow::ensure!(id != DEFAULT_ROOM, "大厅不可删除");
        anyhow::ensure!(self.rooms.contains_key(id), "房间不存在: {id}");
        self.rooms.remove(id);
        let mut events = vec![SceneEvent::RoomGone { id: id.to_string() }];
        for a in self.agents.values_mut() {
            if a.room == id {
                a.room = DEFAULT_ROOM.into();
                events.push(SceneEvent::AgentUpsert { agent: a.clone() });
            }
        }
        Ok(events)
    }

    /// 把 agent 挪进房间（房间必须存在）。返回挪完的条目。
    pub fn move_agent(&mut self, id: &str, to: &str) -> anyhow::Result<AgentInfo> {
        anyhow::ensure!(self.rooms.contains_key(to), "目标房间不存在: {to}");
        let a = self
            .agents
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("agent 不存在: {id}"))?;
        a.room = to.to_string();
        Ok(a.clone())
    }

    /// 直接落一个房间（重放/持久化用，不走差量校验）。同步推进 id 计数器。
    pub fn upsert_room(&mut self, room: RoomInfo) {
        if let Some(n) = room
            .id
            .strip_prefix("rm-")
            .and_then(|s| s.parse::<u64>().ok())
        {
            self.next_room = self.next_room.max(n);
        }
        self.rooms.insert(room.id.clone(), room);
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

    /// 批量更新标题（观察站产出），返回变更事件。
    /// None（观察不到）不清空已有标题——避免 transcript 暂时读不到时名牌闪烁。
    pub fn apply_titles(&mut self, titles: &[(String, Option<String>)]) -> Vec<SceneEvent> {
        let mut events = Vec::new();
        for (id, title) in titles {
            let Some(t) = title else { continue };
            if let Some(a) = self.agents.get_mut(id) {
                if matches!(a.source, Source::Discovered { .. })
                    && a.title.as_deref() != Some(t.as_str())
                {
                    a.title = Some(t.clone());
                    events.push(SceneEvent::AgentUpsert { agent: a.clone() });
                }
            }
        }
        events
    }

    /// 设置任意来源 agent 的标题（注入式喊话刷新任务名牌用），变更才返回事件。
    pub fn set_title(&mut self, id: &str, title: Option<String>) -> Option<SceneEvent> {
        let a = self.agents.get_mut(id)?;
        if a.title == title {
            return None;
        }
        a.title = title;
        Some(SceneEvent::AgentUpsert { agent: a.clone() })
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
            let mut agent = AgentInfo {
                id: id.clone(),
                provider: hit.provider,
                name: id.clone(),
                state: AgentState::Unknown,
                source: Source::Discovered { pid: hit.pid },
                title: None,
                room: DEFAULT_ROOM.into(),
            };
            // 扫描重建时继承旧标题/房间：否则每轮差量比较都会抹掉它们并狂发 upsert
            if let Some(old) = self.agents.get(&id) {
                agent.title = old.title.clone();
                agent.room = old.room.clone();
            }
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
                title: None,
                room: DEFAULT_ROOM.into(),
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
                title: None,
                room: DEFAULT_ROOM.into(),
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

    #[test]
    fn room_lifecycle_and_membership() {
        let mut reg = Registry::new();
        // 大厅常驻
        assert!(reg.rooms().iter().any(|r| r.id == DEFAULT_ROOM));
        assert!(reg.delete_room(DEFAULT_ROOM).is_err(), "大厅不可删");
        assert!(
            reg.update_room(DEFAULT_ROOM, None, Some(true)).is_err(),
            "大厅不可归档"
        );

        // 建房 + 重名拒绝
        let room = reg.create_room("重构 room").unwrap();
        assert_eq!(room.id, "rm-1");
        assert!(reg.create_room("重构 room").is_err());
        assert!(reg.create_room("  ").is_err());

        // 入住 + 扫描继承：成员被扫描重建后房间不丢
        reg.apply_discovered(vec![crate::scanner::ProcHit {
            pid: 77,
            provider: Provider::ClaudeCode,
        }]);
        reg.move_agent("cc-77", &room.id).unwrap();
        reg.apply_discovered(vec![crate::scanner::ProcHit {
            pid: 77,
            provider: Provider::ClaudeCode,
        }]);
        assert_eq!(
            reg.get("cc-77").unwrap().room,
            room.id,
            "扫描差量不得把人踢回大厅"
        );

        // 挪去不存在的房间 → 拒绝
        assert!(reg.move_agent("cc-77", "rm-404").is_err());

        // 改名 / 归档
        reg.update_room(&room.id, Some("重构间"), None).unwrap();
        assert!(
            reg.update_room(&room.id, Some("大厅"), None).is_err(),
            "改名不得撞大厅"
        );
        reg.update_room(&room.id, None, Some(true)).unwrap();
        assert!(reg.rooms().iter().any(|r| r.id == room.id && r.archived));

        // 删房：成员回大厅，事件 = RoomGone + 成员 upsert
        let events = reg.delete_room(&room.id).unwrap();
        assert_eq!(reg.get("cc-77").unwrap().room, DEFAULT_ROOM);
        assert!(matches!(&events[0], SceneEvent::RoomGone { id } if id == &room.id));
        assert!(events
            .iter()
            .skip(1)
            .all(|e| matches!(e, SceneEvent::AgentUpsert { .. })));
        assert!(reg.delete_room(&room.id).is_err(), "已删的房再删报错");
    }
}
