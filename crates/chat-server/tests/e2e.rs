//! 端到端校验 V1 核心链路：大厅摘要 → 加入 → 群发 → 退出。

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
            assert_eq!(info.member_count, 0);
        }
        _ => unreachable!(),
    }

    // A 加入
    a.join(room, "Alice").await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined {
            members,
            latest_seq,
            ..
        } => {
            assert_eq!(members.len(), 1);
            assert_eq!(members[0].name, "Alice");
            assert!(members[0].online);
            assert_eq!(latest_seq, 0);
        }
        _ => unreachable!(),
    }

    // B 未加入即可看到在线人数
    b.lobby(room).await.unwrap();
    match wait_for(&b, |e| matches!(e, ServerEvent::Lobby { .. })).await {
        ServerEvent::Lobby { info } => {
            assert_eq!(info.online_count, 1);
            assert_eq!(info.member_count, 1);
        }
        _ => unreachable!(),
    }

    // B 加入
    b.join(room, "Bob").await.unwrap();
    match wait_for(&b, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined { members, .. } => assert_eq!(members.len(), 2),
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
        ServerEvent::Presence {
            online_count,
            member_count,
            ..
        } => {
            assert_eq!(online_count, 1);
            assert_eq!(member_count, 1);
        }
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

    // 大厅摘要之后不应再有任何推送
    let extra = tokio::time::timeout(Duration::from_millis(150), b.next_event()).await;
    assert!(extra.is_err(), "未加入者不应收到任何消息");
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
async fn rejoining_with_same_name_reuses_member() {
    let server = Server::new("room");
    let room = server.room();

    let first = Client::new(server.connect(DeviceId::new()).await);
    first.join(room, "Alice").await.unwrap();
    wait_for(&first, |e| matches!(e, ServerEvent::Joined { .. })).await;
    assert_eq!(server.member_count().await, 1);
    assert_eq!(server.online_count().await, 1);

    // 断开连接：没点退出，成员应保留，只掉在线
    first.close().await.unwrap();
    assert_eq!(server.member_count().await, 1);
    assert_eq!(server.online_count().await, 0);

    // 同名重连（例如点了自己的房间）：必须复用同一成员而不是新增
    let second = Client::new(server.connect(DeviceId::new()).await);
    second.join(room, "Alice").await.unwrap();
    match wait_for(&second, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined { members, .. } => assert_eq!(members.len(), 1),
        _ => unreachable!(),
    }
    assert_eq!(server.member_count().await, 1);
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
    wait_for(&second, |e| matches!(e, ServerEvent::Joined { .. })).await;
    assert_eq!(server.member_count().await, 2);

    first.send_text(room, "第一条连接仍可用").await.unwrap();
    match wait_for(&second, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => assert_eq!(message.sender_name, "Alice"),
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
