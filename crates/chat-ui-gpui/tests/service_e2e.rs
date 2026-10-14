//! 界面服务链路：创建房间 → 连接 → 看人数 → 加入 → 发言。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chat_proto::RoomId;
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
    let UiEvent::Hosting { addrs, .. } =
        recv_until(&events, |e| matches!(e, UiEvent::Hosting { .. }))
    else {
        unreachable!()
    };
    let addr = addrs
        .iter()
        .find(|a| a.starts_with("127.0.0.1"))
        .expect("应提供回环地址便于单机自测")
        .clone();

    cmds.send(UiCommand::Connect {
        addr,
        fingerprint: None,
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
    assert!(!fingerprint.is_empty(), "应带证书指纹以便 TOFU 校验");

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
    let addr_a = match recv_until(&events_b, |e| matches!(e, UiEvent::Hosting { .. })) {
        UiEvent::Hosting { addrs, .. } => addrs
            .into_iter()
            .find(|addr| addr.starts_with("127.0.0.1"))
            .expect("应提供回环地址"),
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
            fingerprint: None,
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
    let addr = match recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. })) {
        UiEvent::Hosting { addrs, .. } => addrs
            .into_iter()
            .find(|addr| addr.starts_with("127.0.0.1"))
            .expect("应提供回环地址"),
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
            fingerprint: None,
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
    let addr = match recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. })) {
        UiEvent::Hosting { addrs, .. } => addrs
            .into_iter()
            .find(|addr| addr.starts_with("127.0.0.1"))
            .expect("应提供回环地址"),
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
            fingerprint: None,
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
    let addr = match recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. })) {
        UiEvent::Hosting { addrs, .. } => addrs
            .into_iter()
            .find(|addr| addr.starts_with("127.0.0.1"))
            .expect("应提供回环地址"),
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
    recv_until(&events_a, |e| {
        matches!(e, UiEvent::Message { text, .. } if text.as_str() == "第 60 句")
    });

    cmds_b
        .send(UiCommand::Connect {
            addr,
            fingerprint: None,
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
    let addr_b = match recv_until(&events_b, |e| matches!(e, UiEvent::Hosting { .. })) {
        UiEvent::Hosting { addrs, .. } => addrs
            .into_iter()
            .find(|addr| addr.starts_with("127.0.0.1"))
            .expect("应提供回环地址"),
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
            fingerprint: None,
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
