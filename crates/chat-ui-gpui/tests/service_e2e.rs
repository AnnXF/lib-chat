//! 界面服务链路：创建房间 → 连接 → 看人数 → 加入 → 发言。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chat_core::RoomId;
use chat_transport_quic::Liveness;
use chat_ui_gpui::service::{UiCommand, UiEvent, spawn_with_data};

/// 这些用例都要占用全局的 mDNS(5353) 与广播端口，必须串行执行。
///
/// 另外给每个用例分配独立的广播端口：同一端口上并存多个套接字时广播报文会被
/// 分流，上一轮残留的后台实例会把应答抢走，导致发现偶发失败。
fn serial() -> (std::sync::MutexGuard<'static, ()>, u16) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let offset = NEXT_PORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // 等上一轮后台线程退出，避免端口短暂重叠
    std::thread::sleep(Duration::from_millis(400));

    // 库文件按「端口 + 序号」命名，这里清掉上一轮同端口的残留，
    // 否则新用例会顶着旧房间的身份跑起来。
    let port = 53_100 + offset;
    let prefix = format!("libchat-e2e-{port}-");
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    (guard, port)
}

/// 每个后端一个独占的库目录：用例之间不串数据，也不碰真实的 `~/.lib-chat`。
fn test_data_dir(beacon_port: u16) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!("libchat-e2e-{beacon_port}-{n}"))
}

/// 按用例端口启动一个后端，消息库落在临时目录里。
fn spawn_test(beacon_port: u16) -> (flume::Sender<UiCommand>, flume::Receiver<UiEvent>) {
    spawn_with_data(beacon_port, Liveness::default(), test_data_dir(beacon_port))
}

/// 在给定时间内等一个满足条件的事件；等不到就返回 `None`。
fn wait_for(
    events: &flume::Receiver<UiEvent>,
    pred: impl Fn(&UiEvent) -> bool,
    wait: std::time::Duration,
) -> Option<UiEvent> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match events.recv_timeout(left) {
            Ok(event) if pred(&event) => return Some(event),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn recv_until<F>(events: &flume::Receiver<UiEvent>, pred: F) -> UiEvent
where
    F: Fn(&UiEvent) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                if pred(&event) {
                    return event;
                }
            }
            Err(_) => continue,
        }
    }
    panic!("未在超时内收到期望事件");
}

#[test]
fn host_then_connect_join_send() {
    let (_guard, beacon_port) = serial();
    let (cmds, events) = spawn_test(beacon_port);

    cmds.send(UiCommand::Host {
        name: "测试房间".into(),
    })
    .unwrap();
    let UiEvent::Hosting {
        addrs, fingerprint, ..
    } = recv_until(&events, |e| matches!(e, UiEvent::Hosting { .. }))
    else {
        unreachable!()
    };
    let addr = addrs
        .iter()
        .find(|a| a.starts_with("127.0.0.1"))
        .expect("应提供回环地址便于单机自测")
        .clone();
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));

    cmds.send(UiCommand::Connect {
        addr,
        fingerprint: Some(fingerprint),
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { name, online, .. } => {
            assert_eq!(name, "测试房间");
            assert_eq!(online, 0);
        }
        _ => unreachable!(),
    }

    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Joined { .. })) {
        UiEvent::Joined { online, .. } => assert_eq!(online, 1),
        _ => unreachable!(),
    }

    cmds.send(UiCommand::Send {
        text: "你好".into(),
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Message { .. })) {
        UiEvent::Message { sender, text, .. } => {
            assert_eq!(sender, "Alice");
            assert_eq!(text, "你好");
        }
        _ => unreachable!(),
    }

    cmds.send(UiCommand::Leave).unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Left { .. }));

    // 退出后人数必须立刻回落，而不是停在 1/1
    match recv_until(&events, |e| {
        matches!(e, UiEvent::Presence { online: 0, .. })
    }) {
        UiEvent::Presence { online, .. } => assert_eq!(online, 0),
        _ => unreachable!(),
    }
}

/// 同一台机器上跑两个实例：实例 B 应能自动发现实例 A 创建的房间并聊天。
#[test]
fn two_instances_discover_each_other_on_one_host() {
    let (_guard, beacon_port) = serial();
    let (cmds_a, events_a) = spawn_test(beacon_port);
    let (cmds_b, events_b) = spawn_test(beacon_port);

    // 实例 A 创建房间并加入
    cmds_a
        .send(UiCommand::Host {
            name: "同机房间".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. }));
    recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_a
        .send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));

    // 实例 B 无需任何手工配置即可发现该房间
    let found = recv_until(
        &events_b,
        |e| matches!(e, UiEvent::RoomFound { name, .. } if name == "同机房间"),
    );
    let UiEvent::RoomFound {
        addr, fingerprint, ..
    } = found
    else {
        unreachable!()
    };
    assert!(!fingerprint.is_empty(), "应带证书指纹供用户首次核对");

    cmds_b
        .send(UiCommand::Connect {
            addr,
            fingerprint: Some(fingerprint),
        })
        .unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_b.send(UiCommand::Join { name: "Bob".into() }).unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Joined { .. }));

    // A 应看到 2 人在线，不能把同一个人重复计数
    match recv_until(&events_a, |e| {
        matches!(e, UiEvent::Presence { online: 2, .. })
    }) {
        UiEvent::Presence { online, .. } => assert_eq!(online, 2),
        _ => unreachable!(),
    }

    // 实例 B 发言，实例 A 收到
    cmds_b
        .send(UiCommand::Send {
            text: "同机也能聊".into(),
        })
        .unwrap();
    match recv_until(&events_a, |e| matches!(e, UiEvent::Message { .. })) {
        UiEvent::Message { sender, text, .. } => {
            assert_eq!(sender, "Bob");
            assert_eq!(text, "同机也能聊");
        }
        _ => unreachable!(),
    }
}

#[test]
fn discovery_requires_confirmation_then_persists_trust() {
    use chat_transport_quic::TrustStore;

    let (_guard, beacon_port) = serial();
    let root = test_data_dir(beacon_port);
    let owner_dir = root.join("owner");
    let guest_dir = root.join("guest");
    let (owner, owner_events) = spawn_with_data(beacon_port, Liveness::default(), owner_dir);
    owner
        .send(UiCommand::Host {
            name: "需要验证的房间".into(),
        })
        .unwrap();
    let fingerprint = match recv_until(&owner_events, |e| matches!(e, UiEvent::Hosting { .. })) {
        UiEvent::Hosting { fingerprint, .. } => fingerprint,
        _ => unreachable!(),
    };
    let room = match recv_until(&owner_events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };
    let (guest, guest_events) =
        spawn_with_data(beacon_port, Liveness::default(), guest_dir.clone());
    let addr = match recv_until(
        &guest_events,
        |e| matches!(e, UiEvent::RoomFound { room: found, .. } if *found == room),
    ) {
        UiEvent::RoomFound { addr, .. } => addr,
        _ => unreachable!(),
    };
    guest
        .send(UiCommand::ConnectDiscovered {
            room,
            addr: addr.clone(),
            fingerprint: fingerprint.clone(),
        })
        .unwrap();
    let pending = recv_until(&guest_events, |e| {
        matches!(e, UiEvent::TrustRequired { .. })
    });
    assert!(matches!(pending, UiEvent::TrustRequired { room: r, .. } if r == room));
    assert_eq!(TrustStore::new(&guest_dir).get(room).unwrap(), None);

    guest
        .send(UiCommand::ConfirmTrust {
            room,
            addr: addr.clone(),
            fingerprint: fingerprint.clone(),
        })
        .unwrap();
    recv_until(
        &guest_events,
        |e| matches!(e, UiEvent::Lobby { room: joined, .. } if *joined == room),
    );
    assert_eq!(
        TrustStore::new(&guest_dir).get(room).unwrap(),
        Some(fingerprint.clone())
    );
    guest
        .send(UiCommand::ConnectDiscovered {
            room,
            addr,
            fingerprint: "f".repeat(64),
        })
        .unwrap();
    recv_until(
        &guest_events,
        |e| matches!(e, UiEvent::Error(message) if message.contains("指纹已变更")),
    );
    assert_eq!(
        TrustStore::new(&guest_dir).get(room).unwrap(),
        Some(fingerprint)
    );
}

/// 切换房间（这里用「重新创建房间」触发）时会主动退出旧房间，
/// 旧房间的在线人数必须立刻回落，不能留下任何残留计数。
#[test]
fn leaving_on_switch_drops_online_count() {
    let (_guard, beacon_port) = serial();

    // B 托管房间并在其中
    let (cmds_b, events_b) = spawn_test(beacon_port);
    cmds_b
        .send(UiCommand::Host {
            name: "房间A".into(),
        })
        .unwrap();
    let (addr_a, fingerprint_a) =
        match recv_until(&events_b, |e| matches!(e, UiEvent::Hosting { .. })) {
            UiEvent::Hosting {
                addrs, fingerprint, ..
            } => (
                addrs
                    .into_iter()
                    .find(|addr| addr.starts_with("127.0.0.1"))
                    .expect("应提供回环地址"),
                fingerprint,
            ),
            _ => unreachable!(),
        };
    recv_until(&events_b, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_b.send(UiCommand::Join { name: "Bob".into() }).unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Joined { .. }));

    // A 进入同一个房间并加入
    let (cmds_a, events_a) = spawn_test(beacon_port);
    cmds_a
        .send(UiCommand::Connect {
            addr: addr_a,
            fingerprint: Some(fingerprint_a),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_a
        .send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));

    match recv_until(&events_b, |e| {
        matches!(e, UiEvent::Presence { online: 2, .. })
    }) {
        UiEvent::Presence { online, .. } => assert_eq!(online, 2),
        _ => unreachable!(),
    }

    // A 改去创建新房间：应当先退出旧房间
    cmds_a
        .send(UiCommand::Host {
            name: "房间A2".into(),
        })
        .unwrap();

    match recv_until(&events_b, |e| {
        matches!(e, UiEvent::Presence { online: 1, .. })
    }) {
        UiEvent::Presence { online, .. } => {
            assert_eq!(online, 1, "在线应回落，不能留下残留计数");
        }
        _ => unreachable!(),
    }
}

/// 同一房间会经 mDNS 与 UDP 广播重复播报，且地址可能不止一个；
/// 上报必须始终带同一个房间标识，界面才能去重成一条。
#[test]
fn same_room_is_reported_under_one_id() {
    let (_guard, beacon_port) = serial();
    let (cmds, events) = spawn_test(beacon_port);
    cmds.send(UiCommand::Host {
        name: "唯一房间".into(),
    })
    .unwrap();
    let room = match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };

    // 局域网里可能同时存在别的房间，所以只断言「同一个地址永远对应同一个标识」，
    // 并要求至少看到自己那个房间一次。
    let mut by_addr: HashMap<String, RoomId> = HashMap::new();
    let mut seen_ours = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(UiEvent::RoomFound {
            room: found, addr, ..
        }) = events.recv_timeout(Duration::from_millis(200))
        {
            assert!(!addr.is_empty());
            match by_addr.get(&addr) {
                Some(existing) => assert_eq!(
                    *existing, found,
                    "同一地址 {addr} 被上报成了两个不同的房间标识"
                ),
                None => {
                    by_addr.insert(addr, found);
                }
            }
            seen_ours |= found == room;
        }
    }
    assert!(seen_ours, "应至少发现一次自己创建的房间");
}

/// 等待事件，但「被踢出房间」直接判失败。
///
/// 重连期间收到 [`UiEvent::Disconnected`] 就意味着用户被强行弹出房间，正是要防的回归。
fn recv_until_keeping_room<F>(events: &flume::Receiver<UiEvent>, pred: F) -> UiEvent
where
    F: Fn(&UiEvent) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(UiEvent::Disconnected) => panic!("重连期间不应把用户踢出房间"),
            Ok(event) => {
                if pred(&event) {
                    return event;
                }
            }
            Err(_) => continue,
        }
    }
    panic!("未在超时内收到期望事件");
}

/// 连接因空闲断开后必须自动重连并恢复身份，而不是把用户踢回大厅。
///
/// 用 1 秒空闲超时 + 不保活来复现「安静房间里连接被判死」，把真等 30 秒压成真等 2 秒。
#[test]
fn reconnects_after_connection_drops_without_leaving_room() {
    let (_guard, beacon_port) = serial();
    let flaky = Liveness {
        idle_timeout: Duration::from_secs(1),
        // 远大于空闲超时，等价于「不发保活包」，保证连接一定会被判死
        keep_alive_interval: Duration::from_secs(60),
    };
    let (cmds, events) = spawn_with_data(beacon_port, flaky, test_data_dir(beacon_port));

    cmds.send(UiCommand::Host {
        name: "会掉线的房间".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Hosting { .. }));
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));

    // 安静下来，等连接因空闲被判死，再等它自己恢复
    recv_until_keeping_room(&events, |e| matches!(e, UiEvent::Reconnecting(_)));
    recv_until_keeping_room(&events, |e| matches!(e, UiEvent::Joined { .. }));

    // 重连必须恢复身份：还能正常发言，说明服务端认得这个成员
    cmds.send(UiCommand::Send {
        text: "掉线之后还能说话".into(),
    })
    .unwrap();
    match recv_until_keeping_room(&events, |e| matches!(e, UiEvent::Message { .. })) {
        UiEvent::Message { text, .. } => assert_eq!(text, "掉线之后还能说话"),
        _ => unreachable!(),
    }
}

/// 同一个数据目录 + 同一个端口 = 同一个房间：重启后应当还原成原房间，而不是另起一个。
#[test]
fn hosting_again_reuses_the_same_room() {
    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);

    let first = host_once(beacon_port, &dir);
    // 让上一个后端彻底退出，端口释放
    std::thread::sleep(Duration::from_millis(600));
    let second = host_once(beacon_port, &dir);

    assert_eq!(first, second, "同一个库 + 同一个端口应还原成同一个房间");
}

/// 起一个后端建一次房，返回房间标识；返回前后端就结束了。
fn host_once(beacon_port: u16, dir: &std::path::Path) -> RoomId {
    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.to_path_buf());
    cmds.send(UiCommand::Host {
        name: "本机房间".into(),
    })
    .unwrap();
    let room = match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };
    drop(cmds);
    drop(events);
    room
}

/// 重开软件后应当提示本机房间还在，点它就能回到原房间、历史都在。
#[test]
fn reopening_offers_the_local_room_back() {
    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);

    // 全新的数据目录不该凭空冒出一个「本机房间」
    {
        let (_cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
        let found = wait_for(
            &events,
            |e| matches!(e, UiEvent::LocalRoom { .. }),
            Duration::from_millis(400),
        );
        assert!(found.is_none(), "全新目录不该有本机房间");
    }

    // 建一次房，说三句话
    {
        let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
        cmds.send(UiCommand::Host {
            name: "会留下的房间".into(),
        })
        .unwrap();
        recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
        cmds.send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
        recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));
        for i in 1..=3 {
            cmds.send(UiCommand::Send {
                text: format!("第 {i} 句"),
            })
            .unwrap();
        }
        recv_until(
            &events,
            |e| matches!(e, UiEvent::Message { text, .. } if text.as_str() == "第 3 句"),
        );
        drop(cmds);
        drop(events);
    }
    std::thread::sleep(Duration::from_millis(600));

    // 重开：先提示本机房间还在，再点进去应看到原来的历史
    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    let room = match wait_for(
        &events,
        |e| matches!(e, UiEvent::LocalRoom { .. }),
        Duration::from_secs(2),
    ) {
        Some(UiEvent::LocalRoom { messages, room }) => {
            assert_eq!(messages, 3);
            room
        }
        _ => panic!("重开后应提示本机房间还在"),
    };
    let fingerprint = chat_transport_quic::TrustStore::new(&dir)
        .get(room)
        .unwrap()
        .unwrap();

    cmds.send(UiCommand::Host {
        name: "会留下的房间".into(),
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Hosting { .. })) {
        UiEvent::Hosting {
            room: reopened,
            fingerprint: next,
            ..
        } => {
            assert_eq!(reopened, room);
            assert_eq!(next, fingerprint);
        }
        _ => unreachable!(),
    }
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::History { .. })) {
        UiEvent::History { messages, .. } => {
            assert_eq!(messages.len(), 3);
            assert_eq!(messages[0].text, "第 1 句");
        }
        _ => unreachable!(),
    }
}

/// 「新建房间」把本机房间的历史清空，从空白重新开始。
#[test]
fn new_room_clears_the_local_history() {
    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);

    // 先留一条历史
    {
        let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
        cmds.send(UiCommand::Host {
            name: "旧房间".into(),
        })
        .unwrap();
        recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
        cmds.send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
        recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));
        cmds.send(UiCommand::Send {
            text: "旧消息".into(),
        })
        .unwrap();
        recv_until(&events, |e| matches!(e, UiEvent::Message { .. }));
        drop(cmds);
        drop(events);
    }
    std::thread::sleep(Duration::from_millis(600));

    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    wait_for(
        &events,
        |e| matches!(e, UiEvent::LocalRoom { .. }),
        Duration::from_secs(2),
    );

    cmds.send(UiCommand::NewRoom {
        name: "新房间".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::History { .. })) {
        UiEvent::History {
            messages, has_more, ..
        } => {
            assert!(messages.is_empty(), "新建的房间不该带着旧历史");
            assert!(!has_more);
        }
        _ => unreachable!(),
    }
}

#[test]
#[ignore]
fn reset_development_room_from_environment() {
    use chat_core::MessageStore;
    use chat_server::SqliteStore;
    use chat_transport_quic::TrustStore;

    let dir = std::path::PathBuf::from(
        std::env::var("LIBCHAT_RESET_DIR").expect("指定 LIBCHAT_RESET_DIR"),
    );
    let port: u16 = std::env::var("LIBCHAT_RESET_PORT")
        .expect("指定 LIBCHAT_RESET_PORT")
        .parse()
        .unwrap();
    let (cmds, events) = spawn_with_data(port, Liveness::default(), dir.clone());
    cmds.send(UiCommand::NewRoom {
        name: "本机房间".into(),
    })
    .unwrap();
    let (room, fingerprint) = match recv_until(&events, |e| {
        matches!(e, UiEvent::Hosting { .. } | UiEvent::Error(_))
    }) {
        UiEvent::Hosting {
            room, fingerprint, ..
        } => (room, fingerprint),
        UiEvent::Error(reason) => panic!("重建本机房间失败: {reason}"),
        _ => unreachable!(),
    };
    recv_until(
        &events,
        |e| matches!(e, UiEvent::Lobby { room: joined, .. } if *joined == room),
    );
    assert_eq!(TrustStore::new(&dir).get(room).unwrap(), Some(fingerprint));
    drop(cmds);
    drop(events);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = SqliteStore::open(&dir.join(format!("room-{port}.db")))
            .await
            .unwrap();
        assert_eq!(store.room(), room);
        assert_eq!(store.latest_seq(room).await.unwrap(), 0);
        store.close().await;
    });
}

/// 已经开着一个房间时再点「新建房间」，新房间必须照常可用。
#[test]
fn new_room_while_hosting_keeps_the_room_usable() {
    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);
    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir);

    cmds.send(UiCommand::Host {
        name: "第一个".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Hosting { .. }));
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));
    cmds.send(UiCommand::Send {
        text: "旧消息".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Message { .. }));

    cmds.send(UiCommand::NewRoom {
        name: "第二个".into(),
    })
    .unwrap();

    let hosting = wait_for(
        &events,
        |e| matches!(e, UiEvent::Hosting { .. }),
        Duration::from_secs(5),
    );
    assert!(hosting.is_some(), "新建后应当重新发出 Hosting");

    let lobby = wait_for(
        &events,
        |e| matches!(e, UiEvent::Lobby { .. }),
        Duration::from_secs(5),
    );
    assert!(lobby.is_some(), "新建后应当重新接入自己的房间");

    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    let joined = wait_for(
        &events,
        |e| matches!(e, UiEvent::Joined { .. }),
        Duration::from_secs(5),
    );
    assert!(joined.is_some(), "新房间应当能正常加入");
}

/// 连着建两个房间，再点「加入房间」，也必须能加入。
#[test]
fn joining_after_two_new_rooms_still_works() {
    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);
    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir);

    for name in ["一号", "二号"] {
        cmds.send(UiCommand::NewRoom { name: name.into() }).unwrap();
        let hosting = wait_for(
            &events,
            |e| matches!(e, UiEvent::Hosting { .. }),
            Duration::from_secs(10),
        );
        assert!(hosting.is_some(), "建「{name}」应当发出 Hosting");
        let lobby = wait_for(
            &events,
            |e| matches!(e, UiEvent::Lobby { .. }),
            Duration::from_secs(10),
        );
        assert!(lobby.is_some(), "建「{name}」后应当接入自己的房间");
    }

    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    let joined = wait_for(
        &events,
        |e| matches!(e, UiEvent::Joined { .. }),
        Duration::from_secs(10),
    );
    assert!(joined.is_some(), "第二个房间应当能正常加入");
}

/// 同机两个实例都开「本机房间」，消息也必须实时到达。
///
/// 本机房间按广播端口落在同一个库文件里：两个实例拿到的是同一个房间标识。各自起一个
/// 服务器的话，消息只落库、只推给自己那半边的连接，看起来就是「不实时，退出再进才看得见」。
/// 后开的那个应当接到先开的那个上。
#[test]
fn two_instances_sharing_the_local_room_deliver_in_real_time() {
    let (_guard, beacon_port) = serial();
    // 两个实例共用一个库目录：正是「同一台机器上开两次」的样子
    let dir = test_data_dir(beacon_port);
    let (cmds_a, events_a) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());

    cmds_a
        .send(UiCommand::Host {
            name: "本机房间".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. }));
    recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_a
        .send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));

    // 第二个实例后开：启动时看到本机已有房间
    let (cmds_b, events_b) = spawn_with_data(beacon_port, Liveness::default(), dir);
    let room = match recv_until(&events_b, |e| matches!(e, UiEvent::LocalRoom { .. })) {
        UiEvent::LocalRoom { room, .. } => room,
        _ => unreachable!(),
    };

    // B 马上就点「我的房间」：这时它多半还没在房间列表里看到这个房间（广播还没认出来），
    // 也必须接到 A 的服务器上，而不是再起一个
    cmds_b
        .send(UiCommand::Host {
            name: "本机房间".into(),
        })
        .unwrap();
    let outcome = wait_for(
        &events_b,
        |e| matches!(e, UiEvent::Lobby { .. } | UiEvent::Hosting { .. }),
        Duration::from_secs(10),
    )
    .expect("点「我的房间」后既没接上房间，也没起服务器");
    match outcome {
        UiEvent::Lobby { room: shown, .. } => {
            assert_eq!(shown, room, "应当接到本机已有实例的那个房间上")
        }
        // 再起一个服务器就会和本机那个裂成两个，消息各推各的
        UiEvent::Hosting { .. } => panic!("本机已经有实例开着这个房间，不该再起一个服务器"),
        _ => unreachable!(),
    }

    cmds_b.send(UiCommand::Join { name: "Bob".into() }).unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Joined { .. }));

    cmds_a
        .send(UiCommand::Send {
            text: "在吗".into(),
        })
        .unwrap();
    assert!(
        wait_for(
            &events_b,
            |e| matches!(e, UiEvent::Message { .. }),
            Duration::from_secs(5)
        )
        .is_some(),
        "A 发的消息应当实时到达 B"
    );

    cmds_b.send(UiCommand::Send { text: "在".into() }).unwrap();
    assert!(
        wait_for(
            &events_a,
            |e| matches!(e, UiEvent::Message { .. }),
            Duration::from_secs(5)
        )
        .is_some(),
        "B 发的消息应当实时到达 A"
    );
}

#[test]
fn second_instance_cannot_reset_a_hosted_room() {
    use chat_core::MessageStore;
    use chat_server::SqliteStore;

    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);
    let (cmds_a, events_a) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    cmds_a
        .send(UiCommand::Host {
            name: "原房间".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. }));
    let room = match recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };
    cmds_a
        .send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));
    cmds_a
        .send(UiCommand::Send {
            text: "保留的历史".into(),
        })
        .unwrap();
    recv_until(
        &events_a,
        |e| matches!(e, UiEvent::Message { text, .. } if text == "保留的历史"),
    );

    let (cmds_b, events_b) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    recv_until(&events_b, |e| matches!(e, UiEvent::LocalRoom { .. }));
    cmds_b
        .send(UiCommand::NewRoom {
            name: "不应创建".into(),
        })
        .unwrap();
    recv_until(
        &events_b,
        |e| matches!(e, UiEvent::Error(message) if message.contains("本机已有实例")),
    );

    cmds_a
        .send(UiCommand::Send {
            text: "房间仍可用".into(),
        })
        .unwrap();
    recv_until(
        &events_a,
        |e| matches!(e, UiEvent::Message { text, .. } if text == "房间仍可用"),
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = SqliteStore::open(&dir.join(format!("room-{beacon_port}.db")))
            .await
            .unwrap();
        assert_eq!(store.room(), room);
        let history = store.history(room, None, 10).await.unwrap();
        let texts: Vec<_> = history
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["保留的历史", "房间仍可用"]);
    });
}

/// 重建房间后，旧房间（本机自己刚撤下的那个）必须马上离开房间列表。
///
/// 列表里留一行连不上的旧房间，用户就会去点它，然后吃到一次「连接超时」。
#[test]
fn rebuilding_the_local_room_drops_the_old_one_right_away() {
    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);
    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir);

    cmds.send(UiCommand::NewRoom {
        name: "一号".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Hosting { .. }));
    // 房间标识用「自己接进自己房间」带回的那个，别拿房间列表里的：
    // mDNS 会让同网段别的房间也飘进来，抓到的可能不是自己的
    let old = match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };

    cmds.send(UiCommand::NewRoom {
        name: "二号".into(),
    })
    .unwrap();

    // 旧房间的「消失」和新房间的「上线」谁先谁后都算数，所以一起收着看
    let deadline = Instant::now() + Duration::from_secs(6);
    let (mut hosting, mut gone, mut came_back) = (false, false, false);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(UiEvent::Hosting { .. }) => hosting = true,
            Ok(UiEvent::RoomGone { room }) if room == old => gone = true,
            Ok(UiEvent::RoomFound { room, .. }) if room == old => came_back = true,
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(hosting, "新房间应当上线");
    assert!(gone, "旧房间应当马上从列表里消失");
    assert!(!came_back, "旧房间不该再冒出来");
}

/// 连一个没人监听的地址必须很快失败。
///
/// 后端是串行处理指令的：一次握手卡住多久，界面就「点不动」多久。房间重启换了端口后，
/// 列表里残留的旧地址正是这种没人监听的地址，点一下就能把界面钉住。
#[test]
fn connecting_to_a_dead_address_fails_quickly() {
    let (_guard, beacon_port) = serial();
    let (cmds, events) = spawn_test(beacon_port);

    // 分配一个本地端口再立刻放掉：此后没人监听
    let dead = {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        drop(socket);
        addr
    };

    let started = Instant::now();
    cmds.send(UiCommand::Connect {
        addr: dead.to_string(),
        fingerprint: Some("a".repeat(64)),
    })
    .unwrap();
    let failed = wait_for(
        &events,
        |e| matches!(e, UiEvent::Error(_)),
        Duration::from_secs(20),
    );
    let elapsed = started.elapsed();

    assert!(failed.is_some(), "连不上应当报错，实际耗时 {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(8),
        "连不上的地址应当很快放弃，实际耗时 {elapsed:?}"
    );
}

#[test]
fn failed_room_switch_keeps_the_old_session() {
    let (_guard, beacon_port) = serial();
    let (cmds, events) = spawn_test(beacon_port);
    cmds.send(UiCommand::Host {
        name: "原房间".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Hosting { .. }));
    let room = match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));

    cmds.send(UiCommand::Connect {
        addr: "127.0.0.1:0".into(),
        fingerprint: None,
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Error(_)));

    cmds.send(UiCommand::Send {
        text: "仍在原房间".into(),
    })
    .unwrap();
    let received = recv_until(
        &events,
        |e| matches!(e, UiEvent::Message { room: r, text, .. } if *r == room && text == "仍在原房间"),
    );
    assert!(matches!(received, UiEvent::Message { .. }));
}

/// 自己建了房间，又从房间列表点进自己的房间，再点「加入房间」，也必须能加入。
#[test]
fn connecting_to_own_room_then_joining_works() {
    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);
    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir);

    cmds.send(UiCommand::Host {
        name: "自己的房间".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Hosting { .. }));
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));

    // 等自己广播的房间被自己发现，然后像用户点房间列表那样连过去
    let found = wait_for(
        &events,
        |e| matches!(e, UiEvent::RoomFound { .. }),
        Duration::from_secs(10),
    );
    let (addr, fingerprint) = match found {
        Some(UiEvent::RoomFound {
            addr, fingerprint, ..
        }) => (addr, fingerprint),
        _ => panic!("没等到自己房间的广播"),
    };

    cmds.send(UiCommand::Connect {
        addr,
        fingerprint: Some(fingerprint),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));

    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    let joined = wait_for(
        &events,
        |e| matches!(e, UiEvent::Joined { .. }),
        Duration::from_secs(10),
    );
    assert!(joined.is_some(), "点进自己的房间后应当能正常加入");
}

/// 加入房间要把之前的对话带回来，否则进房间只看到一片空白。
#[test]
fn joining_room_loads_recent_history() {
    let (_guard, beacon_port) = serial();
    let (cmds_a, events_a) = spawn_test(beacon_port);
    let (cmds_b, events_b) = spawn_test(beacon_port);

    cmds_a
        .send(UiCommand::Host {
            name: "有历史的房间".into(),
        })
        .unwrap();
    let (addr, fingerprint) = match recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. }))
    {
        UiEvent::Hosting {
            addrs, fingerprint, ..
        } => (
            addrs
                .into_iter()
                .find(|addr| addr.starts_with("127.0.0.1"))
                .expect("应提供回环地址"),
            fingerprint,
        ),
        _ => unreachable!(),
    };
    recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_a
        .send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));
    cmds_a
        .send(UiCommand::Send {
            text: "先说的话".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Message { .. }));

    // 后进来的人应在加入后收到这段历史
    cmds_b
        .send(UiCommand::Connect {
            addr,
            fingerprint: Some(fingerprint),
        })
        .unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_b.send(UiCommand::Join { name: "Bob".into() }).unwrap();

    match recv_until(&events_b, |e| matches!(e, UiEvent::History { .. })) {
        UiEvent::History { messages, .. } => {
            assert!(
                messages.iter().any(|m| m.text == "先说的话"),
                "加入后应取回房间已有的对话"
            );
        }
        _ => unreachable!(),
    }
}

/// 离开期间漏掉的消息，重新加入时必须补回来——界面只在收到消息时追加，
/// 不补的话用户永远不知道自己错过了什么。
#[test]
fn rejoining_catches_up_messages_missed_while_away() {
    let (_guard, beacon_port) = serial();
    let (cmds_a, events_a) = spawn_test(beacon_port);
    let (cmds_b, events_b) = spawn_test(beacon_port);

    cmds_a
        .send(UiCommand::Host {
            name: "补消息的房间".into(),
        })
        .unwrap();
    let (addr, fingerprint) = match recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. }))
    {
        UiEvent::Hosting {
            addrs, fingerprint, ..
        } => (
            addrs
                .into_iter()
                .find(|addr| addr.starts_with("127.0.0.1"))
                .expect("应提供回环地址"),
            fingerprint,
        ),
        _ => unreachable!(),
    };
    recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_a
        .send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));

    cmds_b
        .send(UiCommand::Connect {
            addr,
            fingerprint: Some(fingerprint),
        })
        .unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_b.send(UiCommand::Join { name: "Bob".into() }).unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Joined { .. }));
    cmds_b.send(UiCommand::Leave).unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Left { .. }));

    // Bob 不在房间时 Alice 说话
    cmds_a
        .send(UiCommand::Send {
            text: "你刚才不在".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Message { .. }));

    cmds_b.send(UiCommand::Join { name: "Bob".into() }).unwrap();
    match recv_until(&events_b, |e| matches!(e, UiEvent::Missed { .. })) {
        UiEvent::Missed { messages, .. } => {
            assert!(
                messages.iter().any(|m| m.text == "你刚才不在"),
                "重新加入应补齐离开期间的消息"
            );
        }
        _ => unreachable!(),
    }
}

/// 往上翻能取到更早的历史，翻到最早一页时服务端会说没有了。
#[test]
fn loading_older_walks_back_through_history() {
    let (_guard, beacon_port) = serial();
    let (cmds_a, events_a) = spawn_test(beacon_port);
    let (cmds_b, events_b) = spawn_test(beacon_port);

    cmds_a
        .send(UiCommand::Host {
            name: "很长的房间".into(),
        })
        .unwrap();
    let (addr, fingerprint) = match recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. }))
    {
        UiEvent::Hosting {
            addrs, fingerprint, ..
        } => (
            addrs
                .into_iter()
                .find(|addr| addr.starts_with("127.0.0.1"))
                .expect("应提供回环地址"),
            fingerprint,
        ),
        _ => unreachable!(),
    };
    recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_a
        .send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));

    // 攒够两页多一点：第一页 50 条，剩下的 10 条要往上翻才看得到
    for i in 1..=60 {
        cmds_a
            .send(UiCommand::Send {
                text: format!("第 {i} 句"),
            })
            .unwrap();
    }
    recv_until(
        &events_a,
        |e| matches!(e, UiEvent::Message { text, .. } if text.as_str() == "第 60 句"),
    );

    cmds_b
        .send(UiCommand::Connect {
            addr,
            fingerprint: Some(fingerprint),
        })
        .unwrap();
    recv_until(&events_b, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_b.send(UiCommand::Join { name: "Bob".into() }).unwrap();

    match recv_until(&events_b, |e| matches!(e, UiEvent::History { .. })) {
        UiEvent::History {
            messages, has_more, ..
        } => {
            assert_eq!(messages.len(), 50);
            assert_eq!(messages[0].text, "第 11 句");
            assert!(has_more, "前面还有更早的");
        }
        _ => unreachable!(),
    }

    cmds_b.send(UiCommand::LoadOlder).unwrap();
    match recv_until(&events_b, |e| matches!(e, UiEvent::History { .. })) {
        UiEvent::History {
            messages, has_more, ..
        } => {
            assert_eq!(messages.len(), 10);
            assert_eq!(messages[0].text, "第 1 句");
            assert!(!has_more, "已经到头，不该再说还有");
        }
        _ => unreachable!(),
    }
}

/// 切换房间：旧房间的定时人数上报不能污染新房间。
#[test]
fn switching_rooms_does_not_leak_old_presence() {
    let (_guard, beacon_port) = serial();
    let (cmds_a, events_a) = spawn_test(beacon_port);
    let (cmds_b, events_b) = spawn_test(beacon_port);

    cmds_a
        .send(UiCommand::Host {
            name: "房间A".into(),
        })
        .unwrap();
    let room_a = match recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };
    cmds_a
        .send(UiCommand::Join {
            name: "Alice".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));

    cmds_b
        .send(UiCommand::Host {
            name: "房间B".into(),
        })
        .unwrap();
    let (addr_b, fingerprint_b) =
        match recv_until(&events_b, |e| matches!(e, UiEvent::Hosting { .. })) {
            UiEvent::Hosting {
                addrs, fingerprint, ..
            } => (
                addrs
                    .into_iter()
                    .find(|addr| addr.starts_with("127.0.0.1"))
                    .expect("应提供回环地址"),
                fingerprint,
            ),
            _ => unreachable!(),
        };
    let room_b = match recv_until(&events_b, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };
    assert_ne!(room_a, room_b);

    // A 从自己创建的房间切到 B 的房间
    cmds_a
        .send(UiCommand::Connect {
            addr: addr_b,
            fingerprint: Some(fingerprint_b),
        })
        .unwrap();
    match recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => assert_eq!(room, room_b, "应已切到 B 的房间"),
        _ => unreachable!(),
    }

    // 观察超过房主上报周期的一段时间：不应再出现旧房间的数据
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if let Ok(UiEvent::Presence { room, .. }) =
            events_a.recv_timeout(Duration::from_millis(200))
        {
            assert_eq!(room, room_b, "旧房间的人数上报污染了当前房间");
        }
    }
}

/// 通过界面指令发送文件：本机房主自己加入后发文件，再另存回来。
#[test]
fn sends_and_saves_a_file_through_the_service() {
    let (_guard, beacon_port) = serial();
    let dir = test_data_dir(beacon_port);
    let payload: Vec<u8> = (0..70_000u32).map(|i| (i % 253) as u8).collect();
    let source = dir.join("资料.txt");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&source, &payload).unwrap();

    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    cmds.send(UiCommand::Host {
        name: "文件房间".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));

    cmds.send(UiCommand::SendFile { path: source }).unwrap();
    let attachment = match recv_until(&events, |e| {
        matches!(
            e,
            UiEvent::Message {
                attachment: Some(_),
                ..
            }
        )
    }) {
        UiEvent::Message {
            attachment: Some(attachment),
            ..
        } => attachment,
        other => panic!("期望文件消息，实际 {other:?}"),
    };
    assert_eq!(attachment.size, payload.len() as u64);
    assert_eq!(attachment.name, "资料.txt");
    assert_eq!(attachment.mime, "text/plain");

    let target = dir.join("另存为.txt");
    cmds.send(UiCommand::SaveFile {
        id: attachment.id,
        path: target.clone(),
    })
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(saved) = std::fs::read(&target)
            && saved == payload
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "等待附件保存超时，目标文件未写入"
        );
        std::thread::sleep(Duration::from_millis(50));
        // 同时把事件消化掉，避免通道堆满
        let _ = events.recv_timeout(Duration::from_millis(10));
    }
}
