//! 集成冒烟：真实 hub（随机端口 + 假 /proc）+ 真 WS 客户端，
//! 验证 连接即快照 → 进程出现广播 upsert → 进程消失广播 gone 全链路。

use std::path::Path;

use futures_util::StreamExt;
use pf_hub::{router, Hub};
use pf_scene::{Provider, SceneEvent};
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
    let found = pf_hub::scanner::scan(proc_root.path());
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
    let found = pf_hub::scanner::scan(proc_root.path());
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
    let found = pf_hub::scanner::scan(proc_root.path());
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
