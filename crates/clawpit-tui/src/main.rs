//! clawpit TUI：像素车间。
//!
//! M0：状态墙（发现存量 agent）
//! M1：选中 worker（j/k）、喊话（⏎ 输入回车发送）、招工（n=claude）、解雇（x）
//! 连接 hub 的 /scene WS；断线自动重连。环境变量 `CLAWPIT_TUI_URL` 覆盖 hub 地址。
//!
//! 健壮性约定：选中项用 agent id（下标会因重排漂移→喊错人）；终端状态由
//! TermGuard 的 Drop 恢复（panic 也不泄漏 raw mode）；Ctrl+C 显式处理
//! （raw mode 下 ISIG 被禁，内核不会再发 SIGINT）。

use std::{
    io::{self, Stdout},
    sync::{Arc, Mutex},
    time::Duration,
};

use clawpit_scene::{
    AgentInfo, AgentState, ChatMessage, ClientMessage, Provider, RoomInfo, SceneEvent, DEFAULT_ROOM,
};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// 招工默认 provider（M1 固定 claude）。
const SPAWN_PROVIDER: Provider = Provider::ClaudeCode;

#[derive(Default)]
enum Mode {
    #[default]
    Normal,
    Input(String),
}

struct Ui {
    agents: Vec<AgentInfo>,
    /// 选中的 agent id（不是下标——列表重排后下标会指向别人）
    selected: Option<String>,
    mode: Mode,
    status: String,
    /// 车间对话（最近 50 条，气泡墙）
    messages: Vec<ChatMessage>,
    /// 房间清单与当前房（Tab 切换）；agent 过滤、气泡归属与 Web 同规
    rooms: Vec<RoomInfo>,
    room: String,
}

/// 终端状态守卫：Drop 时恢复 raw mode/备用屏/光标，panic 路径也不泄漏。
struct TermGuard {
    terminal: Option<Term>,
}

impl TermGuard {
    fn setup() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(t) => Ok(Self { terminal: Some(t) }),
            Err(e) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                Err(e)
            }
        }
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        if let Some(mut t) = self.terminal.take() {
            let _ = disable_raw_mode();
            let _ = execute!(t.backend_mut(), LeaveAlternateScreen);
            let _ = t.show_cursor();
        }
    }
}

fn main() -> anyhow::Result<()> {
    let url =
        std::env::var("CLAWPIT_TUI_URL").unwrap_or_else(|_| "ws://127.0.0.1:7664/scene".into());

    let ui = Arc::new(Mutex::new(Ui {
        agents: Vec::new(),
        selected: None,
        mode: Mode::Normal,
        status: "connecting…".into(),
        messages: Vec::new(),
        rooms: Vec::new(),
        room: DEFAULT_ROOM.into(),
    }));
    // 控制面出口：键盘事件 → ClientMessage → WS 上行
    let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel::<ClientMessage>();

    // WS 线程：独立 tokio runtime（下行事件 + 上行控制轮询）
    {
        let ui = ui.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let ctrl_rx = ctrl_rx;
                loop {
                    if let Ok((ws, _)) = connect_async(url.clone()).await {
                        ui.lock().unwrap().status = "connected".into();
                        let (mut sink, mut stream) = ws.split();
                        loop {
                            // 上行：把积压的控制消息发出去（std channel 的 try_recv 直接用）
                            while let Ok(cm) = ctrl_rx.try_recv() {
                                if let Ok(json) = serde_json::to_string(&cm) {
                                    if sink.send(Message::Text(json)).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            // 下行：100ms 一帧的轮询窗口
                            match tokio::time::timeout(Duration::from_millis(100), stream.next())
                                .await
                            {
                                Ok(Some(Ok(Message::Text(txt)))) => {
                                    match serde_json::from_str::<SceneEvent>(&txt) {
                                        Ok(ev) => apply_event(&ui, ev),
                                        Err(_) => {
                                            // 协议不一致时宁可吵醒用户，也不要僵尸视图
                                            ui.lock().unwrap().status =
                                                "事件解析失败（hub 版本不一致？重连中）".into();
                                        }
                                    }
                                }
                                Ok(Some(Ok(_))) => {}
                                Ok(Some(Err(_))) | Ok(None) => break,
                                Err(_) => {} // 窗口超时=无下行，回去看上行
                            }
                        }
                    }
                    ui.lock().unwrap().status = "reconnecting…".into();
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
        });
    }

    // 主线程：终端事件循环（guard 的 Drop 保证任何退出路径都恢复终端）
    let mut guard = TermGuard::setup()?;
    let res = run(guard.terminal.as_mut().unwrap(), ui, ctrl_tx);
    drop(guard);
    res
}

fn apply_event(ui: &Arc<Mutex<Ui>>, ev: SceneEvent) {
    let mut u = ui.lock().unwrap();
    match ev {
        SceneEvent::RoomUpsert { room } => {
            match u.rooms.iter_mut().find(|r| r.id == room.id) {
                Some(slot) => *slot = room,
                None => u.rooms.push(room),
            }
            u.rooms.sort_by(|a, b| a.id.cmp(&b.id));
        }
        SceneEvent::RoomGone { id } => {
            u.rooms.retain(|r| r.id != id);
            if u.room == id {
                u.room = DEFAULT_ROOM.into(); // 成员 upsert 随后会到
            }
        }
        SceneEvent::Snapshot { agents, rooms } => {
            // 快照是全量替换：选中项若已消失必须失效，否则悬空 id 会打到 400
            if let Some(sel) = &u.selected {
                if !agents.iter().any(|a| &a.id == sel) {
                    u.selected = None;
                }
            }
            u.agents = agents;
            if !rooms.is_empty() {
                u.rooms = rooms;
            }
            if !u.rooms.iter().any(|r| r.id == u.room) {
                u.room = DEFAULT_ROOM.into();
            }
        }
        SceneEvent::AgentUpsert { agent } => {
            match u.agents.iter_mut().find(|a| a.id == agent.id) {
                Some(slot) => *slot = agent,
                None => u.agents.push(agent),
            }
            u.agents.sort_by(|a, b| a.id.cmp(&b.id));
        }
        SceneEvent::AgentGone { id } => {
            u.agents.retain(|a| a.id != id);
            if u.selected.as_deref() == Some(id.as_str()) {
                u.selected = None;
            }
        }
        SceneEvent::Chat { message } => {
            u.messages.push(message);
            let len = u.messages.len();
            if len > 50 {
                u.messages.drain(..len - 50);
            }
        }
    }
}

/// 当前选中项在（当前房）列表中的下标（未选中/已消失 = None）。
fn selected_index(u: &Ui) -> Option<usize> {
    let sel = u.selected.as_ref()?;
    visible_agents(u).iter().position(|a| &a.id == sel)
}

/// 当前房的成员（房间归属缺省视为大厅，与 Web 同规）。
fn visible_agents(u: &Ui) -> Vec<&AgentInfo> {
    u.agents.iter().filter(|a| a.room == u.room).collect()
}

/// 气泡归属：发送者或接收者在当前房才上墙。
fn msg_in_room(u: &Ui, m: &ChatMessage) -> bool {
    let in_room = |id: &str| u.agents.iter().any(|a| a.id == id && a.room == u.room);
    in_room(&m.from) || in_room(&m.to)
}

fn room_name(u: &Ui) -> &str {
    u.rooms
        .iter()
        .find(|r| r.id == u.room)
        .map(|r| r.name.as_str())
        .unwrap_or("大厅")
}

fn run(
    terminal: &mut Term,
    ui: Arc<Mutex<Ui>>,
    ctrl: std::sync::mpsc::Sender<ClientMessage>,
) -> anyhow::Result<()> {
    loop {
        {
            let u = ui.lock().unwrap();
            terminal.draw(|f| draw(f, &u))?;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // raw mode 禁了 ISIG，内核不发 SIGINT——Ctrl+C 必须手动处理
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(());
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            continue;
        }

        let mut u = ui.lock().unwrap();
        let mode = std::mem::take(&mut u.mode);
        match (mode, key.code) {
            // ── 输入模式 ──
            (Mode::Input(_), KeyCode::Esc) => {}
            (Mode::Input(buf), KeyCode::Enter) => {
                if buf.is_empty() {
                    // 空输入=取消
                } else if let Some(id) = u.selected.clone() {
                    ctrl.send(ClientMessage::Say { id, text: buf })?;
                } else {
                    u.status = "车间空无一人，先按 n 招工".into();
                }
            }
            (Mode::Input(mut buf), KeyCode::Backspace) => {
                buf.pop();
                u.mode = Mode::Input(buf);
            }
            (Mode::Input(mut buf), KeyCode::Char(c)) => {
                buf.push(c);
                u.mode = Mode::Input(buf);
            }
            (Mode::Input(buf), _) => u.mode = Mode::Input(buf),

            // ── 普通模式（Esc 不退出——输入模式取消后习惯性多按一次不该杀掉整个 TUI）──
            (Mode::Normal, KeyCode::Char('q')) => return Ok(()),
            // Tab 换房：未归档房间循环切换；选中项不在新房则失效（防悬空 id）
            (Mode::Normal, KeyCode::Tab) => {
                let ids: Vec<String> = {
                    let mut v: Vec<String> = u
                        .rooms
                        .iter()
                        .filter(|r| !r.archived)
                        .map(|r| r.id.clone())
                        .collect();
                    v.sort();
                    v
                };
                if !ids.is_empty() {
                    let i = ids
                        .iter()
                        .position(|id| id == &u.room)
                        .map(|i| (i + 1) % ids.len())
                        .unwrap_or(0);
                    u.room = ids[i].clone();
                    if let Some(sel) = u.selected.clone() {
                        if !visible_agents(&u).iter().any(|a| a.id == sel) {
                            u.selected = None;
                        }
                    }
                }
            }
            (Mode::Normal, KeyCode::Char('j') | KeyCode::Down) => {
                let vis = visible_agents(&u);
                if !vis.is_empty() {
                    let next = selected_index(&u)
                        .map(|i| (i + 1).min(vis.len() - 1))
                        .unwrap_or(0);
                    u.selected = Some(vis[next].id.clone());
                }
            }
            (Mode::Normal, KeyCode::Char('k') | KeyCode::Up) => {
                let vis = visible_agents(&u);
                if !vis.is_empty() {
                    let prev = selected_index(&u).map(|i| i.saturating_sub(1)).unwrap_or(0);
                    u.selected = Some(vis[prev].id.clone());
                }
            }
            (Mode::Normal, KeyCode::Enter) => {
                if u.selected.is_none() && u.agents.is_empty() {
                    u.status = "先按 n 招一只 worker".into();
                } else if u.selected.is_none() {
                    u.selected = Some(u.agents[0].id.clone());
                    u.mode = Mode::Input(String::new());
                } else {
                    u.mode = Mode::Input(String::new());
                }
            }
            (Mode::Normal, KeyCode::Char('n')) => {
                ctrl.send(ClientMessage::Spawn {
                    provider: SPAWN_PROVIDER,
                    cwd: None,
                    argv: None,
                })?;
                u.status = format!("已申请招工（{}）…", SPAWN_PROVIDER.short());
            }
            (Mode::Normal, KeyCode::Char('x')) => {
                if let Some(id) = u.selected.clone() {
                    ctrl.send(ClientMessage::Stop { id })?;
                }
            }
            (Mode::Normal, _) => {}
        }
    }
}

fn draw(f: &mut ratatui::Frame, u: &Ui) {
    let [main, chat, input, bottom] = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(8),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let mut lines: Vec<Line> = Vec::new();
    let vis = visible_agents(u);
    if vis.is_empty() {
        lines.push(Line::from(format!(
            "  「{}」空无一人…按 n 招工 / Tab 换房，或在别的终端跑 claude/codex/gemini",
            room_name(u)
        )));
    }
    for a in vis {
        let selected = u.selected.as_deref() == Some(a.id.as_str());
        let cursor = if selected { "▶" } else { " " };
        let name_style = if selected {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw(format!("{cursor} ")),
            Span::styled(
                "██",
                Style::default()
                    .fg(provider_color(a.provider))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}  ", a.name), name_style),
            Span::styled(state_glyph(a.state), Style::default().fg(Color::Gray)),
            // 任务名牌：观察站从 transcript 推的"正在干什么"，一眼分辨车间里每只小人
            Span::styled(
                format!(" {}", title_short(a.title.as_deref().unwrap_or(""))),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" clawpit · 车间 · {} ", room_name(u))),
    );
    f.render_widget(para, main);

    // 消息墙：当前房最近的话（气泡归属与 Web 同规）
    let chat_lines: Vec<Line> = u
        .messages
        .iter()
        .filter(|m| msg_in_room(u, m))
        .rev()
        .take(6)
        .map(|m| {
            Line::from(vec![
                Span::styled(
                    format!("[{}→{}] ", m.from_name, m.to),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(m.text.clone()),
            ])
        })
        .collect();
    let chat_para = Paragraph::new(chat_lines)
        .block(Block::default().borders(Borders::ALL).title(" 车间消息 "));
    f.render_widget(chat_para, chat);

    // 输入行：超长只显示尾部（盲打比截断更糟）
    let input_line = match &u.mode {
        Mode::Input(buf) => {
            let count = buf.chars().count();
            let shown: String = if count > 40 {
                let skip = count - 40;
                format!("…{}", buf.chars().skip(skip).collect::<String>())
            } else {
                buf.clone()
            };
            Line::from(format!(" 喊话› {shown}_"))
        }
        Mode::Normal => Line::from(""),
    };
    f.render_widget(Paragraph::new(input_line), input);

    let hint = match &u.mode {
        Mode::Input(_) => " Enter 发送 · Esc 取消 ".to_string(),
        Mode::Normal => format!(
            " {} | j/k 选人 · ⏎ 喊话 · n 招工({}) · x 解雇 · q/Ctrl+C 退出 ",
            u.status,
            SPAWN_PROVIDER.short()
        ),
    };
    f.render_widget(Paragraph::new(hint), bottom);
}

fn provider_color(p: Provider) -> Color {
    match p {
        Provider::ClaudeCode => Color::LightMagenta,
        Provider::Codex => Color::LightGreen,
        Provider::Gemini => Color::LightBlue,
        Provider::Aider => Color::LightRed,
        Provider::OpenCode => Color::LightYellow,
        Provider::Generic => Color::Gray,
    }
}

/// 名牌一行显示的长度（终端窄，超过就截）。
fn title_short(t: &str) -> String {
    let mut out: String = t.chars().take(24).collect();
    if t.chars().count() > 24 {
        out.push('…');
    }
    out
}

fn state_glyph(s: AgentState) -> &'static str {
    match s {
        AgentState::Unknown => "？",
        AgentState::Thinking => "…在想",
        AgentState::Working => "⚒ 干活",
        AgentState::WaitingInput => "⏸ 等指令",
        AgentState::Done => "✓ 完成",
        AgentState::Error => "✗ 出错",
    }
}
