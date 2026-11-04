//! Hub：账号、好友、单聊/群聊与按房间隔离的消息。

use std::time::Duration;

use chat_core::{Client, DeviceId, ErrorCode, RoomKind, ServerEvent, UserId, UserSummary};
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

/// 走完一次好友申请：requester 申请、target 同意。
async fn be_friends(
    requester: &Client,
    requester_id: UserId,
    target: &Client,
    target_username: &str,
) {
    requester.add_friend(target_username).await.unwrap();
    target.accept_friend(requester_id).await.unwrap();
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

    be_friends(&alice, alice_user.id, &bob, "bob").await;
    match wait_for(
        &alice,
        |event| matches!(event, ServerEvent::Friends { users } if !users.is_empty()),
    )
    .await
    {
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
async fn logging_out_revokes_the_token_for_good() {
    let dir = temp_dir("logout");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let client = Client::new(hub.connect(DeviceId::new()).await);
    client
        .register("dave", "password123", "dave")
        .await
        .unwrap();
    let token = match wait_for(&client, |event| {
        matches!(event, ServerEvent::Authenticated { .. })
    })
    .await
    {
        ServerEvent::Authenticated { token, .. } => token,
        _ => unreachable!(),
    };

    client.logout().await.unwrap();
    wait_for(&client, |event| matches!(event, ServerEvent::AuthRequired)).await;

    // 同一个令牌再拿来恢复登录必须被拒
    let other = Client::new(hub.connect(DeviceId::new()).await);
    other.resume(&token).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), other.next_event())
        .await
        .expect("等待事件超时")
        .expect("连接已关闭");
    assert!(
        matches!(event, ServerEvent::AuthRequired),
        "登出后的令牌应失效，实际收到 {event:?}"
    );
}

#[tokio::test]
async fn lagging_client_is_told_to_resync_instead_of_silently_losing_events() {
    let dir = temp_dir("lag");
    // 这条用例靠灌满队列，先关掉限流
    let hub = Hub::open_with_limit(&dir, "大厅", f64::INFINITY, f64::INFINITY)
        .await
        .unwrap();
    let room = hub.default_room();

    let (alice, _alice_user) = register(&hub, "alice").await;
    let (bob, _bob_user) = register(&hub, "bob").await;
    alice.join(room, "alice").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Joined { .. })).await;
    bob.join(room, "bob").await.unwrap();
    wait_for(&bob, |event| matches!(event, ServerEvent::Joined { .. })).await;

    // alice 不读事件，bob 一直发，直到 alice 的事件队列被灌满
    for i in 0..400 {
        bob.send_text(room, &format!("第 {i} 条")).await.unwrap();
    }

    // alice 开始读：丢过事件就必须先被告知，否则这一截永远补不回来
    let mut saw_lag = false;
    for _ in 0..600 {
        let event = tokio::time::timeout(Duration::from_secs(2), alice.next_event())
            .await
            .expect("等待事件超时")
            .expect("连接已关闭");
        if matches!(event, ServerEvent::Lag) {
            saw_lag = true;
            break;
        }
    }
    assert!(saw_lag, "队列溢出后必须下发 Lag，而不是静默丢弃");
}

#[tokio::test]
async fn groups_only_include_friends_and_reject_outsiders() {
    let dir = temp_dir("group");
    let hub = Hub::open(&dir, "大厅").await.unwrap();

    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;
    let (carol, _) = register(&hub, "carol").await;
    be_friends(&alice, alice_user.id, &bob, "bob").await;
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
async fn a_friend_request_reaches_the_target_and_acceptance_refreshes_both_sides() {
    let dir = temp_dir("friend-notify");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    alice.add_friend("bob").await.unwrap();
    // 被申请的一方在线时立刻看到待处理申请，不必重新登录
    match wait_for(&bob, |event| {
        matches!(event, ServerEvent::FriendRequests { users }
            if users.iter().any(|user| user.username == "alice"))
    })
    .await
    {
        ServerEvent::FriendRequests { .. } => {}
        _ => unreachable!(),
    }
    // 申请方此时还不是好友
    match wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await {
        ServerEvent::Friends { users } => assert!(users.is_empty()),
        _ => unreachable!(),
    }

    bob.accept_friend(alice_user.id).await.unwrap();
    for client in [&alice, &bob] {
        let peer = if std::ptr::eq(client, &alice) {
            "bob"
        } else {
            "alice"
        };
        match wait_for(client, |event| {
            matches!(event, ServerEvent::Friends { users }
                if users.iter().any(|user| user.username == peer))
        })
        .await
        {
            ServerEvent::Friends { .. } => {}
            _ => unreachable!(),
        }
    }
    // 处理完之后申请列表清空
    match wait_for(&bob, |event| {
        matches!(event, ServerEvent::FriendRequests { .. })
    })
    .await
    {
        ServerEvent::FriendRequests { users } => assert!(users.is_empty()),
        _ => unreachable!(),
    }
    let _ = bob_user;
}

#[tokio::test]
async fn group_and_direct_creation_push_rooms_to_members() {
    let dir = temp_dir("rooms-push");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    be_friends(&alice, alice_user.id, &bob, "bob").await;
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

    be_friends(&alice, alice_user.id, &bob, "bob").await;
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    be_friends(&alice, alice_user.id, &carol, "carol").await;
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
    let (alice, alice_user) = register(&hub, "alice").await;
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

    be_friends(&alice, alice_user.id, &bob, "bob").await;
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
    be_friends(&alice, alice_user.id, &bob, "bob").await;
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
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    be_friends(&alice, alice_user.id, &bob, "bob").await;
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

    // 撤回保留序号占位：历史条数不变、内容清空，客户端分页不会出现空洞
    alice.history(room, None, 10).await.unwrap();
    match wait_for(&alice, |event| matches!(event, ServerEvent::History { .. })).await {
        ServerEvent::History { messages, .. } => {
            assert_eq!(messages.len(), 1);
            let message = &messages[0];
            assert_eq!(message.seq, seq);
            assert!(message.recalled);
            assert!(message.text.is_empty());
            assert!(message.quote.is_none());
        }
        _ => unreachable!(),
    }

    // 撤回后不能再编辑，也不能重复撤回
    alice.recall_message(room, seq).await.unwrap();
    match wait_for(&alice, |event| matches!(event, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::NotFound),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn messages_push_room_summaries_to_members_not_in_the_room() {
    let dir = temp_dir("rooms-unread");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    be_friends(&alice, alice_user.id, &bob, "bob").await;
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
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;

    be_friends(&alice, alice_user.id, &bob, "bob").await;
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

/// 计数器跟随真实事件走，`libchat serve` 的状态行与退出小结才有意义。
#[tokio::test]
async fn counters_track_connections_and_messages() {
    let dir = temp_dir("counters");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let before = hub.counters();

    let (alice, _) = register(&hub, "alice").await;
    let room = hub.default_room();
    alice.join(room, "alice").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Joined { .. })).await;
    alice.send_text(room, "计数").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Message { .. })).await;

    let after = hub.counters();
    assert!(after.connections_opened > before.connections_opened);
    assert!(after.messages_appended > before.messages_appended);

    drop(alice);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(hub.counters().connections_closed > after.connections_closed);
}

/// 刷屏会被限速，但正常节奏的聊天不受影响。
#[tokio::test]
async fn sending_faster_than_the_bucket_allows_is_rate_limited() {
    let dir = temp_dir("rate");
    let hub = Hub::open_with_limit(&dir, "大厅", 3.0, 0.0001)
        .await
        .unwrap();
    let (alice, _) = register(&hub, "alice").await;
    let room = hub.default_room();
    alice.join(room, "alice").await.unwrap();
    wait_for(&alice, |event| matches!(event, ServerEvent::Joined { .. })).await;

    for index in 0..3 {
        alice
            .send_text(room, &format!("第 {index} 条"))
            .await
            .unwrap();
        wait_for(&alice, |event| matches!(event, ServerEvent::Message { .. })).await;
    }

    alice.send_text(room, "第四条").await.unwrap();
    match wait_for(&alice, |event| matches!(event, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::RateLimited),
        _ => unreachable!(),
    }
}

/// 静音按人存：只影响自己看到的摘要，另一个人仍然正常计数。
#[tokio::test]
async fn muting_a_room_is_per_member_and_survives_relisting() {
    let dir = temp_dir("mute");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;
    be_friends(&alice, alice_user.id, &bob, "bob").await;
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;

    let room = hub.default_room();
    assert!(!rooms(&alice).await.iter().any(|known| known.muted));

    alice.set_muted(room, true).await.unwrap();
    let muted = match wait_for(&alice, |event| matches!(event, ServerEvent::Rooms { .. })).await {
        ServerEvent::Rooms { rooms } => rooms
            .into_iter()
            .find(|known| known.room == room)
            .expect("大厅应在列表里"),
        _ => unreachable!(),
    };
    assert!(muted.muted, "自己看到的是静音");

    // 重新拉一次列表，静音要还在
    assert!(rooms(&alice)
        .await
        .iter()
        .any(|known| known.room == room && known.muted));
    // 对方不受影响
    assert!(rooms(&bob)
        .await
        .iter()
        .all(|known| !known.muted || known.room != room));

    let _ = bob_user;
}

/// 群公告只有群主能改，成员看到的是最新的那条。
#[tokio::test]
async fn only_the_owner_can_set_the_group_announcement() {
    let dir = temp_dir("announcement");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;
    be_friends(&alice, alice_user.id, &bob, "bob").await;
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;

    alice
        .create_group("公告群", vec![bob_user.id])
        .await
        .unwrap();
    let group = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.name == "公告群"))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => {
            rooms
                .into_iter()
                .find(|room| room.name == "公告群")
                .unwrap()
                .room
        }
        _ => unreachable!(),
    };

    alice
        .set_announcement(group, Some("今晚八点发版".into()))
        .await
        .unwrap();
    match wait_for(&bob, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.announcement.is_some()))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => {
            let room = rooms.iter().find(|room| room.room == group).unwrap();
            assert_eq!(room.announcement.as_deref(), Some("今晚八点发版"));
            assert!(!room.owner, "bob 不是群主");
        }
        _ => unreachable!(),
    }

    // 普通成员改不了
    bob.set_announcement(group, Some("我改的".into()))
        .await
        .unwrap();
    match wait_for(&bob, |event| matches!(event, ServerEvent::Error { .. })).await {
        ServerEvent::Error { code, .. } => assert_eq!(code, ErrorCode::Forbidden),
        _ => unreachable!(),
    }
}

/// 同一账号在另一台设备登录：消息互通，已读位置共享。
#[tokio::test]
async fn the_same_account_on_two_devices_shares_messages_and_read_state() {
    let dir = temp_dir("multi-device");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (phone, user) = register(&hub, "alice").await;

    // 第二台设备：同一账号另开一条连接
    let laptop = Client::new(hub.connect(DeviceId::new()).await);
    laptop.login("alice", "password123").await.unwrap();
    wait_for(&laptop, |event| {
        matches!(event, ServerEvent::Authenticated { .. })
    })
    .await;

    let room = hub.default_room();
    for client in [&phone, &laptop] {
        client.join(room, "Alice").await.unwrap();
        wait_for(client, |event| matches!(event, ServerEvent::Joined { .. })).await;
    }

    // 手机上发，电脑上应该收到
    phone.send_text(room, "两边都看得到").await.unwrap();
    match wait_for(&laptop, |event| {
        matches!(event, ServerEvent::Message { .. })
    })
    .await
    {
        ServerEvent::Message { message } => {
            assert_eq!(message.text, "两边都看得到");
            assert_eq!(message.sender, user.id);
        }
        _ => unreachable!(),
    }

    // 电脑上读了，手机拿到的会话摘要里 read_seq 也要跟上
    laptop.mark_read(room, 1).await.unwrap();
    match wait_for(&phone, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|known| known.room == room && known.read_seq == 1))
    })
    .await
    {
        ServerEvent::Rooms { .. } => {}
        _ => unreachable!(),
    }

    // 在线人数按连接算：同一个人开两个端就算两条连接
    assert_eq!(hub.online_count().await, 2);
}

/// 置顶：任何成员都能置顶，摘要里带着快照；取消后变空。
#[tokio::test]
async fn any_member_can_pin_and_unpin_a_message() {
    let dir = temp_dir("pin");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;
    be_friends(&alice, alice_user.id, &bob, "bob").await;
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;

    alice
        .create_group("置顶群", vec![bob_user.id])
        .await
        .unwrap();
    let group = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms } if rooms.iter().any(|room| room.name == "置顶群"))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => rooms
            .into_iter()
            .find(|room| room.name == "置顶群")
            .unwrap()
            .room,
        _ => unreachable!(),
    };
    for (client, name) in [(&alice, "Alice"), (&bob, "Bob")] {
        client.join(group, name).await.unwrap();
        wait_for(client, |event| matches!(event, ServerEvent::Joined { .. })).await;
    }

    alice.send_text(group, "要置顶的话").await.unwrap();
    let seq = match wait_for(&bob, |event| matches!(event, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => message.seq,
        _ => unreachable!(),
    };

    // 普通成员也能置顶
    bob.pin_message(group, Some(seq)).await.unwrap();
    match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.room == group && room.pinned.is_some()))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => {
            let pinned = rooms
                .iter()
                .find(|room| room.room == group)
                .and_then(|room| room.pinned.clone())
                .unwrap();
            assert_eq!(pinned.seq, seq);
            assert_eq!(pinned.text, "要置顶的话");
            assert_eq!(pinned.sender, "Alice");
        }
        _ => unreachable!(),
    }

    bob.pin_message(group, None).await.unwrap();
    match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms }
            if rooms.iter().any(|room| room.room == group && room.pinned.is_none()))
    })
    .await
    {
        ServerEvent::Rooms { .. } => {}
        _ => unreachable!(),
    }
}

/// 转发：把群里的消息复制到单聊，双方都能收到，正文与附件照搬。
#[tokio::test]
async fn forwarding_copies_the_message_into_another_room() {
    let dir = temp_dir("forward");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;
    be_friends(&alice, alice_user.id, &bob, "bob").await;
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;

    alice
        .create_group("来源群", vec![bob_user.id])
        .await
        .unwrap();
    let group = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms } if rooms.iter().any(|room| room.name == "来源群"))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => rooms
            .into_iter()
            .find(|room| room.name == "来源群")
            .unwrap()
            .room,
        _ => unreachable!(),
    };
    alice.open_direct(bob_user.id).await.unwrap();

    let direct = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms } if rooms.iter().any(|room| room.kind == RoomKind::Direct))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => rooms
            .into_iter()
            .find(|room| room.kind == RoomKind::Direct)
            .unwrap()
            .room,
        _ => unreachable!(),
    };

    for (client, name) in [(&alice, "Alice"), (&bob, "Bob")] {
        client.join(group, name).await.unwrap();
        wait_for(client, |event| matches!(event, ServerEvent::Joined { .. })).await;
    }
    alice.send_text(group, "要转发的话").await.unwrap();
    let seq = match wait_for(&bob, |event| matches!(event, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => message.seq,
        _ => unreachable!(),
    };

    for (client, name) in [(&alice, "Alice"), (&bob, "Bob")] {
        client.join(direct, name).await.unwrap();
        wait_for(client, |event| matches!(event, ServerEvent::Joined { .. })).await;
    }
    alice.forward(group, seq, direct).await.unwrap();
    match wait_for(
        &bob,
        |event| matches!(event, ServerEvent::Message { message } if message.forwarded),
    )
    .await
    {
        ServerEvent::Message { message } => {
            assert_eq!(message.room, direct);
            assert_eq!(message.text, "要转发的话");
            assert_eq!(message.sender, alice_user.id, "转发者是发送者");
        }
        _ => unreachable!(),
    }
}

/// 群里的已读人数：别人读过之后，发送者能看到「几人已读」。
#[tokio::test]
async fn read_counts_reach_the_room_after_a_member_reads() {
    let dir = temp_dir("read-counts");
    let hub = Hub::open(&dir, "大厅").await.unwrap();
    let (alice, alice_user) = register(&hub, "alice").await;
    let (bob, bob_user) = register(&hub, "bob").await;
    be_friends(&alice, alice_user.id, &bob, "bob").await;
    wait_for(&alice, |event| matches!(event, ServerEvent::Friends { .. })).await;
    wait_for(&bob, |event| matches!(event, ServerEvent::Friends { .. })).await;

    alice
        .create_group("计数群", vec![bob_user.id])
        .await
        .unwrap();
    let group = match wait_for(&alice, |event| {
        matches!(event, ServerEvent::Rooms { rooms } if rooms.iter().any(|room| room.name == "计数群"))
    })
    .await
    {
        ServerEvent::Rooms { rooms } => rooms
            .into_iter()
            .find(|room| room.name == "计数群")
            .unwrap()
            .room,
        _ => unreachable!(),
    };
    for (client, name) in [(&alice, "Alice"), (&bob, "Bob")] {
        client.join(group, name).await.unwrap();
        wait_for(client, |event| matches!(event, ServerEvent::Joined { .. })).await;
    }

    alice.send_text(group, "有人读吗").await.unwrap();
    let seq = match wait_for(&bob, |event| matches!(event, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => message.seq,
        _ => unreachable!(),
    };

    bob.mark_read(group, seq).await.unwrap();
    match wait_for(
        &alice,
        |event| matches!(event, ServerEvent::ReadCounts { counts, .. } if !counts.is_empty()),
    )
    .await
    {
        ServerEvent::ReadCounts { counts, .. } => {
            let readers = counts.iter().find(|(at, _)| *at == seq).map(|(_, n)| *n);
            assert_eq!(readers, Some(1), "只有 bob 读过");
        }
        _ => unreachable!(),
    }
}
