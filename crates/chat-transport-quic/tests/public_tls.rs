//! 公网模式：域名连接 + 受信 CA 校验。用自建 CA 签发的证书走真实 QUIC 握手。

use std::sync::Arc;

use chat_core::{Client, ConnectionAcceptor, ServerEvent};
use chat_server::{Acceptor, Server};
use chat_transport_quic::{connect_to, Liveness, PublicCa, QuicServer, ServerIdentity};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};

/// 一套测试用 PKI：CA 证书与指定主机名的叶证书。
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

    let leaf_params = CertificateParams::new(
        names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

    Pki {
        ca_pem: ca_cert.pem(),
        cert_pem: leaf.pem(),
        key_pem: leaf_key.serialize_pem(),
    }
}

/// 启动只监听回环地址的 QUIC 服务端。返回端口与房间。
async fn start(pki: &Pki) -> (Arc<Server>, u16) {
    let server = Server::new("公网房间");
    let quic = Arc::new(
        QuicServer::bind_with_pem(
            "127.0.0.1:0".parse().unwrap(),
            Liveness::default(),
            pki.cert_pem.as_bytes(),
            pki.key_pem.as_bytes(),
        )
        .expect("服务端证书必须可用"),
    );
    let port = quic.local_addr().port();
    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&quic).serve(acceptor));
    (server, port)
}

fn ca_for(pki: &Pki, server_name: &str) -> ServerIdentity {
    ServerIdentity::PublicCa(
        PublicCa {
            server_name: server_name.to_string(),
            roots_pem: None,
        }
        .with_roots_pem(pki.ca_pem.as_bytes().to_vec()),
    )
}

#[tokio::test]
async fn trusted_ca_connects_and_reads_lobby() {
    let pki = issue(&["chat.test"]);
    let (server, port) = start(&pki).await;

    let conn = connect_to(
        "127.0.0.1",
        port,
        &ca_for(&pki, "chat.test"),
        Liveness::default(),
    )
    .await
    .expect("受信 CA 签发的证书必须能连上");
    assert_eq!(conn.room_info().room, server.room());

    let client = Client::new(Arc::new(conn));
    client.lobby(server.room()).await.unwrap();
    match tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
        .await
        .expect("等待大厅摘要超时")
        .expect("连接已关闭")
    {
        ServerEvent::Lobby { info } => {
            assert_eq!(info.name, "公网房间");
            assert_eq!(info.online_count, 0);
        }
        other => panic!("期望大厅摘要，实际收到 {other:?}"),
    }
}

#[tokio::test]
async fn certificate_from_an_untrusted_ca_is_rejected() {
    let pki = issue(&["chat.test"]);
    let (_, port) = start(&pki).await;

    // 不给额外根证书：内置公共根不会信任这套测试 CA
    let result = connect_to(
        "127.0.0.1",
        port,
        &ServerIdentity::public_ca("chat.test"),
        Liveness::default(),
    )
    .await;
    assert!(result.is_err(), "不受信 CA 签发的证书必须拒绝连接");
}

#[tokio::test]
async fn certificate_for_another_host_name_is_rejected() {
    let pki = issue(&["chat.test"]);
    let (_, port) = start(&pki).await;

    let result = connect_to(
        "127.0.0.1",
        port,
        &ca_for(&pki, "other.test"),
        Liveness::default(),
    )
    .await;
    assert!(result.is_err(), "主机名不匹配时必须拒绝连接");
}

#[tokio::test]
async fn host_names_are_resolved_and_addresses_tried_until_one_answers() {
    let pki = issue(&["localhost"]);
    let (_, port) = start(&pki).await;

    // localhost 通常同时有 ::1 与 127.0.0.1，而服务端只监听 IPv4：
    // 能连上就说明解析后确实逐个尝试了地址。
    let conn = connect_to(
        "localhost",
        port,
        &ca_for(&pki, "localhost"),
        Liveness::default(),
    )
    .await
    .expect("域名应解析出可连的地址");
    assert_eq!(conn.room_info().name, "公网房间");
}

#[test]
fn invalid_pem_is_reported_instead_of_panicking() {
    let err = match QuicServer::bind_with_pem(
        "127.0.0.1:0".parse().unwrap(),
        Liveness::default(),
        b"not a certificate",
        b"not a key",
    ) {
        Ok(_) => panic!("无效 PEM 必须报错"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("证书"),
        "错误信息应指明证书问题: {err}"
    );
}

#[test]
fn public_ca_identity_carries_its_own_roots() {
    let pki = issue(&["chat.test"]);
    let identity = ca_for(&pki, "chat.test");
    assert_eq!(
        identity,
        ServerIdentity::PublicCa(PublicCa {
            server_name: "chat.test".into(),
            roots_pem: Some(pki.ca_pem.as_bytes().to_vec()),
        })
    );
}
