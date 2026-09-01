//! 集成冒烟：真实 hub（随机端口 + 假 /proc）+ 真 WS 客户端，
//! 验证 连接即快照 → 进程出现广播 upsert → 进程消失广播 gone 全链路。

use std::path::Path;

use clawpit::{router, Hub};
use clawpit_scene::{Provider, SceneEvent};
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;

fn fake_proc(root: &Path, pid: u32, argv0: &str) {
    let d = root.join(pid.to_string());
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("cmdline"), format!("{argv0}\0-v\0").into_bytes()).unwrap();
}

async fn next_scene_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> SceneEvent {
    let msg = ws
        .next()
        .await
        .expect("ws 流不应结束")
        .expect("ws 不应出错");
    let text = msg.into_text().expect("场景事件必须是文本帧");
    serde_json::from_str(&text).expect("事件应能反序列化为 SceneEvent")
}

#[tokio::test]
async fn snapshot_upsert_gone_over_ws() -> anyhow::Result<()> {
    let proc_root = tempfile::tempdir()?;
    fake_proc(proc_root.path(), 1, "/usr/bin/sleep");
    fake_proc(proc_root.path(), 42, "/home/u/.nvm/bin/claude");

    let hub = Hub::new();
    // 第一轮扫描：两个进程，只有 claude 命中
    let found = clawpit::scanner::scan(proc_root.path());
    hub.state.registry.write().await.apply_discovered(found);

    // 起 hub（随机端口）
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (mut ws, _) = connect_async(format!("ws://{addr}/scene")).await?;

    // 1) 连接即快照：只有 cc-42，sleep 不算 agent
    let ev = next_scene_event(&mut ws).await;
    let SceneEvent::Snapshot { agents } = ev else {
        panic!("首条必须是 Snapshot，实际 {ev:?}");
    };
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].id, "cc-42");
    assert_eq!(agents[0].provider, Provider::ClaudeCode);

    // 2) 新 agent 进程出现 → 广播 upsert
    fake_proc(proc_root.path(), 100, "codex");
    let found = clawpit::scanner::scan(proc_root.path());
    let events = hub.state.registry.write().await.apply_discovered(found);
    assert_eq!(events.len(), 1, "只应有一条 upsert");
    for ev in events {
        let _ = hub.state.tx.send(ev);
    }
    let ev = next_scene_event(&mut ws).await;
    assert!(
        matches!(&ev, SceneEvent::AgentUpsert { agent } if agent.id == "cx-100"),
        "应广播 cx-100 的 upsert，实际 {ev:?}"
    );

    // 3) 进程消失 → 广播 gone
    std::fs::remove_dir_all(proc_root.path().join("100"))?;
    let found = clawpit::scanner::scan(proc_root.path());
    let events = hub.state.registry.write().await.apply_discovered(found);
    assert_eq!(events.len(), 1, "只应有一条 gone");
    for ev in events {
        let _ = hub.state.tx.send(ev);
    }
    let ev = next_scene_event(&mut ws).await;
    assert!(
        matches!(&ev, SceneEvent::AgentGone { id } if id == "cx-100"),
        "应广播 cx-100 的 gone，实际 {ev:?}"
    );

    Ok(())
}

/// Lagged 自愈：客户端停止读取、广播通道（容量 64）被打爆后，
/// 服务端 send_task 应检测 RecvError::Lagged 并补发全量 Snapshot，
/// 客户端恢复读取时必须能收到第二份 Snapshot（非首帧位置）。
#[tokio::test]
async fn lagged_client_gets_snapshot_resync() -> anyhow::Result<()> {
    let hub = Hub::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (mut ws, _) = connect_async(format!("ws://{addr}/scene")).await?;

    // 洪水：3000 条 chat（to=human 不入收件箱，纯广播），客户端故意不读 →
    // TCP 缓冲吃满后 send_task 的 rx 必然 Lagged
    for i in 0..3000 {
        hub.mail
            .send(None, "human", &format!("洪水第 {i} 波"))
            .await?;
    }
    // 给服务端一点时间打满背压
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // 恢复读取：10s 内必须出现第二份 Snapshot（首帧是连接时的初始快照）
    let mut snapshots = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && snapshots < 2 {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), next_scene_event(&mut ws))
            .await?;
        if matches!(ev, SceneEvent::Snapshot { .. }) {
            snapshots += 1;
        }
    }
    assert!(
        snapshots >= 2,
        "Lagged 后应补发快照，实际收到 {snapshots} 份"
    );
    Ok(())
}
