//! 界面后端链路：创建房间 → 连接 → 看人数 → 加入 → 发言。

use std::time::{Duration, Instant};

use chat_ui_gpui::backend::{UiCommand, UiEvent, spawn};

/// 这些用例都要占用全局的 mDNS(5353) 与广播(52100) 端口，必须串行执行。
///
/// 广播报文在多个同端口套接字之间会被分流，因此除了互斥，还要等上一轮的后台线程
/// 真正退出（应答线程最多 400ms 才检查退出标志）才能开始下一轮。
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::thread::sleep(Duration::from_millis(800));
    guard
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
    let _guard = serial();
    let (cmds, events) = spawn();

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
        UiEvent::Lobby {
            name,
            online,
            members,
            ..
        } => {
            assert_eq!(name, "测试房间");
            assert_eq!(online, 0);
            assert_eq!(members, 0);
        }
        _ => unreachable!(),
    }

    cmds.send(UiCommand::Join { name: "Alice".into() }).unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Joined { .. })) {
        UiEvent::Joined { members, .. } => assert_eq!(members, 1),
        _ => unreachable!(),
    }

    cmds.send(UiCommand::Send { text: "你好".into() }).unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Message { .. })) {
        UiEvent::Message { sender, text, .. } => {
            assert_eq!(sender, "Alice");
            assert_eq!(text, "你好");
        }
        _ => unreachable!(),
    }

    cmds.send(UiCommand::Leave).unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Left { .. }));
}

/// 同一台机器上跑两个实例：实例 B 应能自动发现实例 A 创建的房间并聊天。
#[test]
fn two_instances_discover_each_other_on_one_host() {
    let _guard = serial();
    let (cmds_a, events_a) = spawn();
    let (cmds_b, events_b) = spawn();

    // 实例 A 创建房间并加入
    cmds_a
        .send(UiCommand::Host {
            name: "同机房间".into(),
        })
        .unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Hosting { .. }));
    recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds_a.send(UiCommand::Join { name: "Alice".into() }).unwrap();
    recv_until(&events_a, |e| matches!(e, UiEvent::Joined { .. }));

    // 实例 B 无需任何手工配置即可发现该房间
    let found = recv_until(&events_b, |e| {
        matches!(e, UiEvent::RoomFound { name, .. } if name == "同机房间")
    });
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

    // A 应看到 2 人在线、2 个成员，不能把同一个人重复计数
    match recv_until(&events_a, |e| {
        matches!(e, UiEvent::Presence { online: 2, .. })
    }) {
        UiEvent::Presence {
            online, members, ..
        } => {
            assert_eq!(online, 2);
            assert_eq!(members, 2);
        }
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

/// 同一房间会经 mDNS 与 UDP 广播重复播报，且地址可能不止一个；
/// 上报必须始终带同一个房间标识，界面才能去重成一条。
#[test]
fn same_room_is_reported_under_one_id() {
    let _guard = serial();
    let (cmds, events) = spawn();
    cmds.send(UiCommand::Host {
        name: "唯一房间".into(),
    })
    .unwrap();
    let room = match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };

    let mut seen = 0;
    let deadline = Instant::now() + Duration::from_secs(7);
    while Instant::now() < deadline {
        if let Ok(UiEvent::RoomFound {
            room: found, addr, ..
        }) = events.recv_timeout(Duration::from_millis(200))
        {
            assert_eq!(found, room, "同一房间必须使用同一标识");
            assert!(!addr.is_empty());
            seen += 1;
        }
    }
    assert!(seen > 0, "应至少发现一次自己的房间");
}

/// 切换房间：旧房间的定时人数上报不能污染新房间。
#[test]
fn switching_rooms_does_not_leak_old_presence() {
    let _guard = serial();
    let (cmds_a, events_a) = spawn();
    let (cmds_b, events_b) = spawn();

    cmds_a
        .send(UiCommand::Host {
            name: "房间A".into(),
        })
        .unwrap();
    let room_a = match recv_until(&events_a, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room, .. } => room,
        _ => unreachable!(),
    };
    cmds_a.send(UiCommand::Join { name: "Alice".into() }).unwrap();
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
