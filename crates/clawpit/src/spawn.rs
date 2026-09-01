//! 宿舍（M1）：hub 自己 spawn 的 agent worker（pty 宿主）。
//!
//! - stdin 注入可靠；stdout 持续排水（pty 缓冲不读会堵死子进程）
//! - 状态映射（粗粒度）：进程活着 = Working；退出码 0 = Done；非 0 = Error。
//!   精确状态（Thinking/WaitingInput）依赖 transcript 解析，M3 观察站接入。
//! - 注册表约定：Spawned 条目归本 driver 管；退出后保留（能看到结果），
//!   显式 stop 才移除。

use std::{
    collections::HashMap,
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use clawpit_scene::{AgentInfo, AgentState, Provider, SceneEvent, Source};

use crate::AppState;

/// provider → 默认启动命令。
pub fn provider_command(p: Provider) -> &'static str {
    match p {
        Provider::ClaudeCode => "claude",
        Provider::Codex => "codex",
        Provider::Gemini => "gemini",
        Provider::Aider => "aider",
        Provider::OpenCode => "opencode",
        Provider::Generic => "sh",
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct SpawnRequest {
    pub provider: Provider,
    pub cwd: Option<String>,
    /// 覆盖启动命令（测试用，如 ["sh", "-c", "echo hi"]）。
    #[serde(default)]
    pub argv: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct SayRequest {
    pub text: String,
}

struct Session {
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    writer: Box<dyn Write + Send>,
}

pub struct SpawnManager {
    state: AppState,
    sessions: Mutex<HashMap<String, Session>>,
    next: AtomicU64,
}

impl SpawnManager {
    pub fn new(state: AppState) -> Arc<Self> {
        Arc::new(Self {
            state,
            sessions: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    /// 招一只 worker：pty spawn → 注册 → 广播 → 后台监听退出。
    pub async fn spawn(self: &Arc<Self>, req: SpawnRequest) -> anyhow::Result<AgentInfo> {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let id = format!("sp-{n}");

        let argv = req
            .argv
            .unwrap_or_else(|| vec![provider_command(req.provider).to_string()]);
        let mut cmd = portable_pty::CommandBuilder::new(&argv[0]);
        for a in &argv[1..] {
            cmd.arg(a);
        }
        if let Some(cwd) = &req.cwd {
            cmd.cwd(cwd);
        }

        let pty = portable_pty::native_pty_system();
        let pair = pty.openpty(portable_pty::PtySize::default())?;
        let child = pair.slave.spawn_command(cmd)?;
        let child_pid = child.process_id().unwrap_or(0);
        let writer = pair.master.take_writer()?;
        let reader = pair.master.try_clone_reader()?;

        // 排水线程：丢弃输出（M1）；M3 在这里接 transcript 解析
        std::thread::spawn(move || {
            let mut r = reader;
            let mut buf = [0u8; 4096];
            while std::io::Read::read(&mut r, &mut buf).unwrap_or(0) > 0 {}
        });

        let agent = AgentInfo {
            id: id.clone(),
            provider: req.provider,
            name: id.clone(),
            state: AgentState::Working,
            source: Source::Spawned { pid: child_pid },
        };
        {
            let child = Arc::new(Mutex::new(child));
            self.sessions.lock().unwrap().insert(
                id.clone(),
                Session {
                    child: child.clone(),
                    writer,
                },
            );
            // 退出监听：try_wait 轮询（短临界区，不与 kill 抢锁死等）
            let mgr = self.clone();
            let agent_id = id.clone();
            tokio::spawn(async move {
                loop {
                    let exited = child.lock().unwrap().try_wait().ok().flatten();
                    if let Some(status) = exited {
                        let state = if status.success() {
                            AgentState::Done
                        } else {
                            AgentState::Error
                        };
                        mgr.on_exit(&agent_id, state).await;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
        }

        self.state.registry.write().await.upsert(agent.clone());
        let _ = self.state.tx.send(SceneEvent::AgentUpsert {
            agent: agent.clone(),
        });
        Ok(agent)
    }

    /// 对 worker 喊话：写 stdin（pty 下 `\r` 即回车提交）。
    pub fn say(&self, id: &str, text: &str) -> anyhow::Result<()> {
        let mut map = self.sessions.lock().unwrap();
        let s = map
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("agent 不存在或已退出: {id}"))?;
        s.writer.write_all(text.as_bytes())?;
        s.writer.write_all(b"\r")?;
        s.writer.flush()?;
        Ok(())
    }

    /// worker 是否还活着（会话仍在 = 可注入）。
    pub fn is_alive(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    /// 停掉并移除 worker（对已退出的条目等于"清理"）。
    pub async fn stop(&self, id: &str) -> anyhow::Result<()> {
        let sess = self.sessions.lock().unwrap().remove(id);
        if let Some(s) = sess {
            let _ = s.child.lock().unwrap().kill();
        }
        self.state.registry.write().await.remove(id);
        let _ = self
            .state
            .tx
            .send(SceneEvent::AgentGone { id: id.to_string() });
        Ok(())
    }

    async fn on_exit(self: Arc<Self>, id: &str, state: AgentState) {
        self.sessions.lock().unwrap().remove(id);
        let mut reg = self.state.registry.write().await;
        if reg.set_state(id, state) {
            if let Some(agent) = reg.get(id).cloned() {
                let _ = self.state.tx.send(SceneEvent::AgentUpsert { agent });
            }
        }
    }
}
