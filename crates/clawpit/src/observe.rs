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
    let Some(path) = resolve_transcript(proc_root, pid, claude_home) else {
        return AgentState::Unknown;
    };
    let Ok(mut f) = std::fs::File::open(&path) else {
        return AgentState::Unknown;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(8192);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return AgentState::Unknown;
    }
    let mut buf = String::new();
    if f.take(8192).read_to_string(&mut buf).is_err() {
        return AgentState::Unknown;
    }
    parse_state(&buf)
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
}
