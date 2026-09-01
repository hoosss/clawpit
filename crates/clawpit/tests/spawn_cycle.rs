//! M1 宿舍集成测试：spawn → 状态映射 → say → stop 全生命周期。

use std::time::Duration;

use clawpit::{
    spawn::{provider_command, SpawnRequest},
    Hub,
};
use clawpit_scene::{AgentState, Provider, Source};

#[tokio::test]
async fn spawn_exit_state_then_stop_clears() -> anyhow::Result<()> {
    let hub = Hub::new();

    // 招一只立刻以码 3 退出的 worker
    let agent = hub
        .spawn
        .spawn(SpawnRequest {
            provider: Provider::Generic,
            cwd: None,
            argv: Some(vec!["sh".into(), "-c".into(), "exit 3".into()]),
        })
        .await?;
    assert_eq!(agent.state, AgentState::Working);
    assert!(matches!(agent.source, Source::Spawned { pid: p } if p > 0));

    // 退出监听是 200ms 轮询，最多等 10s
    let mut became_error = false;
    for _ in 0..50 {
        let agents = hub.state.registry.read().await.snapshot();
        if agents
            .iter()
            .any(|a| a.id == agent.id && a.state == AgentState::Error)
        {
            became_error = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(became_error, "退出码非 0 应映射为 Error 并广播");
    // 退出后条目保留（能看到结果），直到显式 stop
    assert!(hub.state.registry.read().await.get(&agent.id).is_some());

    hub.spawn.stop(&agent.id).await?;
    assert!(hub.state.registry.read().await.get(&agent.id).is_none());
    Ok(())
}

#[tokio::test]
async fn say_writes_stdin_and_stop_kills() -> anyhow::Result<()> {
    let hub = Hub::new();

    // cat：stdin 行原样回显，用来验证写入通道畅通
    let agent = hub
        .spawn
        .spawn(SpawnRequest {
            provider: Provider::Generic,
            cwd: None,
            argv: Some(vec!["cat".into()]),
        })
        .await?;
    assert_eq!(agent.state, AgentState::Working);

    hub.spawn.say(&agent.id, "hello clawpit")?;

    // 对不存在的 worker 喊话应报错
    assert!(hub.spawn.say("sp-99999", "x").is_err());

    hub.spawn.stop(&agent.id).await?;
    assert!(hub.state.registry.read().await.get(&agent.id).is_none());
    Ok(())
}

#[tokio::test]
async fn exit_zero_maps_to_done() -> anyhow::Result<()> {
    let hub = Hub::new();
    let agent = hub
        .spawn
        .spawn(SpawnRequest {
            provider: Provider::Generic,
            cwd: None,
            argv: Some(vec!["true".into()]),
        })
        .await?;
    let mut became_done = false;
    for _ in 0..50 {
        if hub
            .state
            .registry
            .read()
            .await
            .get(&agent.id)
            .is_some_and(|a| a.state == AgentState::Done)
        {
            became_done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(became_done, "退出码 0 应映射为 Done");
    Ok(())
}

#[test]
fn provider_default_commands() {
    assert_eq!(provider_command(Provider::ClaudeCode), "claude");
    assert_eq!(provider_command(Provider::Codex), "codex");
}
