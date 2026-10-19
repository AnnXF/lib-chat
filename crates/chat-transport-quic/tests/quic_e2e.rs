//! 通过真实 QUIC 连接验证消息传输和证书指纹匹配。

use std::sync::Arc;
use std::time::Duration;

use chat_core::{
    guess_mime, sha256_hex, Attachment, AttachmentId, Client, ConnectionAcceptor, ServerEvent,
};
use chat_server::{Acceptor, Server};
use chat_transport_quic::{connect, QuicServer, SelfSignedCert};

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
        connect(addr, &fingerprint).await.expect("A 连接失败"),
    ));
    let b = Client::new(Arc::new(
        connect(addr, &fingerprint).await.expect("B 连接失败"),
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
    let result = connect(addr, &wrong).await;
    assert!(result.is_err(), "证书指纹不匹配时必须拒绝连接");
}

#[test]
fn identity_survives_restart_and_missing_identity_is_rejected() {
    let dir = std::env::temp_dir().join(format!("libchat-cert-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("room.identity");
    let first = SelfSignedCert::load_or_create(&path, true).unwrap();
    let original = first.fingerprint().to_string();
    let second = SelfSignedCert::load_or_create(&path, false).unwrap();
    assert_eq!(second.fingerprint(), original);
    std::fs::remove_file(&path).unwrap();
    assert!(SelfSignedCert::load_or_create(&path, false).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn transfers_attachment_over_dedicated_stream() {
    let (server, _quic, addr, fingerprint) = start_server().await;
    let room = server.room();

    let sender = Client::new(Arc::new(
        connect(addr, &fingerprint).await.expect("发送端连接失败"),
    ));
    let receiver = Client::new(Arc::new(
        connect(addr, &fingerprint).await.expect("接收端连接失败"),
    ));
    for (client, name) in [(&sender, "Alice"), (&receiver, "Bob")] {
        client.join(room, name).await.unwrap();
        wait_for(client, |e| matches!(e, ServerEvent::Joined { .. })).await;
    }

    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let attachment = Attachment {
        id: AttachmentId::new(),
        name: "blob.bin".into(),
        size: data.len() as u64,
        mime: guess_mime("blob.bin").into(),
        sha256: sha256_hex(&data),
    };

    sender
        .upload(room, attachment.clone(), data.clone())
        .await
        .expect("上传失败");
    sender
        .send_file(room, uuid::Uuid::new_v4(), attachment.clone())
        .await
        .unwrap();
    match wait_for(&receiver, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => {
            assert_eq!(
                message.attachment.as_ref().map(|a| a.id),
                Some(attachment.id)
            );
        }
        _ => unreachable!(),
    }

    let downloaded = receiver
        .download(room, attachment.id)
        .await
        .expect("下载失败");
    assert_eq!(downloaded, data);
    assert_eq!(sha256_hex(&downloaded), attachment.sha256);
}
