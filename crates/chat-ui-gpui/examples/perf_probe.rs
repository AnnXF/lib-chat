//! 临时性能取证：把排查中怀疑的几处逐条量出来，用数字确认而不是靠推断。

use std::sync::Arc;
use std::time::{Duration, Instant};

use chat_core::Client;
use chat_core::{ConnectionAcceptor, MessageStore, NewMessage};
use chat_core::{DeviceId, MessageKind, RoomId, UserId};
use chat_server::{Acceptor, InMemoryStore, Server};
use chat_transport_quic::{Liveness, QuicServer};
use chat_ui_gpui::service::{UiCommand, UiEvent};

#[tokio::main]
async fn main() {
    history_cost().await;
    handshake_cost().await;
    unreachable_cost().await;
    probe_rate().await;
    message_footprint().await;
}

/// 怀疑：`history` 先克隆房间内**全部**消息，再截断到最后 `limit` 条。
/// 若属实，耗时应随房间消息总数线性增长，而不是随 `limit` 恒定。
async fn history_cost() {
    println!("── ① 历史查询：只取最近 50 条 ──");
    for count in [1_000usize, 10_000, 50_000, 100_000] {
        let store = InMemoryStore::default();
        let room = RoomId::new();
        for i in 0..count {
            store
                .append(
                    room,
                    NewMessage {
                        client_msg_id: DeviceId::new().0,
                        sender: UserId::new(),
                        sender_name: "某人".into(),
                        kind: MessageKind::Text,
                        text: format!("这是第 {i} 条消息，长度大约二十来个字"),
                    },
                )
                .await
                .unwrap();
        }

        let started = Instant::now();
        let got = store.history(room, None, 50).await.unwrap();
        let elapsed = started.elapsed();
        println!(
            "  房间共 {count:>7} 条 → 耗时 {elapsed:>10.2?}（返回 {} 条）",
            got.len()
        );
    }
}

/// 怀疑：房间探测每次都新建连接（完整 QUIC + TLS 握手），而不是复用。
/// 量出单次探测的成本，再乘以实际发生频率，就是白白烧掉的量。
async fn handshake_cost() {
    println!("── ② 单次房间探测的成本 ──");
    let server = Server::new("取证房间");
    let quic = Arc::new(QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let addr = quic.local_addr();
    let fingerprint = quic.fingerprint().to_string();
    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&quic).serve(acceptor));

    // 先预热一次，排除首次建 socket / 证书校验的固定开销
    {
        let conn = chat_transport_quic::connect(addr, &fingerprint)
            .await
            .unwrap();
        let client = Client::new(Arc::new(conn));
        client.lobby(server.room()).await.unwrap();
        let _ = client.close().await;
    }

    let rounds = 30;
    let started = Instant::now();
    for _ in 0..rounds {
        let conn = chat_transport_quic::connect(addr, &fingerprint)
            .await
            .unwrap();
        let client = Client::new(Arc::new(conn));
        client.lobby(server.room()).await.unwrap();
        let _ = client.close().await;
    }
    let total = started.elapsed();
    println!(
        "  {rounds} 次「连接 → 查大厅 → 关闭」共 {total:.2?}，单次 {:.2?}",
        total / rounds
    );
    quic.close();
}

/// 记录底层建连超时所需的时间。
async fn unreachable_cost() {
    println!("── ④ 底层 connect 的失败耗时（探测路径已由 probe_room 加超时，见回归测试）──");
    // 192.0.2.0/24 是 TEST-NET-1，保证不会有主机应答
    let dead: std::net::SocketAddr = "192.0.2.1:45678".parse().unwrap();
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(25),
        chat_transport_quic::connect(dead, &"0".repeat(64)),
    )
    .await;
    match outcome {
        Ok(Err(e)) => println!("  {:.2?} 后失败: {e}", started.elapsed()),
        Ok(Ok(_)) => println!("  意外连上了"),
        Err(_) => println!("  25 秒仍未失败，建连超时保护可能失效"),
    }
}

/// 量出界面实际发出的探测频率（每次成功探测 = 一次完整握手）。
async fn probe_rate() {
    println!("── ③ 界面实际的探测频率（10 秒实测）──");
    let dir = std::env::temp_dir().join(format!("libchat-perf-{}", uuid::Uuid::new_v4()));
    let (cmds, events) = chat_ui_gpui::service::spawn_with_data(53_300, Liveness::default(), dir);
    cmds.send(UiCommand::Host {
        name: "频率取证".into(),
    })
    .unwrap();

    // 等房间起来并被自己发现
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(UiEvent::RoomFound { .. }) = events.recv_timeout(Duration::from_millis(200)) {
            break;
        }
    }

    let window = Duration::from_secs(10);
    let started = Instant::now();
    let mut probes = 0u32;
    while started.elapsed() < window {
        if let Ok(UiEvent::RoomFound { .. }) = events.recv_timeout(Duration::from_millis(200)) {
            probes += 1;
        }
    }
    println!(
        "  单个房间 {probes} 次 / 10 秒 ≈ {:.2} 次/秒（相当于每 {:.1} 秒一次完整握手）",
        probes as f64 / window.as_secs_f64(),
        window.as_secs_f64() / probes.max(1) as f64
    );
}

/// 怀疑：服务端消息表无上限。量出单条消息的常驻内存，才能算清涨得多快。
async fn message_footprint() {
    println!("── ⑤ 单条消息的常驻内存 ──");
    let store = InMemoryStore::default();
    let room = RoomId::new();
    let count = 100_000usize;

    let before = rss_bytes();
    for i in 0..count {
        store
            .append(
                room,
                NewMessage {
                    client_msg_id: DeviceId::new().0,
                    sender: UserId::new(),
                    sender_name: "某人".into(),
                    kind: MessageKind::Text,
                    text: format!("这是第 {i} 条消息，长度大约二十来个字"),
                },
            )
            .await
            .unwrap();
    }
    let grown = rss_bytes().saturating_sub(before);
    let per = grown as f64 / count as f64;

    println!(
        "  {count} 条消息占用约 {:.1} MB（正文约 40 字节，每条约 {per:.0} 字节）",
        grown as f64 / 1024.0 / 1024.0
    );
    println!(
        "  推算：1 万条 ≈ {:.0} MB，100 万条 ≈ {:.0} MB，且永不释放",
        per * 10_000.0 / 1024.0 / 1024.0,
        per * 1_000_000.0 / 1024.0 / 1024.0
    );
    std::hint::black_box(&store);
}

/// 读当前进程的常驻内存（KB → 字节）。
fn rss_bytes() -> u64 {
    let pid = std::process::id().to_string();
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        * 1024
}
