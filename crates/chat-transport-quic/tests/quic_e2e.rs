//! 通过真实 QUIC 连接跑通 V1 链路，验证传输层与 TOFU 校验。

use std::sync::Arc;
use std::time::Duration;

use chat_client::Client;
use chat_core::ConnectionAcceptor;
use chat_proto::ServerEvent;
use chat_server::{Acceptor, Server};
use chat_transport_quic::{connect, QuicServer};

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

/// 启动一个监听本机随机端口的 QUIC 服务端。
async fn start_server() -> (Arc<Server>, Arc<QuicServer>, std::net::SocketAddr, String) {
    let server = Server::new("局域网聊天室");
    let quic = Arc::new(QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let addr = quic.local_addr();
    let fingerprint = quic.fingerprint().to_string();

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&quic).serve(acceptor));
    (server, quic, addr, fingerprint)
}

#[tokio::test]
async fn remote_clients_chat_over_quic() {
    let (server, _quic, addr, fingerprint) = start_server().await;
    let room = server.room();

    let a = Client::new(Arc::new(
        connect(addr, Some(&fingerprint)).await.expect("A 连接失败"),
    ));
    let b = Client::new(Arc::new(
        connect(addr, Some(&fingerprint)).await.expect("B 连接失败"),
    ));

    // 未加入时只能看到摘要
    b.lobby(room).await.unwrap();
    match wait_for(&b, |e| matches!(e, ServerEvent::Lobby { .. })).await {
        ServerEvent::Lobby { info } => {
            assert_eq!(info.name, "局域网聊天室");
            assert_eq!(info.online_count, 0);
        }
        _ => unreachable!(),
    }

    a.join(room, "Alice").await.unwrap();
    match wait_for(&a, |e| matches!(e, ServerEvent::Joined { .. })).await {
        ServerEvent::Joined { online_count, .. } => assert_eq!(online_count, 1),
        _ => unreachable!(),
    }

    b.join(room, "Bob").await.unwrap();
    wait_for(&b, |e| matches!(e, ServerEvent::Joined { .. })).await;

    a.send_text(room, "你好，QUIC").await.unwrap();
    match wait_for(&b, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => {
            assert_eq!(message.text, "你好，QUIC");
            assert_eq!(message.sender_name, "Alice");
            assert_eq!(message.seq, 1);
        }
        _ => unreachable!(),
    }

    assert_eq!(server.online_count().await, 2);

    a.close().await.unwrap();
    let _ = b.close().await;
}

#[tokio::test]
async fn rejects_mismatched_fingerprint() {
    let (_server, _quic, addr, _fingerprint) = start_server().await;

    let wrong = "00".repeat(32);
    let result = connect(addr, Some(&wrong)).await;
    assert!(result.is_err(), "证书指纹不匹配时必须拒绝连接");
}
