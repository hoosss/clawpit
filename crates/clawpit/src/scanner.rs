//! /proc 扫描（观察站 v0）：从进程 argv[0] 识别 agent CLI。
//!
//! 刻意保持保守：只认 basename 精确命中（claude/codex/gemini/aider/opencode），
//! node/python 宿主进程不认——M3 再引入 transcript/cmdline 深度识别。

use clawpit_scene::Provider;

pub struct ProcHit {
    pub pid: u32,
    pub provider: Provider,
    /// 命中自定义名单（CLAWPIT_AGENTS）时的可执行名，供显示名用（如 zcode → zc-<pid>）。
    pub exe: Option<String>,
}

/// 扫描 /proc。`extra` 是自定义 agent 可执行名名单（CLAWPIT_AGENTS）：
/// 命中的按 Generic provider 收编——zcode / qwen-code 这类长尾 CLI 零改码接入。
pub fn scan(proc_root: &std::path::Path, extra: &[String]) -> Vec<ProcHit> {
    let mut hits = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return hits;
    };
    for entry in entries.flatten() {
        // 只看纯数字目录（进程 pid）
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        // cmdline 以 \0 分隔，第一段是 argv[0]（可执行路径）
        let Some(argv0) = cmdline.split(|&b| b == 0).next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let argv0 = String::from_utf8_lossy(argv0);
        let argv0_base = argv0.rsplit('/').next().unwrap_or(&argv0);
        if let Some(provider) = Provider::detect(&argv0) {
            hits.push(ProcHit {
                pid,
                provider,
                exe: None,
            });
        } else if extra.iter().any(|e| e == argv0_base) {
            hits.push(ProcHit {
                pid,
                provider: Provider::Generic,
                exe: Some(argv0_base.to_string()),
            });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc(root: &std::path::Path, pid: u32, argv0: &str) {
        let d = root.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("cmdline"), format!("{argv0}\0--flag\0").into_bytes()).unwrap();
    }

    #[test]
    fn finds_only_agent_processes() {
        let dir = tempfile::tempdir().unwrap();
        fake_proc(dir.path(), 1, "/usr/bin/sleep");
        fake_proc(dir.path(), 42, "/home/u/.nvm/versions/node/v20/bin/claude");
        fake_proc(dir.path(), 100, "codex");
        fake_proc(dir.path(), 101, "/usr/bin/python3");
        // 非数字目录应被忽略
        std::fs::create_dir_all(dir.path().join("irq")).unwrap();

        let hits = scan(dir.path(), &[]);
        // read_dir 顺序不保证，排序后再断言（扫描器本身无序，注册表快照时才排序）
        let mut pids: Vec<u32> = hits.iter().map(|h| h.pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, vec![42, 100]);
        let provider_of = |pid: u32| hits.iter().find(|h| h.pid == pid).unwrap().provider;
        assert_eq!(provider_of(42), Provider::ClaudeCode);
        assert_eq!(provider_of(100), Provider::Codex);
    }

    #[test]
    fn custom_agent_list_recruits_unknown_clis() {
        let dir = tempfile::tempdir().unwrap();
        fake_proc(dir.path(), 7, "/usr/local/bin/zcode");
        fake_proc(dir.path(), 8, "/usr/bin/sleep");

        let hits = scan(dir.path(), &["zcode".to_string(), "qwen-code".to_string()]);
        assert_eq!(hits.len(), 1, "只有名单内的 CLI 被收编");
        assert_eq!(hits[0].provider, Provider::Generic);
        assert_eq!(hits[0].exe.as_deref(), Some("zcode"));
        // 名单为空时行为不变：不收编未知 CLI
        assert!(scan(dir.path(), &[]).is_empty());
    }
}
