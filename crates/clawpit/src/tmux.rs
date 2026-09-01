//! tmux 桥（M3）：给外部（非 hub 宿主）会话注入输入。
//!
//! 外部 agent 跑在 tmux pane 里时，`tmux send-keys` 是唯一可靠的注入通道。
//! 进程 → pane 的匹配：沿 /proc 父链上行，命中任一 pane 的 shell pid。

use std::path::Path;

/// pid 的父进程链（含自身，最多 16 层）。
pub fn ancestors(proc_root: &Path, pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut cur = pid;
    for _ in 0..16 {
        let Ok(status) = std::fs::read_to_string(proc_root.join(cur.to_string()).join("status"))
        else {
            break;
        };
        let Some(ppid) = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            break;
        };
        if ppid == 0 {
            break;
        }
        chain.push(ppid);
        cur = ppid;
    }
    chain
}

/// pid（或其任一祖先）若是某 tmux pane 的 shell → 返回 pane id（如 %3）。
pub fn pane_for_pid(proc_root: &Path, pid: u32) -> Option<String> {
    let out = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_pid} #{pane_id}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let chain = ancestors(proc_root, pid);
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pane_pid), Some(pane_id)) = (it.next(), it.next()) else {
            continue;
        };
        if let Ok(p) = pane_pid.parse::<u32>() {
            if chain.contains(&p) {
                return Some(pane_id.to_string());
            }
        }
    }
    None
}

/// pid 复用防线：注入前复核该 pid 的 argv[0] 仍是指定 provider 的 CLI。
/// 进程死了 pid 被系统复用成无关进程时，send-keys 会把字打进无辜终端——必须挡住。
pub fn pid_still_agent(proc_root: &Path, pid: u32, provider: clawpit_scene::Provider) -> bool {
    let Ok(cmdline) = std::fs::read(proc_root.join(pid.to_string()).join("cmdline")) else {
        return false;
    };
    let Some(argv0) = cmdline.split(|&b| b == 0).next().filter(|s| !s.is_empty()) else {
        return false;
    };
    clawpit_scene::Provider::detect(&String::from_utf8_lossy(argv0)) == Some(provider)
}

/// 向 pane 打字并回车（-l 逐字输入，防热键解释）。
pub fn send_text(pane: &str, text: &str) -> anyhow::Result<()> {
    let st = std::process::Command::new("tmux")
        .args(["send-keys", "-t", pane, "-l", "--", text])
        .status()?;
    anyhow::ensure!(st.success(), "tmux send-keys 失败");
    let st = std::process::Command::new("tmux")
        .args(["send-keys", "-t", pane, "Enter"])
        .status()?;
    anyhow::ensure!(st.success(), "tmux send Enter 失败");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_still_agent_checks_argv0() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |pid: u32, argv0: &str| {
            let d = dir.path().join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("cmdline"), format!("{argv0}\0-v\0").into_bytes()).unwrap();
        };
        mk(1, "/nvm/bin/claude");
        mk(2, "/usr/bin/vim"); // pid 被复用后的无辜进程
        assert!(pid_still_agent(
            dir.path(),
            1,
            clawpit_scene::Provider::ClaudeCode
        ));
        assert!(!pid_still_agent(
            dir.path(),
            2,
            clawpit_scene::Provider::ClaudeCode
        ));
        assert!(!pid_still_agent(
            dir.path(),
            3,
            clawpit_scene::Provider::ClaudeCode
        )); // 已消失
    }

    #[test]
    fn ancestors_walks_ppid_chain() {
        let dir = tempfile::tempdir().unwrap();
        // 伪造三代进程链：300 <- 200 <- 100
        for (pid, ppid) in [(100u32, 200u32), (200, 300), (300, 0)] {
            let d = dir.path().join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("status"), format!("Name: fake\nPPid: {ppid}\n")).unwrap();
        }
        assert_eq!(ancestors(dir.path(), 100), vec![100, 200, 300]);
        // 不存在的 pid → 只有自己
        assert_eq!(ancestors(dir.path(), 999), vec![999]);
    }

    /// tmux 不存在时静默跳过（CI/裸 WSL 环境）。
    #[test]
    fn pane_lookup_and_send_when_tmux_exists() {
        if which("tmux").is_none() {
            eprintln!("(tmux 未安装，跳过)");
            return;
        }
        // 在 tmux 里跑个 cat，验证注入链路
        let ok = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", "clawpit-test", "cat"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "tmux new-session 失败");
        // 等 cat 起来，拿到 pane 的 shell pid 树
        std::thread::sleep(std::time::Duration::from_millis(300));
        let out = std::process::Command::new("tmux")
            .args([
                "list-panes",
                "-t",
                "clawpit-test",
                "-F",
                "#{pane_pid} #{pane_id}",
            ])
            .output()
            .unwrap();
        let line = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap()
            .to_string();
        let (shell_pid, pane) = line.split_once(' ').unwrap();
        let shell_pid: u32 = shell_pid.parse().unwrap();
        // cat 是 shell 的子进程：用 shell_pid 自己也该命中
        assert_eq!(
            pane_for_pid(Path::new("/proc"), shell_pid).as_deref(),
            Some(pane)
        );
        send_text(pane, "hello clawpit").unwrap();
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", "clawpit-test"])
            .status();
    }

    fn which(cmd: &str) -> Option<std::path::PathBuf> {
        let paths = std::env::var_os("PATH")?;
        std::env::split_paths(&paths)
            .map(|p| p.join(cmd))
            .find(|p| p.exists())
    }
}
