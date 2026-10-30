//! Hub：账号、好友、单聊/群聊与按房间隔离的消息。

use std::time::Duration;

use chat_core::{Client, DeviceId, ErrorCode, RoomKind, ServerEvent, UserSummary};
use chat_server::Hub;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("libchat-hub-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn wait_for<F>(client: &Client, pred: F) -> ServerEvent
where
    F: Fn(&ServerEvent) -> bool,
{
    for _ in 0..64 {
        let event = tokio::time::timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("等待事件超时")
            .expect("连接已关闭");
        if pred(&event) {
            return event;
        }
    }
    panic!("未收到期望事件");
}

async fn register(hub: &std::sync::Arc<Hub>, name: &str) -> (Client, UserSummary) {
    let client = Client::new(hub.connect(DeviceId::new()).await);
    client.register(name, "password123", name).await.unwrap();
    match wait_for(&client, |event| {
        matches!(event, ServerEvent::Authenticated { .. })
    })
    .await
    {
        ServerEvent::Authenticated { user, .. } => (client, user),
        _ => unreachable!(),
    }
}

async fn rooms(client: &Client) -> Vec<chat_core::RoomSummary> {
    client.list_rooms().await.unwrap();
    match wait_for(client, |event| matches!(event, ServerEvent::Rooms { .. })).await {
        ServerEvent::Rooms { rooms } => rooms,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn accounts_friends_direct_chat_and_messages() {
    let dir = temp_dir("direct");
    let hub = Hub::open(&dir, "大厅").await.unwrap();

    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    alice.add_friend("bob").await.unwrap();
    match wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await {
        ServerEvent::Friends { users } => {
            assert_eq!(
                users.iter().map(|u| &u.username).collect::<Vec<_>>(),
                ["bob"]
            );
        }
        _ => unreachable!(),
    }

    alice.open_direct(bob_user.id).await.unwrap();
    let direct = rooms(&alice)
        .await
        .into_iter()
        .find(|room| room.kind == RoomKind::Direct)
        .expect("应出现单聊");
    assert_eq!(direct.member_count, 2);

    alice.join(direct.room, "alice").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Joined { .. })).await;
    bob.join(direct.room, "bob").await.unwrap();
    wait_for(&bob, |event| matches!(event, ServerEvent::Joined { .. })).await;

    alice.send_text(direct.room, "单聊消息").await.unwrap();
    match wait_for(
        &bob,
        |event| matches!(event, ServerEvent::Message { message } if message.text == "单聊消息"),
    )
    .await
    {
        ServerEvent::Message { message } => {
            assert_eq!(message.sender, alice_user.id);
            assert_eq!(message.sender_name, "alice");
        }
        _ => unreachable!(),
    }

    bob.history(direct.room, None, 10).await.unwrap();
    match wait_for(&bob, |event| matches!(event, ServerEvent::History { .. })).await {
        ServerEvent::History { messages, .. } => assert_eq!(messages.len(), 1),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn groups_only_include_friends_and_reject_outsiders() {
    let dir = temp_dir("group");
    let hub = Hub::open(&dir, "大厅").await.unwrap();

    let (alice, _) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;
    let (carol, _) = register(&hub, "carol").await;
    alice.add_friend("bob").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;

    alice
        .create_group("项目群", vec![bob_user.id])
        .await
        .unwrap();
    let group = rooms(&alice)
        .await
        .into_iter()
        .find(|room| room.kind == RoomKind::Group && room.name == "项目群")
        .expect("应出现群聊");
    assert_eq!(group.member_count, 2);

    alice.join(group.room, "alice").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Joined { .. })).await;
    bob.join(group.room, "bob").await.unwrap();
    wait_for(&bob, |event| matches!(event, ServerEvent::Joined { .. })).await;

    alice.send_text(group.room, "群消息").await.unwrap();
    wait_for(
        &bob,
        |event| matches!(event, ServerEvent::Message { message } if message.text == "群消息"),
    )
    .await;

    // 非成员既不能加入，也收不到消息
    carol.join(group.room, "carol").await.unwrap();
    match wait_for(&carol, |event| matches!(event, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::RoomMismatch),
        _ => unreachable!(),
    }
    match tokio::time::timeout(Duration::from_millis(100), carol.next_event()).await {
        Err(_) => {}
        Ok(Ok(ServerEvent::Message { .. })) => panic!("非成员不应收到群消息"),
        _ => {}
    }
}

#[tokio::test]
async fn unauthenticated_connections_are_asked_to_login() {
    let dir = temp_dir("auth");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let anon = Client::new(hub.connect(DeviceId::new()).await);
    anon.list_rooms().await.unwrap();
    match wait_for(&anon, |event| matches!(event, ServerEvent::AuthRequired)).await {
        ServerEvent::AuthRequired => {}
        _ => unreachable!(),
    }

    // 登录后默认房间自动可见
    let (alice, _) = register(&hub, "alice").await;
    let rooms = rooms(&alice).await;
    assert!(rooms.iter().any(|room| room.room == hub.default_room()));
}

#[tokio::test]
async fn room_members_are_visible_to_members() {
    let dir = temp_dir("members");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, _) = register(&hub, "alice").await;
    let (_bob, bob) = register(&hub, "bob").await;

    alice.room_members(hub.default_room()).await.unwrap();
    let members = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::RoomMembers { .. })
    })
    .await
    {
        ServerEvent::RoomMembers { members, .. } => members,
        _ => unreachable!(),
    };
    assert!(members.iter().any(|member| member.user.id == bob.id));
}

#[tokio::test]
async fn adding_a_friend_notifies_both_online_sides() {
    let dir = temp_dir("friend-notify");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, _) = register(&hub, "alice").await;
    let (bob, _) = register(&hub, "bob").await;

    alice.add_friend("bob").await.unwrap();
    match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Friends { users }
            if users.iter().any(|user| user.username == "bob"))
    })
    .await
    {
        ServerEvent::Friends { .. } => {}
        _ => unreachable!(),
    }

    // 被添加的一方在线时也该立刻刷新，不必重新登录
    match wait_for(&bob, |event| {
        matches!(event, ServerEvent::Friends { users }
            if users.iter().any(|user| user.username == "alice"))
    })
    .await
    {
        ServerEvent::Friends { .. } => {}
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn group_and_direct_creation_push_rooms_to_members() {
    let dir = temp_dir("rooms-push");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, _) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    alice.add_friend("bob").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;

    alice
        .create_group("项目群", vec![bob_user.id])
        .await
        .unwrap();
    wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.name == "项目群"))
    })
    .await;
    wait_for(&bob, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.name == "项目群"))
    })
    .await;

    alice.open_direct(bob_user.id).await.unwrap();
    wait_for(&bob, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.kind == RoomKind::Direct))
    })
    .await;
}

#[tokio::test]
async fn owner_can_remove_member_and_dissolve_group() {
    use chat_core::MemberRole;

    let dir = temp_dir("group-admin");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;
    let (carol, carol_user) = register(&hub, "carol").await;

    alice.add_friend("bob").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    alice.add_friend("carol").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;

    alice
        .create_group("管理群", vec![bob_user.id, carol_user.id])
        .await
        .unwrap();
    let group = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.name == "管理群"))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => {
            rooms
                .into_iter()
                .find(|room| room.name == "管理群")
                .unwrap()
                .room
        }
        _ => unreachable!(),
    };
    wait_for(&bob, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.room == group))
    })
    .await;
    wait_for(&carol, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.room == group))
    })
    .await;

    alice.room_members(group).await.unwrap();
    match wait_for(&alice, |event| {
        matches!(event, ServerEvent::RoomMembers { .. })
    })
    .await
    {
        ServerEvent::RoomMembers { members, .. } => {
            assert_eq!(members.len(), 3);
            assert!(members.iter().any(|member| {
                member.user.id == alice_user.id && member.role == MemberRole::Owner
            }));
        }
        _ => unreachable!(),
    }

    alice.remove_member(group, bob_user.id).await.unwrap();
    wait_for(&bob, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if !rooms.iter().any(|room| room.room == group))
    })
    .await;
    alice.dissolve_group(group).await.unwrap();
    wait_for(&carol, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if !rooms.iter().any(|room| room.room == group))
    })
    .await;
}

#[tokio::test]
async fn search_remove_friend_and_presence() {
    let dir = temp_dir("friend-search-remove");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, _) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    alice.search_users("bo").await.unwrap();
    match wait_for(&alice, |event| {
        matches!(event, ServerEvent::UserSearch { .. })
    })
    .await
    {
        ServerEvent::UserSearch { users } => {
            assert!(users.iter().any(|user| user.id == bob_user.id));
        }
        _ => unreachable!(),
    }

    alice.add_friend("bob").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;
    // 双方都能看到对方在线
    wait_for(&alice, |event| {
        matches!(event, ServerEvent::FriendPresence { user, online: true } if *user == bob_user.id)
    })
    .await;

    alice.remove_friend(bob_user.id).await.unwrap();
    match wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await {
        ServerEvent::Friends { users } => {
            assert!(!users.iter().any(|user| user.id == bob_user.id));
        }
        _ => unreachable!(),
    }

    // 重新加好友，bob 退出后 alice 应收到离线
    alice.add_friend("bob").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;
    bob.logout().await.unwrap();
    wait_for(&alice, |event| {
        matches!(event, ServerEvent::FriendPresence { user, online: false } if *user == bob_user.id)
    })
    .await;
}

#[tokio::test]
async fn searches_messages_in_room() {
    let dir = temp_dir("message-search");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, _) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    alice.add_friend("bob").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;
    alice.open_direct(bob_user.id).await.unwrap();
    let room = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.kind == RoomKind::Direct))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => {
            rooms
                .into_iter()
                .find(|room| room.kind == RoomKind::Direct)
                .unwrap()
                .room
        }
        _ => unreachable!(),
    };

    alice.join(room, "Alice").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Joined { .. })).await;
    bob.join(room, "Bob").await.unwrap();
    wait_for(&bob, |event| matches!(event, ServerEvent::Joined { .. })).await;

    alice
        .send_text(room, "hello searchable world")
        .await
        .unwrap();
    let seq = match wait_for(&bob, |event| matches!(event, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => message.seq,
        _ => unreachable!(),
    };

    alice.search_messages(room, "searchable").await.unwrap();
    match wait_for(&alice, |event| {
        matches!(event, ServerEvent::SearchResults { .. })
    })
    .await
    {
        ServerEvent::SearchResults { messages, .. } => {
            assert!(messages
                .iter()
                .any(|message| message.text.contains("searchable")));
        }
        _ => unreachable!(),
    }

    alice.edit_message(room, seq, "edited text").await.unwrap();
    wait_for(&bob, |event| {
        matches!(event, ServerEvent::MessageEdited { message }
            if message.text == "edited text")
    })
    .await;
    alice.recall_message(room, seq).await.unwrap();
    wait_for(&bob, |event| {
        matches!(event, ServerEvent::MessageRecalled { .. })
    })
    .await;
}

#[tokio::test]
async fn messages_push_room_summaries_to_members_not_in_the_room() {
    let dir = temp_dir("rooms-unread");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, _) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    alice.add_friend("bob").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;
    alice.open_direct(bob_user.id).await.unwrap();
    let room = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.kind == RoomKind::Direct))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => {
            rooms
                .into_iter()
                .find(|room| room.kind == RoomKind::Direct)
                .unwrap()
                .room
        }
        _ => unreachable!(),
    };

    // 只有 alice 进入房间；bob 不在房间里，也应该收到会话摘要更新
    alice.join(room, "Alice").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Joined { .. })).await;
    alice.send_text(room, "不在房间也要看到未读").await.unwrap();

    wait_for(&bob, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
        if rooms.iter().any(|known| {
            known.room == room
                && known.last_text.as_deref() == Some("不在房间也要看到未读")
        }))
    })
    .await;
}

#[tokio::test]
async fn mark_read_persists_in_room_summary() {
    let dir = temp_dir("mark-read");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, _) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    alice.add_friend("bob").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;
    alice.open_direct(bob_user.id).await.unwrap();
    let room = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.kind == RoomKind::Direct))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => {
            rooms
                .into_iter()
                .find(|room| room.kind == RoomKind::Direct)
                .unwrap()
                .room
        }
        _ => unreachable!(),
    };
    alice.join(room, "Alice").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Joined { .. })).await;
    alice.send_text(room, "已读位置").await.unwrap();
    let seq = match wait_for(&alice, |event| matches!(event, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => message.seq,
        _ => unreachable!(),
    };

    alice.mark_read(room, seq).await.unwrap();
    alice.list_rooms().await.unwrap();
    wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|known| known.room == room && known.read_seq == seq))
    })
    .await;
}
