//! pixel-forge TUI：像素车间（v0 状态墙）。
//!
//! 连接 hub 的 /scene WS，渲染当前所有 agent。断线自动重连。
//! 环境变量 `PF_TUI_URL` 覆盖 hub 地址。

use std::{
    io::{self, Stdout},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use pf_scene::{AgentInfo, AgentState, Provider, SceneEvent};
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind},
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("PF_TUI_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:7664/scene".into());

    let agents: Arc<Mutex<Vec<AgentInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let status = Arc::new(Mutex::new(String::from("connecting…")));

    // WS 读取任务：断线退避重连
    {
        let agents = agents.clone();
        let status = status.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((mut ws, _)) = connect_async(url.clone()).await {
                    *status.lock().unwrap() = "connected".into();
                    loop {
                        match ws.next().await {
                            Some(Ok(Message::Text(txt))) => {
                                if let Ok(ev) = serde_json::from_str::<SceneEvent>(&txt) {
                                    apply_event(&agents, ev);
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break,
                        }
                    }
                }
                *status.lock().unwrap() = "reconnecting…".into();
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, agents, status);
    restore_terminal(terminal)?;
    res
}

fn apply_event(agents: &Arc<Mutex<Vec<AgentInfo>>>, ev: SceneEvent) {
    let mut list = agents.lock().unwrap();
    match ev {
        SceneEvent::Snapshot { agents: incoming } => *list = incoming,
        SceneEvent::AgentUpsert { agent } => {
            match list.iter_mut().find(|a| a.id == agent.id) {
                Some(slot) => *slot = agent,
                None => list.push(agent),
            }
            list.sort_by(|a, b| a.id.cmp(&b.id));
        }
        SceneEvent::AgentGone { id } => list.retain(|a| a.id != id),
    }
}

fn run(
    terminal: &mut Term,
    agents: Arc<Mutex<Vec<AgentInfo>>>,
    status: Arc<Mutex<String>>,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|f| draw(f, &agents, &status))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press
                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                {
                    return Ok(());
                }
            }
        }
    }
}

fn draw(f: &mut ratatui::Frame, agents: &Arc<Mutex<Vec<AgentInfo>>>, status: &Arc<Mutex<String>>) {
    let list = agents.lock().unwrap().clone();
    let st = status.lock().unwrap().clone();
    let [main, bottom] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.size());

    let mut lines: Vec<Line> = Vec::new();
    if list.is_empty() {
        lines.push(Line::from("  车间空无一人…在别的终端里跑一个 claude/codex/gemini 试试"));
    }
    for a in &list {
        lines.push(Line::from(vec![
            Span::styled(
                "██ ",
                Style::default()
                    .fg(provider_color(a.provider))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {}  ", a.name)),
            Span::styled(state_glyph(a.state), Style::default().fg(Color::Gray)),
        ]));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" pixel-forge · 车间 "),
    );
    f.render_widget(para, main);
    f.render_widget(Paragraph::new(format!(" {st}  ·  q 退出 ")), bottom);
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
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(mut terminal: Term) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}
