//! 任务层（平台阶段一）：统一调度的一等公民。
//!
//! 调度规则（v1 刻意简单，规则可解释）：
//! 1. 指定 agent_id → 直派（不挑状态，人对你负责）
//! 2. 指定 provider → 找该工具的空闲 agent（waiting_input/done），没有就招工
//! 3. 都不指定 → 任意空闲，没有就招 claude
//!
//! 任务下达复用邮局（stdin/tmux/收件箱三通路 + delivered 报告）。
//! 状态联动：assignee 回到 waiting_input/done → 任务自动 done；消失/出错 → failed。

use std::collections::HashMap;

use clawpit_scene::{AgentInfo, AgentState, Provider, TaskInfo, TaskStatus};

use crate::{spawn::SpawnRequest, Hub};

#[derive(Default)]
pub struct TaskRegistry {
    tasks: HashMap<String, TaskInfo>,
    next: u64,
}

impl TaskRegistry {
    pub fn create(
        &mut self,
        title: &str,
        brief: &str,
        created_by: &str,
        room: &str,
    ) -> anyhow::Result<TaskInfo> {
        let title = title.trim();
        anyhow::ensure!(!title.is_empty(), "任务标题不能为空");
        anyhow::ensure!(title.chars().count() <= 80, "任务标题过长（≤80 字符）");
        self.next += 1;
        let task = TaskInfo {
            id: format!("task-{}", self.next),
            title: title.to_string(),
            brief: if brief.trim().is_empty() {
                title.to_string()
            } else {
                brief.trim().to_string()
            },
            status: TaskStatus::Queued,
            assignee: None,
            created_by: created_by.to_string(),
            room: room.to_string(),
        };
        self.tasks.insert(task.id.clone(), task.clone());
        Ok(task)
    }

    pub fn upsert(&mut self, task: TaskInfo) {
        if let Some(n) = task
            .id
            .strip_prefix("task-")
            .and_then(|s| s.parse::<u64>().ok())
        {
            self.next = self.next.max(n);
        }
        self.tasks.insert(task.id.clone(), task);
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.tasks.remove(id).is_some()
    }

    pub fn get(&self, id: &str) -> Option<&TaskInfo> {
        self.tasks.get(id)
    }

    /// 全量快照（按 id 排序，渲染端稳定）。
    pub fn snapshot(&self) -> Vec<TaskInfo> {
        let mut v: Vec<TaskInfo> = self.tasks.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn assign(&mut self, id: &str, agent: &str) -> Option<TaskInfo> {
        let t = self.tasks.get_mut(id)?;
        if t.status != TaskStatus::Queued {
            return None; // 已派的不能重派（取消后重新建）
        }
        t.status = TaskStatus::Assigned;
        t.assignee = Some(agent.to_string());
        Some(t.clone())
    }

    fn transit(&mut self, id: &str, to: TaskStatus) -> Option<TaskInfo> {
        let t = self.tasks.get_mut(id)?;
        if t.status != TaskStatus::Assigned {
            return None;
        }
        t.status = to;
        Some(t.clone())
    }

    pub fn complete(&mut self, id: &str) -> Option<TaskInfo> {
        self.transit(id, TaskStatus::Done)
    }

    pub fn fail(&mut self, id: &str) -> Option<TaskInfo> {
        self.transit(id, TaskStatus::Failed)
    }

    /// 状态联动：agent 到达"等人"态 → 它名下的 assigned 任务全部 done。
    /// 返回变更后的任务（调用方负责广播）。
    pub fn reconcile_agent_state(&mut self, agent_id: &str, state: AgentState) -> Vec<TaskInfo> {
        match state {
            AgentState::WaitingInput | AgentState::Done => {
                let ids: Vec<String> = self
                    .tasks
                    .values()
                    .filter(|t| {
                        t.assignee.as_deref() == Some(agent_id) && t.status == TaskStatus::Assigned
                    })
                    .map(|t| t.id.clone())
                    .collect();
                ids.iter().filter_map(|id| self.complete(id)).collect()
            }
            AgentState::Error => {
                let ids: Vec<String> = self
                    .tasks
                    .values()
                    .filter(|t| {
                        t.assignee.as_deref() == Some(agent_id) && t.status == TaskStatus::Assigned
                    })
                    .map(|t| t.id.clone())
                    .collect();
                ids.iter().filter_map(|id| self.fail(id)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// agent 消失 → 名下 assigned 任务全部 failed。
    pub fn reconcile_agent_gone(&mut self, agent_id: &str) -> Vec<TaskInfo> {
        let ids: Vec<String> = self
            .tasks
            .values()
            .filter(|t| t.assignee.as_deref() == Some(agent_id) && t.status == TaskStatus::Assigned)
            .map(|t| t.id.clone())
            .collect();
        ids.iter().filter_map(|id| self.fail(id)).collect()
    }

    /// 启动重放后的清账：assignee 已不存在的 assigned 任务 → failed。
    pub fn reconcile_boot(&mut self, live_agent_ids: &[String]) -> Vec<TaskInfo> {
        let ids: Vec<String> = self
            .tasks
            .values()
            .filter(|t| {
                t.status == TaskStatus::Assigned
                    && t.assignee
                        .as_ref()
                        .is_some_and(|a| !live_agent_ids.contains(a))
            })
            .map(|t| t.id.clone())
            .collect();
        ids.iter().filter_map(|id| self.fail(id)).collect()
    }
}

/// 空闲判定：等人/已完成的 agent 可以接新活；在想/在干的先不打扰。
fn idle(a: &AgentInfo) -> bool {
    matches!(a.state, AgentState::WaitingInput | AgentState::Done)
}

/// 调度请求：三选一（agent_id 直派 > provider 定工具 > 默认任意）。
#[derive(Debug, Default)]
pub struct DispatchRequest {
    pub title: String,
    pub brief: String,
    pub agent_id: Option<String>,
    pub provider: Option<Provider>,
    pub created_by: String,
    pub room: String,
}

/// 挑人 + 下达 + 登记指派。任务先建（Queued），派成才转 Assigned。
/// 返回（任务, 承接 agent, 投递去向文案）。
pub async fn dispatch(
    hub: &Hub,
    req: DispatchRequest,
) -> anyhow::Result<(TaskInfo, AgentInfo, String)> {
    let task =
        hub.state
            .tasks
            .write()
            .await
            .create(&req.title, &req.brief, &req.created_by, &req.room)?;
    hub.state
        .emit(clawpit_scene::SceneEvent::TaskUpsert { task: task.clone() });

    // ── 挑人 ──
    let picked: AgentInfo = if let Some(id) = &req.agent_id {
        hub.state
            .registry
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("agent 不存在: {id}"))?
    } else {
        let want_provider = req.provider;
        let found = hub
            .state
            .registry
            .read()
            .await
            .snapshot()
            .into_iter()
            .filter(|a| want_provider.is_none_or(|p| a.provider == p))
            .find(idle);
        match found {
            Some(a) => a,
            None => {
                // 没有空闲的 → 招工（provider 未指定时默认 claude）
                let provider = want_provider.unwrap_or(Provider::ClaudeCode);
                hub.spawn
                    .spawn(SpawnRequest {
                        provider,
                        cwd: None,
                        argv: None,
                    })
                    .await?
            }
        }
    };

    // ── 下达（复用邮局三通路）──
    let payload = format!("[任务 {}] {}", task.id, task.brief);
    let (.., delivery) = hub.mail.send(None, &picked.id, &payload).await?;
    let delivery_text = match delivery {
        crate::mail::Delivery::InjectedStdin => "stdin 注入".into(),
        crate::mail::Delivery::InjectedTmux => "tmux 注入".into(),
        crate::mail::Delivery::Inbox => "收件箱（对方会取信）".into(),
        crate::mail::Delivery::Wall => "只上墙".into(),
    };

    // ── 登记 + 广播 ──
    let assigned = hub
        .state
        .tasks
        .write()
        .await
        .assign(&task.id, &picked.id)
        .ok_or_else(|| anyhow::anyhow!("任务 {} 状态已变，派发失败", task.id))?;
    hub.state.emit(clawpit_scene::SceneEvent::TaskUpsert {
        task: assigned.clone(),
    });
    Ok((assigned, picked, delivery_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: TaskStatus, assignee: Option<&str>) -> TaskInfo {
        TaskInfo {
            id: id.into(),
            title: "t".into(),
            brief: "b".into(),
            status,
            assignee: assignee.map(String::from),
            created_by: "human".into(),
            room: "lobby".into(),
        }
    }

    #[test]
    fn lifecycle_and_rules() {
        let mut reg = TaskRegistry::default();
        // 建任务：空标题拒绝；brief 空时回退标题
        assert!(reg.create("", "", "human", "lobby").is_err());
        let t = reg.create("修登录", "", "human", "lobby").unwrap();
        assert_eq!(t.id, "task-1");
        assert_eq!(t.brief, "修登录");
        assert_eq!(t.status, TaskStatus::Queued);

        // 派发：Queued→Assigned；重复派发拒绝
        let a = reg.assign("task-1", "cc-1").unwrap();
        assert_eq!(a.status, TaskStatus::Assigned);
        assert_eq!(a.assignee.as_deref(), Some("cc-1"));
        assert!(reg.assign("task-1", "cc-2").is_none());

        // 完成：Assigned→Done；再 complete 无效
        assert!(reg.complete("task-1").is_some());
        assert!(reg.complete("task-1").is_none());

        // 状态联动：assigned 任务随 agent 等人而完成
        reg.upsert(task("task-2", TaskStatus::Assigned, Some("cc-1")));
        let done = reg.reconcile_agent_state("cc-1", AgentState::WaitingInput);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].status, TaskStatus::Done);

        // 出错 → failed；消失 → failed
        reg.upsert(task("task-3", TaskStatus::Assigned, Some("cx-2")));
        let failed = reg.reconcile_agent_state("cx-2", AgentState::Error);
        assert_eq!(failed[0].status, TaskStatus::Failed);
        reg.upsert(task("task-4", TaskStatus::Assigned, Some("gone-3")));
        assert_eq!(
            reg.reconcile_agent_gone("gone-3")[0].status,
            TaskStatus::Failed
        );

        // 启动清账：assignee 不在存活名单 → failed
        reg.upsert(task("task-5", TaskStatus::Assigned, Some("ghost")));
        let ids = vec!["cc-1".to_string()];
        let boot = reg.reconcile_boot(&ids);
        assert!(boot
            .iter()
            .any(|t| t.id == "task-5" && t.status == TaskStatus::Failed));
    }

    #[tokio::test]
    async fn dispatch_picks_idle_and_direct() -> anyhow::Result<()> {
        use crate::{scanner::ProcHit, AppState, SpawnManager};
        use clawpit_scene::Provider;

        let state = AppState::new();
        let spawn = SpawnManager::new(state.clone());
        let mail = crate::mail::MailManager::new(state.clone(), spawn.clone());
        let hub = crate::Hub {
            state: state.clone(),
            spawn,
            mail,
        };

        // 两个假 discovered：一个在干（不打扰）、一个等人（空闲）
        state.registry.write().await.apply_discovered(vec![
            ProcHit {
                pid: 100,
                provider: Provider::ClaudeCode,
                exe: None,
            },
            ProcHit {
                pid: 200,
                provider: Provider::ClaudeCode,
                exe: None,
            },
        ]);
        state.registry.write().await.apply_states(&[
            ("cc-100".into(), AgentState::Working),
            ("cc-200".into(), AgentState::WaitingInput),
        ]);

        // 自动路径：挑中空闲的 cc-200（不是在干的 cc-100），消息进收件箱（非 tmux）
        let (t1, agent, delivery) = dispatch(
            &hub,
            DispatchRequest {
                title: "自动挑空闲".into(),
                brief: "干活".into(),
                agent_id: None,
                provider: Some(Provider::ClaudeCode),
                created_by: "human".into(),
                room: "lobby".into(),
            },
        )
        .await?;
        assert_eq!(agent.id, "cc-200", "必须挑空闲的，不打扰在干的");
        assert_eq!(t1.status, TaskStatus::Assigned);
        assert_eq!(t1.assignee.as_deref(), Some("cc-200"));
        assert!(
            delivery.contains("收件箱"),
            "非 tmux 外部会话走收件箱：{delivery}"
        );

        // 直派路径：指定在干的 cc-100 也照派（人对你负责）
        let (t2, agent2, _) = dispatch(
            &hub,
            DispatchRequest {
                title: "直派".into(),
                brief: "".into(),
                agent_id: Some("cc-100".into()),
                provider: None,
                created_by: "human".into(),
                room: "lobby".into(),
            },
        )
        .await?;
        assert_eq!(agent2.id, "cc-100");
        assert_eq!(t2.brief, "直派", "brief 空时回退标题");

        // 直派不存在的 agent → 报错，任务留队（可删）
        let err = dispatch(
            &hub,
            DispatchRequest {
                title: "幽灵".into(),
                brief: "".into(),
                agent_id: Some("cc-404".into()),
                provider: None,
                created_by: "human".into(),
                room: "lobby".into(),
            },
        )
        .await;
        assert!(err.is_err());
        assert!(hub
            .state
            .tasks
            .read()
            .await
            .get("task-3")
            .is_some_and(|t| t.status == TaskStatus::Queued));
        Ok(())
    }
}
