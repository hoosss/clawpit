//! 观察站（M3）：给外部 agent 读出真实状态。
//!
//! 原理：运行中的 CLI 会把会话日志（transcript）文件保持打开，
//! `/proc/<pid>/fd/*` 的符号链接直接指向它——零配置定位；
//! tail 最后 8KB，按最后一个有意义事件推断状态。
//! 目前只认 Claude Code 的 transcript 事件模型（type=user/assistant）。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use clawpit_scene::AgentState;

/// 找 pid 的 transcript。两级定位：
/// 1. fd 链接（进程若保持日志打开，最精确——部分 CLI 是这么做的）
/// 2. cwd 推导：Claude Code 的 transcript 在 `~/.claude/projects/<cwd的slug>/<session>.jsonl`，
///    取目录里 mtime 最新的（活跃会话每次事件都会写）。
///    已知局限：同 cwd 并发多个会话时无法区分，都显示最新那份的状态。
pub fn resolve_transcript(proc_root: &Path, pid: u32, claude_home: &Path) -> Option<PathBuf> {
    // 1) fd 直连
    let fd_dir = proc_root.join(pid.to_string()).join("fd");
    if let Ok(entries) = std::fs::read_dir(&fd_dir) {
        for e in entries.flatten() {
            let Ok(target) = std::fs::read_link(e.path()) else {
                continue;
            };
            let s = target.to_string_lossy();
            if s.ends_with(".jsonl") && s.contains("projects") {
                return Some(if target.is_absolute() {
                    target
                } else {
                    e.path().parent().unwrap().join(&target)
                });
            }
        }
    }
    // 2) cwd → projects/<slug>/ 最新 jsonl
    let cwd = std::fs::read_link(proc_root.join(pid.to_string()).join("cwd")).ok()?;
    let proj_dir = claude_home
        .join("projects")
        .join(slug(&cwd.to_string_lossy()));
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(&proj_dir).ok()?.flatten() {
        if e.path().extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, e.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// Claude Code 的目录 slug 规则：`/` 和 `.` 都替换为 `-`。
fn slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// tail 最后 8KB，按最后几行推断状态（纯函数，好测）。
/// 事件模型（Claude Code transcript）：
/// - 最后是 user 行 → 刚收到输入：Thinking
/// - 最后是 assistant 行：
///   - stop_reason=end_turn → 说完话等人：WaitingInput
///   - 含 tool_use → 正在调工具干活：Working
///   - 其他（纯文本流式中）→ Thinking
/// - system/summary 等行跳过，往更早找
pub fn parse_state(tail: &str) -> AgentState {
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match ty {
            "user" => return AgentState::Thinking,
            "assistant" => {
                let stopped = v.pointer("/message/stop_reason").and_then(|x| x.as_str());
                let has_tool_use = v
                    .pointer("/message/content")
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .any(|i| i.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                    })
                    .unwrap_or(false);
                return match (stopped, has_tool_use) {
                    (Some("end_turn"), _) => AgentState::WaitingInput,
                    (_, true) => AgentState::Working,
                    _ => AgentState::Thinking,
                };
            }
            _ => continue,
        }
    }
    AgentState::Unknown
}

/// 定位并读出 pid 的真实状态（定位不到 = Unknown）。
pub fn read_state(proc_root: &Path, pid: u32, claude_home: &Path) -> AgentState {
    read_observation(proc_root, pid, claude_home).state
}

// ── Codex 适配：rollout 日志（~/.codex/sessions/**/*.jsonl） ──────────────

/// 找 codex 会话日志：fd 链接直连（codex 保持 rollout 文件打开）。
/// 刻意不做 cwd/最新文件推导——codex 目录按日期/uuid 组织，与进程对不上号，
/// 宁可观察不到也不能把别人的会话错认成它。
pub fn resolve_codex_transcript(proc_root: &Path, pid: u32) -> Option<PathBuf> {
    let fd_dir = proc_root.join(pid.to_string()).join("fd");
    let entries = std::fs::read_dir(&fd_dir).ok()?;
    for e in entries.flatten() {
        let Ok(target) = std::fs::read_link(e.path()) else {
            continue;
        };
        let s = target.to_string_lossy();
        if s.ends_with(".jsonl") && s.contains(".codex") {
            return Some(if target.is_absolute() {
                target
            } else {
                e.path().parent().unwrap().join(&target)
            });
        }
    }
    None
}

/// codex rollout 事件模型（response_item）：
/// - payload.type=message role=user（input_text）→ 刚收到输入：Thinking
/// - payload.type=function_call / custom_tool_call → 调工具干活：Working
/// - payload.type=message role=assistant（output_text）→ 说完话等人：WaitingInput
/// - reasoning / turn_context / session_meta / event_msg 跳过，往更早找
///
/// 标题：最后一个真人 input_text（过滤环境注入/指令标签）。
pub fn parse_codex(tail: &str) -> Observation {
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("response_item") {
            continue;
        }
        let payload_type = v.pointer("/payload/type").and_then(|x| x.as_str());
        let role = v.pointer("/payload/role").and_then(|x| x.as_str());
        match (payload_type, role) {
            (Some("function_call"), _) | (Some("custom_tool_call"), _) => {
                return Observation {
                    state: AgentState::Working,
                    title: None,
                }
            }
            (Some("message"), Some("user")) => {
                let text = codex_text_blocks(&v, "input_text");
                if let Some(t) = text.filter(|t| is_human_prompt(t)) {
                    return Observation {
                        state: AgentState::Thinking,
                        title: Some(clean_title(&t)),
                    };
                }
                // 环境注入等非人话 user 行：继续往更早找
            }
            (Some("message"), Some("assistant")) => {
                return Observation {
                    state: AgentState::WaitingInput,
                    title: None,
                }
            }
            _ => {}
        }
    }
    Observation::default()
}

/// 拼 codex message content 里指定类型块的文本。
fn codex_text_blocks(v: &serde_json::Value, block_type: &str) -> Option<String> {
    let texts: Vec<&str> = v
        .pointer("/payload/content")?
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some(block_type))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join(" "))
    }
}

/// 真人输入判定：codex 会往 user 通道塞 <user_instructions>/<environment_context>
/// 这类系统注入，跟 claude 的斜杠命令一样不算任务。
fn is_human_prompt(t: &str) -> bool {
    let t = t.trim();
    !t.is_empty() && !t.starts_with('<') && !t.starts_with("# ")
}

/// 标题截断（与 claude 侧同规：含省略号共 40 字符）。
fn clean_title(s: &str) -> String {
    let t = s.trim();
    let mut out: String = t.chars().take(39).collect();
    if t.chars().count() > 39 {
        out.push('…');
    }
    out
}

/// codex 版 read_observation。
pub fn read_codex_observation(proc_root: &Path, pid: u32) -> Observation {
    let Some(path) = resolve_codex_transcript(proc_root, pid) else {
        return Observation::default();
    };
    let Some(tail) = tail_utf8(&path) else {
        return Observation::default();
    };
    parse_codex(&tail)
}

/// 一次 tail 同时拿状态和标题（定位不到 = Unknown / None）。
pub fn read_observation(proc_root: &Path, pid: u32, claude_home: &Path) -> Observation {
    let Some(path) = resolve_transcript(proc_root, pid, claude_home) else {
        return Observation::default();
    };
    let Some(tail) = tail_utf8(&path) else {
        return Observation::default();
    };
    Observation {
        state: parse_state(&tail),
        title: parse_title(&tail),
    }
}

/// 读文件尾部 8KB 并对齐到行首（窗口起点可能落在多字节 UTF-8 中间，
/// 先按字节读再 lossy 解码，否则中文会话的 tail 大概率整窗作废）。
fn tail_utf8(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(8192);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    f.take(8192 + 16).read_to_end(&mut bytes).ok()?;
    if start > 0 {
        match bytes.iter().position(|&b| b == b'\n') {
            Some(nl) => {
                bytes.drain(..=nl);
            }
            None => bytes.clear(), // 窗口内没有完整行，放弃
        }
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// 观察结果：真实状态 + 会话标题（最后一个真人输入，截断到一行）。
#[derive(Debug)]
pub struct Observation {
    pub state: AgentState,
    pub title: Option<String>,
}

impl Default for Observation {
    fn default() -> Self {
        Self {
            state: AgentState::Unknown,
            title: None,
        }
    }
}

/// 从 tail 里找最后一个"真人输入"当会话标题（纯函数，好测）。
/// 规则：倒序扫 user 行；content 为字符串或含 text 块才算，tool_result 不算；
/// 斜杠命令 / <system-reminder> / <command-*> / caveat 开头的行不算；
/// 找到后按字符截到 40，超长补省略号。
pub fn parse_title(tail: &str) -> Option<String> {
    fn clean(s: &str) -> String {
        let t = s.trim();
        let mut out: String = t.chars().take(39).collect();
        if t.chars().count() > 39 {
            out.push('…');
        }
        out
    }
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("user") {
            continue;
        }
        let Some(content) = v.pointer("/message/content") else {
            continue;
        };
        let text = match content {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(arr) => {
                let texts: Vec<&str> = arr
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect();
                if texts.is_empty() {
                    None // tool_result-only：不是人话
                } else {
                    Some(texts.join(" "))
                }
            }
            _ => None,
        };
        let Some(text) = text else { continue };
        let t = text.trim();
        if t.is_empty()
            || t.starts_with('<')
            || t.starts_with("Caveat:")
            || t.starts_with("[Request interrupted")
        {
            continue;
        }
        return Some(clean(t));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_by_last_event() {
        let user = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        let asst_end = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"#;
        let asst_tool = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}"#;
        let asst_text =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"…" }]}}"#;
        let sys = r#"{"type":"system","subtype":"init"}"#;

        assert_eq!(parse_state(user), AgentState::Thinking);
        assert_eq!(parse_state(asst_end), AgentState::WaitingInput);
        assert_eq!(parse_state(asst_tool), AgentState::Working);
        assert_eq!(parse_state(asst_text), AgentState::Thinking);
        // system 行要跳过看更早
        assert_eq!(
            parse_state(&format!("{sys}\n{user}\n")),
            AgentState::Thinking
        );
        assert_eq!(parse_state(""), AgentState::Unknown);
        assert_eq!(parse_state("not json"), AgentState::Unknown);
    }

    #[test]
    fn resolve_via_fd_symlink_and_cwd_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("claude-home");

        // ── 场景 A：fd 直连（进程保持日志打开） ──
        let log_a = home.join("projects/aaa.jsonl");
        std::fs::create_dir_all(log_a.parent().unwrap()).unwrap();
        std::fs::write(
            &log_a,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use"}]}}"#,
        )
        .unwrap();
        let fd = dir.path().join("4242/fd");
        std::fs::create_dir_all(&fd).unwrap();
        std::os::unix::fs::symlink(&log_a, fd.join("3")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", fd.join("4")).unwrap();
        assert_eq!(read_state(dir.path(), 4242, &home), AgentState::Working);

        // ── 场景 B：fd 没有 → cwd 推导 + 最新 mtime ──
        let proj = home.join("projects/-home-u-demo"); // cwd=/home/u.demo 的 slug
        std::fs::create_dir_all(&proj).unwrap();
        let old = proj.join("old.jsonl");
        std::fs::write(&old, r#"{"type":"user"}"#).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let new = proj.join("new.jsonl");
        std::fs::write(
            &new,
            r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}"#,
        )
        .unwrap();
        let pdir = dir.path().join("5000");
        std::fs::create_dir_all(pdir.join("fd")).unwrap();
        std::os::unix::fs::symlink("/home/u.demo", pdir.join("cwd")).unwrap();
        // 应选 mtime 最新的 new.jsonl → WaitingInput
        assert_eq!(
            read_state(dir.path(), 5000, &home),
            AgentState::WaitingInput
        );

        // cwd 也读不到 → Unknown
        assert_eq!(read_state(dir.path(), 9999, &home), AgentState::Unknown);
    }

    #[test]
    fn slug_replaces_slash_and_dot() {
        assert_eq!(
            slug("/home/jinxing.hu/p/edding-erp"),
            "-home-jinxing-hu-p-edding-erp"
        );
    }

    #[test]
    fn parse_title_picks_last_real_user_prompt() {
        // 纯字符串 content 的 user 行 → 直接用
        let plain = r#"{"type":"user","message":{"content":"修复登录页的空指针"}}"#;
        assert_eq!(parse_title(plain).as_deref(), Some("修复登录页的空指针"));
        // 数组 content 带 text 块 → 拼 text
        let blocks =
            r#"{"type":"user","message":{"content":[{"type":"text","text":"先写测试再实现"}]}}"#;
        assert_eq!(parse_title(blocks).as_deref(), Some("先写测试再实现"));
        // tool_result-only 的 user 行不是人话，跳过，往更早找
        let toolres = concat!(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"真正的任务"}}"#
        );
        assert_eq!(parse_title(toolres).as_deref(), Some("真正的任务"));
        // 斜杠命令 / 系统提醒行不算任务
        let cmd = concat!(
            r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"<system-reminder>别用</system-reminder>"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"正经需求"}}"#
        );
        assert_eq!(parse_title(cmd).as_deref(), Some("正经需求"));
        // 超长截断
        let long = format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "很".repeat(80)
        );
        let got = parse_title(&long).unwrap();
        assert!(got.chars().count() <= 40, "超长标题必须截断，得到 {got}");
        // 没有 user 行 → None
        assert_eq!(
            parse_title(r#"{"type":"assistant","message":{"content":[]}}"#),
            None
        );
        assert_eq!(parse_title(""), None);
    }

    #[test]
    fn tail_window_starting_mid_utf8_char_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("claude-home");
        let log = home.join("projects/aaa.jsonl");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let tool_line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use"}]}}"#;
        // 构造 >8KB 文件，且窗口起点落在"中"字（3 字节 UTF-8）中间
        let mut content = String::new();
        content.push_str(&"中".repeat(3000)); // 9000 字节，无换行（起点必在多字节字符内）
        content.push('\n');
        content.push_str(tool_line);
        content.push('\n');
        std::fs::write(&log, content).unwrap();
        let fd = dir.path().join("4242/fd");
        std::fs::create_dir_all(&fd).unwrap();
        std::os::unix::fs::symlink(&log, fd.join("3")).unwrap();

        assert_eq!(
            read_state(dir.path(), 4242, &home),
            AgentState::Working,
            "窗口起点落在 UTF-8 字符中间也不应作废整窗"
        );
    }

    #[test]
    fn codex_state_and_title_from_rollout() {
        let user = r#"{"timestamp":"t1","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"修 codex 适配"}]}}"#;
        let env_inject = r#"{"timestamp":"t0","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd=/tmp</environment_context>"}]}}"#;
        let tool = r#"{"timestamp":"t2","type":"response_item","payload":{"type":"function_call","name":"shell"}}"#;
        let asst = r#"{"timestamp":"t3","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#;
        let meta = r#"{"timestamp":"t4","type":"turn_context","payload":{}}"#;

        // user 之后调工具 → Working
        assert_eq!(
            parse_codex(&format!("{user}\n{tool}\n")).state,
            AgentState::Working
        );
        // 最后是真人 user → Thinking + 标题
        let o = parse_codex(user);
        assert_eq!(o.state, AgentState::Thinking);
        assert_eq!(o.title.as_deref(), Some("修 codex 适配"));
        // 环境注入的 user 行不算人话，往更早找
        let o = parse_codex(&format!("{user}\n{env_inject}"));
        assert_eq!(o.title.as_deref(), Some("修 codex 适配"));
        // assistant 收尾（turn_context 跳过）→ WaitingInput
        assert_eq!(
            parse_codex(&format!("{tool}\n{asst}\n{meta}")).state,
            AgentState::WaitingInput
        );
        assert_eq!(parse_codex("").state, AgentState::Unknown);
    }

    #[test]
    fn codex_resolves_via_fd_symlink_only() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir
            .path()
            .join(".codex/sessions/2026/09/07/rollout-x.jsonl");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(
            &log,
            r#"{"timestamp":"t1","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
        )
        .unwrap();
        let fd = dir.path().join("31415/fd");
        std::fs::create_dir_all(&fd).unwrap();
        std::os::unix::fs::symlink(&log, fd.join("3")).unwrap();
        assert_eq!(
            read_codex_observation(dir.path(), 31415).state,
            AgentState::WaitingInput
        );
        // 没有 fd 链接 → Unknown（不做最新文件兜底：codex 目录与进程对不上号）
        assert_eq!(
            read_codex_observation(dir.path(), 999).state,
            AgentState::Unknown
        );
    }
}
