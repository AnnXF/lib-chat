//! 端到端校验核心链路：大厅摘要 → 加入 → 群发 → 退出。
//!
//! 房间只有「在线人数」这一个计数：连接断开即离开。

use std::time::Duration;

use chat_core::Client;
use chat_core::{
    guess_mime, sha256_hex, Attachment, AttachmentId, ChatError, ClientRequest, Connection,
    DeviceId, ErrorCode, Message, MessageKind, MessageStore, NewMessage, RoomId, Seq, ServerEvent,
    EVENT_QUEUE_CAPACITY,
};
use chat_server::Server;
use std::sync::atomic::{AtomicBool, Ordering};

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

    match wait_for(
        &watcher,
        |e| matches!(e, ServerEvent::Lobby { info } if info.online_count == 1),
    )
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
    match wait_for(
        &bob,
        |e| matches!(e, ServerEvent::Lobby { info } if info.online_count == 1),
    )
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
    a.history(room, Some(3), 2).await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::History { .. })).await {
        ServerEvent::History {
            messages, has_more, ..
        } => {
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].text, "m1");
            assert!(!has_more, "恰好一页时已经没有更早历史");
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

    a.sync(room, 3, 2).await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Sync { .. })).await {
        ServerEvent::Sync {
            messages, has_more, ..
        } => {
            assert_eq!(messages.len(), 2);
            assert!(!has_more, "恰好一页时已追到末尾");
        }
        _ => unreachable!(),
    }
}

struct FailingStore {
    fail_latest: AtomicBool,
}

#[async_trait::async_trait]
impl MessageStore for FailingStore {
    async fn append(&self, _: RoomId, _: NewMessage) -> chat_core::Result<chat_core::AppendResult> {
        Err(ChatError::Storage("写入失败".into()))
    }

    async fn history(
        &self,
        _: RoomId,
        _: Option<Seq>,
        _: usize,
    ) -> chat_core::Result<Vec<Message>> {
        Ok(Vec::new())
    }

    async fn since(&self, _: RoomId, _: Seq, _: usize) -> chat_core::Result<Vec<Message>> {
        Ok(Vec::new())
    }

    async fn latest_seq(&self, _: RoomId) -> chat_core::Result<Seq> {
        if self.fail_latest.swap(false, Ordering::SeqCst) {
            Err(ChatError::Storage("查询失败".into()))
        } else {
            Ok(0)
        }
    }

    async fn attachment(
        &self,
        _: RoomId,
        _: chat_core::AttachmentId,
    ) -> chat_core::Result<Option<chat_core::Attachment>> {
        Ok(None)
    }
}

#[tokio::test]
async fn failed_join_does_not_leave_a_hidden_member() {
    let server = Server::with_store(
        "room",
        std::sync::Arc::new(FailingStore {
            fail_latest: AtomicBool::new(true),
        }),
    );
    let room = server.room();
    let a = Client::new(server.connect(DeviceId::new()).await);

    a.join(room, "Alice").await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::Internal),
        _ => unreachable!(),
    }
    assert_eq!(server.online_count().await, 0);

    a.join(room, "Alice").await.unwrap();
    wait_for(&a, |e| matches!(e, ServerEvent::Joined { .. })).await;
    assert_eq!(server.online_count().await, 1);

    a.send_text(room, "消息").await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, message } => {
            assert_eq!(code, ErrorCode::Internal);
            assert!(message.contains("保存失败"));
        }
        _ => unreachable!(),
    }
    assert_eq!(server.online_count().await, 1);
}

#[tokio::test]
async fn oversized_text_is_rejected_without_dropping_the_connection() {
    let server = Server::new("room");
    let room = server.room();
    let a = Client::new(server.connect(DeviceId::new()).await);
    a.join(room, "Alice").await.unwrap();
    wait_for(&a, |e| matches!(e, ServerEvent::Joined { .. })).await;
    a.send_text(room, "中".repeat(3000)).await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::InvalidRequest),
        _ => unreachable!(),
    }
    a.send_text(room, "小消息").await.unwrap();
    wait_for(
        &a,
        |e| matches!(e, ServerEvent::Message { message } if message.text == "小消息"),
    )
    .await;
}

#[tokio::test]
async fn duplicate_message_replies_to_sender_without_broadcast_or_new_seq() {
    let server = Server::new("room");
    let room = server.room();
    let sender = server.connect(DeviceId::new()).await;
    let sender_client = Client::new(sender.clone());
    let observer = Client::new(server.connect(DeviceId::new()).await);
    sender_client.join(room, "Alice").await.unwrap();
    wait_for(&sender_client, |e| matches!(e, ServerEvent::Joined { .. })).await;
    observer.join(room, "Bob").await.unwrap();
    wait_for(&observer, |e| matches!(e, ServerEvent::Joined { .. })).await;

    let id = uuid::Uuid::new_v4();
    sender_client
        .send_text_with_id(room, id, "一次")
        .await
        .unwrap();
    wait_for(&sender_client, |e| matches!(e, ServerEvent::Message { .. })).await;
    wait_for(&observer, |e| matches!(e, ServerEvent::Message { .. })).await;
    sender_client
        .send_text_with_id(room, id, "一次")
        .await
        .unwrap();
    match wait_for(&sender_client, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => assert_eq!(message.seq, 1),
        _ => unreachable!(),
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), observer.next_event())
            .await
            .is_err()
    );
    sender
        .request(ClientRequest::Send {
            room,
            client_msg_id: id,
            text: "篡改".into(),
        })
        .await
        .unwrap();
    match wait_for(&sender_client, |e| matches!(e, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::InvalidRequest),
        _ => unreachable!(),
    }
    sender_client.history(room, None, 10).await.unwrap();
    match wait_for(&sender_client, |e| matches!(e, ServerEvent::History { .. })).await {
        ServerEvent::History { messages, .. } => assert_eq!(messages.len(), 1),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn slow_member_is_disconnected_without_blocking_other_members() {
    use chat_core::RequestSink;
    let server = Server::new("room");
    let room = server.room();
    let (slow, mut events) = server.attach(DeviceId::new()).await;
    slow.dispatch(ClientRequest::Join {
        room,
        profile: chat_core::Profile {
            name: "slow".into(),
        },
    })
    .await
    .unwrap();
    let fast = Client::new(server.connect(DeviceId::new()).await);
    fast.join(room, "fast").await.unwrap();
    wait_for(&fast, |e| matches!(e, ServerEvent::Joined { .. })).await;
    for i in 0..EVENT_QUEUE_CAPACITY {
        fast.send_text(room, format!("m{i}")).await.unwrap();
        wait_for(&fast, |e| matches!(e, ServerEvent::Message { .. })).await;
    }
    assert_eq!(server.online_count().await, 1);
    assert!(matches!(
        slow.dispatch(ClientRequest::Lobby { room }).await,
        Err(ChatError::Closed)
    ));
    for _ in 0..EVENT_QUEUE_CAPACITY + 2 {
        if events.recv().await.is_none() {
            break;
        }
    }
    assert!(events.recv().await.is_none());
    fast.send_text(room, "still online").await.unwrap();
    wait_for(
        &fast,
        |e| matches!(e, ServerEvent::Message { message } if message.text == "still online"),
    )
    .await;
}

fn attachment_of(name: &str, data: &[u8]) -> Attachment {
    Attachment {
        id: AttachmentId::new(),
        name: name.into(),
        size: data.len() as u64,
        mime: guess_mime(name).into(),
        sha256: sha256_hex(data),
    }
}

#[tokio::test]
async fn file_message_carries_attachment_and_can_be_downloaded() {
    let server = Server::new("room");
    let room = server.room();
    let sender = Client::new(server.connect(DeviceId::new()).await);
    let receiver = Client::new(server.connect(DeviceId::new()).await);
    for (client, name) in [(&sender, "Alice"), (&receiver, "Bob")] {
        client.join(room, name).await.unwrap();
        wait_for(client, |e| matches!(e, ServerEvent::Joined { .. })).await;
    }

    let data = b"hello, file".to_vec();
    let attachment = attachment_of("hello.txt", &data);
    sender
        .upload(room, attachment.clone(), data.clone())
        .await
        .unwrap();
    sender
        .send_file(room, uuid::Uuid::new_v4(), attachment.clone())
        .await
        .unwrap();

    match wait_for(&receiver, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => {
            assert_eq!(message.kind, MessageKind::File);
            assert_eq!(message.text, "");
            let received = message.attachment.expect("文件消息必须带附件");
            assert_eq!(received.id, attachment.id);
            assert_eq!(received.name, "hello.txt");
            assert_eq!(received.size, data.len() as u64);
            assert_eq!(received.mime, "text/plain");
        }
        _ => unreachable!(),
    }

    assert_eq!(receiver.download(room, attachment.id).await.unwrap(), data);

    receiver.history(room, None, 10).await.unwrap();
    match wait_for(&receiver, |e| matches!(e, ServerEvent::History { .. })).await {
        ServerEvent::History { messages, .. } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(
                messages[0].attachment.as_ref().map(|a| a.sha256.as_str()),
                Some(attachment.sha256.as_str())
            );
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn upload_requires_membership_and_matching_content() {
    let server = Server::new("room");
    let room = server.room();
    let member = Client::new(server.connect(DeviceId::new()).await);
    let outsider = Client::new(server.connect(DeviceId::new()).await);
    member.join(room, "Alice").await.unwrap();
    wait_for(&member, |e| matches!(e, ServerEvent::Joined { .. })).await;

    let data = b"payload".to_vec();
    let attachment = attachment_of("a.bin", &data);

    // 未加入的连接不能上传，也不能下载
    assert!(outsider
        .upload(room, attachment.clone(), data.clone())
        .await
        .is_err());
    assert!(outsider.download(room, attachment.id).await.is_err());

    // 摘要不一致必须拒绝
    let mut wrong_hash = attachment.clone();
    wrong_hash.sha256 = sha256_hex(b"other");
    assert!(member.upload(room, wrong_hash, data.clone()).await.is_err());

    // 大小不一致必须拒绝
    let mut wrong_size = attachment.clone();
    wrong_size.size += 1;
    assert!(member.upload(room, wrong_size, data.clone()).await.is_err());

    // 没有上传就发文件消息，必须报 NotFound
    member
        .send_file(room, uuid::Uuid::new_v4(), attachment.clone())
        .await
        .unwrap();
    match wait_for(&member, |e| matches!(e, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::NotFound),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn duplicate_file_message_is_stored_once() {
    let server = Server::new("room");
    let room = server.room();
    let client = Client::new(server.connect(DeviceId::new()).await);
    client.join(room, "Alice").await.unwrap();
    wait_for(&client, |e| matches!(e, ServerEvent::Joined { .. })).await;

    let data = b"same".to_vec();
    let attachment = attachment_of("same.txt", &data);
    client
        .upload(room, attachment.clone(), data.clone())
        .await
        .unwrap();
    let id = uuid::Uuid::new_v4();
    client
        .send_file(room, id, attachment.clone())
        .await
        .unwrap();
    wait_for(&client, |e| matches!(e, ServerEvent::Message { .. })).await;
    client.send_file(room, id, attachment).await.unwrap();
    match wait_for(&client, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => assert_eq!(message.seq, 1),
        _ => unreachable!(),
    }

    client.history(room, None, 10).await.unwrap();
    match wait_for(&client, |e| matches!(e, ServerEvent::History { .. })).await {
        ServerEvent::History { messages, .. } => assert_eq!(messages.len(), 1),
        _ => unreachable!(),
    }
}
