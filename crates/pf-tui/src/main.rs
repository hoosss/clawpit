//! pixel-forge TUI：像素车间。
//!
//! M0：状态墙（发现存量 agent）
//! M1：选中 worker（j/k）、喊话（⏎ 输入回车发送）、招工（n=claude）、解雇（x）
//! 连接 hub 的 /scene WS；断线自动重连。环境变量 `PF_TUI_URL` 覆盖 hub 地址。

use std::{
    io::{self, Stdout},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use pf_scene::{AgentInfo, AgentState, ClientMessage, Provider, SceneEvent};
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
    selected: usize,
    mode: Mode,
    status: String,
}

fn main() -> anyhow::Result<()> {
    let url = std::env::var("PF_TUI_URL").unwrap_or_else(|_| "ws://127.0.0.1:7664/scene".into());

    let ui = Arc::new(Mutex::new(Ui {
        agents: Vec::new(),
        selected: 0,
        mode: Mode::Normal,
        status: "connecting…".into(),
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
                                    if let Ok(ev) = serde_json::from_str::<SceneEvent>(&txt) {
                                        apply_event(&ui, ev);
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

    // 主线程：终端事件循环
    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, ui, ctrl_tx);
    restore_terminal(terminal)?;
    res
}

fn apply_event(ui: &Arc<Mutex<Ui>>, ev: SceneEvent) {
    let mut u = ui.lock().unwrap();
    match ev {
        SceneEvent::Snapshot { agents } => u.agents = agents,
        SceneEvent::AgentUpsert { agent } => {
            match u.agents.iter_mut().find(|a| a.id == agent.id) {
                Some(slot) => *slot = agent,
                None => u.agents.push(agent),
            }
            u.agents.sort_by(|a, b| a.id.cmp(&b.id));
        }
        SceneEvent::AgentGone { id } => {
            u.agents.retain(|a| a.id != id);
            if u.selected >= u.agents.len() {
                u.selected = u.agents.len().saturating_sub(1);
            }
        }
    }
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
        if key.kind != KeyEventKind::Press || key.modifiers.contains(KeyModifiers::CONTROL) {
            continue;
        }

        let mut u = ui.lock().unwrap();
        let mode = std::mem::take(&mut u.mode);
        match (mode, key.code) {
            // ── 输入模式 ──
            (Mode::Input(_), KeyCode::Esc) => {}
            (Mode::Input(buf), KeyCode::Enter) => {
                let target = u.agents.get(u.selected).map(|a| a.id.clone());
                if buf.is_empty() {
                    // 空输入=取消
                } else if let Some(id) = target {
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

            // ── 普通模式 ──
            (Mode::Normal, KeyCode::Char('q') | KeyCode::Esc) => return Ok(()),
            (Mode::Normal, KeyCode::Char('j') | KeyCode::Down) => {
                if !u.agents.is_empty() {
                    u.selected = (u.selected + 1).min(u.agents.len() - 1);
                }
            }
            (Mode::Normal, KeyCode::Char('k') | KeyCode::Up) => {
                u.selected = u.selected.saturating_sub(1);
            }
            (Mode::Normal, KeyCode::Enter) => {
                if u.agents.is_empty() {
                    u.status = "先按 n 招一只 worker".into();
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
                if let Some(a) = u.agents.get(u.selected) {
                    let id = a.id.clone();
                    ctrl.send(ClientMessage::Stop { id })?;
                }
            }
            (Mode::Normal, _) => {}
        }
    }
}

fn draw(f: &mut ratatui::Frame, u: &Ui) {
    let [main, input, bottom] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let mut lines: Vec<Line> = Vec::new();
    if u.agents.is_empty() {
        lines.push(Line::from(
            "  车间空无一人…按 n 招工，或在别的终端跑 claude/codex/gemini",
        ));
    }
    for (i, a) in u.agents.iter().enumerate() {
        let selected = i == u.selected;
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
        ]));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" pixel-forge · 车间 "),
    );
    f.render_widget(para, main);

    // 输入行（喊话编辑区）
    let input_line = match &u.mode {
        Mode::Input(buf) => Line::from(format!(" 喊话› {buf}_")),
        Mode::Normal => Line::from(""),
    };
    f.render_widget(Paragraph::new(input_line), input);

    let hint = match &u.mode {
        Mode::Input(_) => " Enter 发送 · Esc 取消 ".to_string(),
        Mode::Normal => format!(
            " {} | j/k 选人 · ⏎ 喊话 · n 招工({}) · x 解雇 · q 退出 ",
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

fn setup_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(mut terminal: Term) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}
