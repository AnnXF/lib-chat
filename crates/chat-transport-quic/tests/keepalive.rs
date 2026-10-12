//! 回归：安静房间里的连接必须靠保活包活下来。
//!
//! 起因是「两个用户都在房间里，突然都被强行弹出」。根因是 quinn 默认
//! `max_idle_timeout = 30s` 且**不发送保活包**，而安静房间不产生任何网络流量
//! （房主的人数定时器只走进程内通道），于是所有连接会在 30 秒后几乎同时被判死。
//!
//! 这里把超时压缩到秒级来复现同一机制，避免测试等 30 秒。

use std::sync::Arc;
use std::time::Duration;

use chat_client::Client;
use chat_core::ConnectionAcceptor;
use chat_server::{Acceptor, Server};
use chat_transport_quic::{Liveness, QuicServer, connect_with};

async fn start_server(liveness: Liveness) -> (Arc<Server>, std::net::SocketAddr, String) {
    let server = Server::new("安静的房间");
    let quic = Arc::new(QuicServer::bind_with("127.0.0.1:0".parse().unwrap(), liveness).unwrap());
    let addr = quic.local_addr();
    let fingerprint = quic.fingerprint().to_string();

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&quic).serve(acceptor));
    (server, addr, fingerprint)
}

/// 吃掉一次查询的应答，确保后续断言拿到的不是残留事件。
async fn eat_reply(client: &Client) {
    let reply = tokio::time::timeout(Duration::from_secs(2), client.next_event()).await;
    assert!(matches!(reply, Ok(Ok(_))), "应收到首次查询的应答: {reply:?}");
}

/// 有保活：沉默远超空闲超时之后，连接依然完整可用。
#[tokio::test]
async fn keep_alive_survives_idle_room() {
    // 1 秒判死 + 250 毫秒一次 PING，沉默 3 秒等价于默认配置下沉默 90 秒
    let liveness = Liveness {
        idle_timeout: Duration::from_secs(1),
        keep_alive_interval: Duration::from_millis(250),
    };
    let (server, addr, fingerprint) = start_server(liveness).await;
    let room = server.room();

    let client = Client::new(Arc::new(
        connect_with(addr, Some(&fingerprint), liveness)
            .await
            .expect("连接失败"),
    ));
    client.lobby(room).await.expect("首次查询失败");
    eat_reply(&client).await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    client
        .lobby(room)
        .await
        .expect("保活生效时，沉默 3 秒后仍应能发出请求");
    let reply = tokio::time::timeout(Duration::from_secs(2), client.next_event()).await;
    assert!(
        matches!(reply, Ok(Ok(_))),
        "保活生效时，沉默 3 秒后仍应收到应答: {reply:?}"
    );
}

/// 无保活：同样的沉默会把连接判死——这正是修复前的线上表现。
#[tokio::test]
async fn without_keep_alive_idle_connection_dies() {
    let liveness = Liveness {
        idle_timeout: Duration::from_secs(1),
        // 远大于空闲超时，等价于「不发保活包」
        keep_alive_interval: Duration::from_secs(60),
    };
    let (server, addr, fingerprint) = start_server(liveness).await;
    let room = server.room();

    let client = Client::new(Arc::new(
        connect_with(addr, Some(&fingerprint), liveness)
            .await
            .expect("连接失败"),
    ));
    client.lobby(room).await.expect("首次查询失败");
    eat_reply(&client).await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    // 对端没有保活，本端也没发保活，连接必然已过空闲超时：事件通道应已关闭
    let alive = tokio::time::timeout(Duration::from_secs(2), client.next_event()).await;
    assert!(
        matches!(alive, Ok(Err(_))),
        "不发保活包时连接应在空闲超时后被判死，实际: {alive:?}"
    );
}
