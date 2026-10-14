//! 端到端校验核心链路：大厅摘要 → 加入 → 群发 → 退出。
//!
//! 房间只有「在线人数」这一个计数：连接断开即离开。

use std::time::Duration;

use chat_client::Client;
use chat_proto::{DeviceId, ErrorCode, ServerEvent};
use chat_server::Server;

async fn wait_for<F>(client: &Client, pred: F) -> ServerEvent
where
    F: Fn(&ServerEvent) -> bool,
{
    for _ in 0..32 {
        let ev = tokio::time::timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("等待事件超时")
            .expect("连接已关闭");
        if pred(&ev) {
            return ev;
        }
    }
    panic!("未收到期望事件");
}

#[tokio::test]
async fn lobby_join_send_leave_flow() {
    let server = Server::new("局域网聊天室");
    let room = server.room();

    let a = Client::new(server.connect(DeviceId::new()).await);
    let b = Client::new(server.connect(DeviceId::new()).await);

    // 未加入时只能看到摘要
    a.lobby(room).await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Lobby { .. })).await {
        ServerEvent::Lobby { info } => {
            assert_eq!(info.name, "局域网聊天室");
            assert_eq!(info.online_count, 0);
        }
        _ => unreachable!(),
    }

    // A 加入
    a.join(room, "Alice").await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined {
            you: _,
            online_count,
            latest_seq,
            ..
        } => {
            assert_eq!(online_count, 1);
            assert_eq!(latest_seq, 0);
        }
        _ => unreachable!(),
    }

    // B 未加入即可看到人数
    b.lobby(room).await.unwrap();
    match wait_for(&b, |e| matches!(e, ServerEvent::Lobby { .. })).await {
        ServerEvent::Lobby { info } => assert_eq!(info.online_count, 1),
        _ => unreachable!(),
    }

    // B 加入
    b.join(room, "Bob").await.unwrap();
    match wait_for(&b, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined { online_count, .. } => assert_eq!(online_count, 2),
        _ => unreachable!(),
    }

    // A 发言，双方都应收到
    a.send_text(room, "大家好").await.unwrap();
    match wait_for(&b, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => {
            assert_eq!(message.text, "大家好");
            assert_eq!(message.sender_name, "Alice");
            assert_eq!(message.seq, 1);
        }
        _ => unreachable!(),
    }
    match wait_for(&a, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => assert_eq!(message.seq, 1),
        _ => unreachable!(),
    }

    // B 退出后，A 看到人数下降
    b.leave(room).await.unwrap();
    wait_for(&b, |e| matches!(e, ServerEvent::Left { .. })).await;
    match wait_for(&a, |e| matches!(e, ServerEvent::Presence { .. })).await {
        ServerEvent::Presence { online_count, .. } => assert_eq!(online_count, 1),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn non_member_never_receives_messages() {
    let server = Server::new("room");
    let room = server.room();
    let a = Client::new(server.connect(DeviceId::new()).await);
    let b = Client::new(server.connect(DeviceId::new()).await);

    a.join(room, "Alice").await.unwrap();
    wait_for(&a, |e| matches!(e, ServerEvent::Joined { .. })).await;

    a.send_text(room, "秘密").await.unwrap();
    wait_for(&a, |e| matches!(e, ServerEvent::Message { .. })).await;

    b.lobby(room).await.unwrap();
    match wait_for(&b, |e| matches!(e, ServerEvent::Lobby { .. })).await {
        ServerEvent::Lobby { info } => assert_eq!(info.online_count, 1),
        _ => unreachable!(),
    }

    // 之后最多只会推人数摘要，消息内容一条都不该出现
    let leaked = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            if let Ok(ServerEvent::Message { .. }) = b.next_event().await {
                return;
            }
        }
    })
    .await;
    assert!(leaked.is_err(), "未加入者不应收到消息内容");
}

#[tokio::test]
async fn cannot_send_before_join() {
    let server = Server::new("room");
    let room = server.room();
    let a = Client::new(server.connect(DeviceId::new()).await);

    a.send_text(room, "hi").await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::NotJoined),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn reconnect_does_not_inflate_online() {
    let server = Server::new("room");
    let room = server.room();

    let first = Client::new(server.connect(DeviceId::new()).await);
    first.join(room, "Alice").await.unwrap();
    match wait_for(&first, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined { online_count, .. } => assert_eq!(online_count, 1),
        _ => unreachable!(),
    }

    // 断开即离开：不点退出也不会有残留，下次回来仍是 1
    first.close().await.unwrap();
    assert_eq!(server.online_count().await, 0);

    let second = Client::new(server.connect(DeviceId::new()).await);
    second.join(room, "Alice").await.unwrap();
    match wait_for(&second, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined { online_count, .. } => assert_eq!(online_count, 1),
        _ => unreachable!(),
    }
    assert_eq!(server.online_count().await, 1);
}

#[tokio::test]
async fn same_device_can_hold_multiple_connections() {
    let server = Server::new("room");
    let room = server.room();
    let device = DeviceId::new();

    let first = Client::new(server.connect(device).await);
    let second = Client::new(server.connect(device).await);

    first.join(room, "Alice").await.unwrap();
    wait_for(&first, |e| matches!(e, ServerEvent::Joined { .. })).await;

    // 同一设备再开一条连接，不应把前一条挤掉
    second.join(room, "Bob").await.unwrap();
    match wait_for(&second, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined { online_count, .. } => assert_eq!(online_count, 2),
        _ => unreachable!(),
    }

    first.send_text(room, "第一条连接仍可用").await.unwrap();
    match wait_for(&second, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => assert_eq!(message.sender_name, "Alice"),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn lobby_watcher_sees_count_changes() {
    let server = Server::new("room");
    let room = server.room();

    // 观望者只连不加入
    let watcher = Client::new(server.connect(DeviceId::new()).await);
    watcher.lobby(room).await.unwrap();
    wait_for(&watcher, |e| matches!(e, ServerEvent::Lobby { .. })).await;

    // 有人加入时，观望者的人数摘要也要跟着变——否则界面会一直停在 0
    let joiner = Client::new(server.connect(DeviceId::new()).await);
    joiner.join(room, "Alice").await.unwrap();

    match wait_for(&watcher, |e| {
        matches!(e, ServerEvent::Lobby { info } if info.online_count == 1)
    })
    .await
    {
        ServerEvent::Lobby { info } => assert_eq!(info.online_count, 1),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn leaver_sees_updated_counts() {
    let server = Server::new("room");
    let room = server.room();

    let alice = Client::new(server.connect(DeviceId::new()).await);
    alice.join(room, "Alice").await.unwrap();
    wait_for(&alice, |e| matches!(e, ServerEvent::Joined { .. })).await;

    let bob = Client::new(server.connect(DeviceId::new()).await);
    bob.join(room, "Bob").await.unwrap();
    wait_for(&bob, |e| matches!(e, ServerEvent::Joined { .. })).await;

    bob.leave(room).await.unwrap();
    wait_for(&bob, |e| matches!(e, ServerEvent::Left { .. })).await;

    // 退出后不应卡在 2，要立刻拿到 1
    match wait_for(&bob, |e| {
        matches!(e, ServerEvent::Lobby { info } if info.online_count == 1)
    })
    .await
    {
        ServerEvent::Lobby { info } => assert_eq!(info.online_count, 1),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn history_returns_recent_messages() {
    let server = Server::new("room");
    let room = server.room();
    let a = Client::new(server.connect(DeviceId::new()).await);

    a.join(room, "Alice").await.unwrap();
    wait_for(&a, |e| matches!(e, ServerEvent::Joined { .. })).await;

    for i in 1..=5 {
        a.send_text(room, format!("m{i}")).await.unwrap();
    }
    for _ in 1..=5 {
        wait_for(&a, |e| matches!(e, ServerEvent::Message { .. })).await;
    }

    a.history(room, None, 3).await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::History { .. })).await {
        ServerEvent::History {
            messages, has_more, ..
        } => {
            assert_eq!(messages.len(), 3);
            assert_eq!(messages[0].text, "m3");
            assert_eq!(messages[2].text, "m5");
            assert!(has_more);
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn sync_returns_messages_after_cursor() {
    let server = Server::new("room");
    let room = server.room();
    let a = Client::new(server.connect(DeviceId::new()).await);

    a.join(room, "Alice").await.unwrap();
    wait_for(&a, |e| matches!(e, ServerEvent::Joined { .. })).await;

    for i in 1..=5 {
        a.send_text(room, format!("m{i}")).await.unwrap();
    }
    for _ in 1..=5 {
        wait_for(&a, |e| matches!(e, ServerEvent::Message { .. })).await;
    }

    // 只补游标之后的部分
    a.sync(room, 2, 10).await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Sync { .. })).await {
        ServerEvent::Sync {
            messages, has_more, ..
        } => {
            assert_eq!(messages.len(), 3);
            assert_eq!(messages[0].text, "m3");
            assert_eq!(messages[2].text, "m5");
            assert!(!has_more, "已追到最新，不应再提示还有更多");
        }
        _ => unreachable!(),
    }

    // 缺口超过一页时靠 has_more 翻页
    a.sync(room, 0, 2).await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Sync { .. })).await {
        ServerEvent::Sync {
            messages, has_more, ..
        } => {
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].text, "m1");
            assert!(has_more);
        }
        _ => unreachable!(),
    }
}
