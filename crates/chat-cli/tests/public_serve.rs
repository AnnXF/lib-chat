//! 独立服务端（公网形态）：真实 QUIC、受信 CA、重启后房间与历史延续。

use std::sync::Arc;
use std::time::Duration;

use chat_cli::{serve, ServeConfig, ServerTls};
use chat_core::{guess_mime, sha256_hex, Attachment, AttachmentId, Client, ServerEvent};
use chat_transport_quic::{connect_to, Liveness, PublicCa, ServerIdentity};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};

struct Pki {
    ca_pem: String,
    cert_pem: String,
    key_pem: String,
}

fn issue(names: &[&str]) -> Pki {
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();

    let leaf_params =
        CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = leaf_params
        .signed_by(&leaf_key, &Issuer::from_params(&ca_params, &ca_key))
        .unwrap();

    Pki {
        ca_pem: ca_cert.pem(),
        cert_pem: leaf.pem(),
        key_pem: leaf_key.serialize_pem(),
    }
}

fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("libchat-serve-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn identity(pki: &Pki, server_name: &str) -> ServerIdentity {
    ServerIdentity::PublicCa(
        PublicCa {
            server_name: server_name.into(),
            roots_pem: None,
        }
        .with_roots_pem(pki.ca_pem.as_bytes().to_vec()),
    )
}

async fn wait_for<F>(client: &Client, pred: F) -> ServerEvent
where
    F: Fn(&ServerEvent) -> bool,
{
    for _ in 0..32 {
        let event = tokio::time::timeout(Duration::from_secs(3), client.next_event())
            .await
            .expect("等待事件超时")
            .expect("连接已关闭");
        if pred(&event) {
            return event;
        }
    }
    panic!("未收到期望事件");
}

async fn join(client: &Client, room: chat_core::RoomId, name: &str) {
    client.join(room, name).await.unwrap();
    wait_for(client, |e| matches!(e, ServerEvent::Joined { .. })).await;
}

#[tokio::test]
async fn public_server_keeps_room_and_history_across_restarts() {
    let dir = temp_dir();
    let pki = issue(&["localhost"]);
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, &pki.cert_pem).unwrap();
    std::fs::write(&key, &pki.key_pem).unwrap();

    let config = || ServeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        name: "公网房间".into(),
        data_dir: dir.clone(),
        tls: ServerTls::Pem {
            cert: cert.clone(),
            key: key.clone(),
        },
        announce_port: None,
    };

    let first = serve(config()).await.expect("公网服务端启动失败");
    let room = first.room;
    assert_eq!(first.addr.ip().to_string(), "127.0.0.1");

    let conn = connect_to(
        "127.0.0.1",
        first.addr.port(),
        &identity(&pki, "localhost"),
        Liveness::default(),
    )
    .await
    .expect("受信 CA 必须能连上公网服务端");
    assert_eq!(conn.room_info().room, room);
    let client = Client::new(Arc::new(conn));
    client.lobby(room).await.unwrap();
    match wait_for(&client, |e| matches!(e, ServerEvent::Lobby { .. })).await {
        ServerEvent::Lobby { info } => {
            assert_eq!(info.name, "公网房间");
            assert_eq!(info.online_count, 0);
        }
        _ => unreachable!(),
    }

    join(&client, room, "Alice").await;
    client.send_text(room, "公网第一条").await.unwrap();
    wait_for(&client, |e| matches!(e, ServerEvent::Message { .. })).await;
    drop(client);
    first.shutdown().await;

    let second = serve(config()).await.expect("重启服务端失败");
    assert_eq!(second.room, room, "同一数据目录必须还是同一个房间");

    let client = Client::new(Arc::new(
        connect_to(
            "127.0.0.1",
            second.addr.port(),
            &identity(&pki, "localhost"),
            Liveness::default(),
        )
        .await
        .expect("重启后必须还能连上"),
    ));
    join(&client, room, "Bob").await;
    client.history(room, None, 50).await.unwrap();
    match wait_for(&client, |e| matches!(e, ServerEvent::History { .. })).await {
        ServerEvent::History { messages, .. } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].text, "公网第一条");
            assert_eq!(messages[0].sender_name, "Alice");
        }
        _ => unreachable!(),
    }

    let _ = client.close().await;
    second.shutdown().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn public_server_transfers_attachments_and_keeps_them_after_restart() {
    let dir = temp_dir();
    let pki = issue(&["localhost"]);
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, &pki.cert_pem).unwrap();
    std::fs::write(&key, &pki.key_pem).unwrap();

    let config = || ServeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        name: "文件房间".into(),
        data_dir: dir.clone(),
        tls: ServerTls::Pem {
            cert: cert.clone(),
            key: key.clone(),
        },
        announce_port: None,
    };

    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
    let attachment = Attachment {
        id: AttachmentId::new(),
        name: "photo.bin".into(),
        size: data.len() as u64,
        mime: guess_mime("photo.bin").into(),
        sha256: sha256_hex(&data),
    };

    let first = serve(config()).await.unwrap();
    let room = first.room;
    let sender = Client::new(Arc::new(
        connect_to(
            "127.0.0.1",
            first.addr.port(),
            &identity(&pki, "localhost"),
            Liveness::default(),
        )
        .await
        .unwrap(),
    ));
    join(&sender, room, "Alice").await;
    sender
        .upload(room, attachment.clone(), data.clone())
        .await
        .expect("上传失败");
    sender
        .send_file(room, uuid::Uuid::new_v4(), attachment.clone())
        .await
        .unwrap();
    wait_for(&sender, |e| matches!(e, ServerEvent::Message { .. })).await;
    let _ = sender.close().await;
    first.shutdown().await;

    let second = serve(config()).await.unwrap();
    assert_eq!(second.room, room);
    let receiver = Client::new(Arc::new(
        connect_to(
            "127.0.0.1",
            second.addr.port(),
            &identity(&pki, "localhost"),
            Liveness::default(),
        )
        .await
        .unwrap(),
    ));
    join(&receiver, room, "Bob").await;
    let downloaded = receiver
        .download(room, attachment.id)
        .await
        .expect("重启后附件仍应可取");
    assert_eq!(downloaded, data);

    let _ = receiver.close().await;
    second.shutdown().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn self_signed_identity_survives_restart() {
    let dir = temp_dir();
    let config = || ServeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        name: "自签房间".into(),
        data_dir: dir.clone(),
        tls: ServerTls::SelfSigned,
        announce_port: None,
    };

    let first = serve(config()).await.unwrap();
    let (room, fingerprint) = (first.room, first.fingerprint.clone());
    first.shutdown().await;

    let second = serve(config()).await.unwrap();
    assert_eq!(second.room, room, "重启后仍是同一个房间");
    assert_eq!(
        second.fingerprint, fingerprint,
        "已有消息库时不能换掉证书身份"
    );
    second.shutdown().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn self_signed_server_works_with_pinned_fingerprint() {
    let dir = temp_dir();
    let serving = serve(ServeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        name: "自签房间".into(),
        data_dir: dir.clone(),
        tls: ServerTls::SelfSigned,
        announce_port: None,
    })
    .await
    .unwrap();

    let client = Client::new(Arc::new(
        connect_to(
            "127.0.0.1",
            serving.addr.port(),
            &ServerIdentity::fingerprint(serving.fingerprint.clone()),
            Liveness::default(),
        )
        .await
        .expect("自签服务端按指纹连接"),
    ));
    join(&client, serving.room, "Alice").await;
    client.send_text(serving.room, "内网").await.unwrap();
    match wait_for(&client, |e| matches!(e, ServerEvent::Message { .. })).await {
        ServerEvent::Message { message } => assert_eq!(message.text, "内网"),
        _ => unreachable!(),
    }

    let _ = client.close().await;
    serving.shutdown().await;
    std::fs::remove_dir_all(dir).unwrap();
}
