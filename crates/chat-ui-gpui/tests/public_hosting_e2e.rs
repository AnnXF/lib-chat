//! 界面内的公网托管：自签/受信证书、自动接入、错误反馈与重启延续。

mod support;

use std::path::PathBuf;
use std::time::Duration;

use chat_transport_quic::Liveness;
use chat_ui_gpui::service::{PublicHosting, UiCommand, UiEvent, spawn_with_data};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use support::{recv_until, serial, test_data_dir};

const BASE_PORT: u16 = 53_400;

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

/// 测试期间让公网连接信任这套私有 CA，用完恢复。
struct CaEnv;

impl Drop for CaEnv {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("LIBCHAT_CA_FILE") };
    }
}

fn trust_ca(path: &std::path::Path) -> CaEnv {
    unsafe { std::env::set_var("LIBCHAT_CA_FILE", path) };
    CaEnv
}

fn read_hosting(event: UiEvent) -> (PublicHosting, String) {
    match event {
        UiEvent::Hosting {
            public: Some(public),
            fingerprint,
            ..
        } => (public, fingerprint),
        other => panic!("期望公网托管事件，实际 {other:?}"),
    }
}

#[test]
fn hosts_a_self_signed_public_room_and_accepts_a_second_client() {
    let (_guard, beacon_port) = serial(BASE_PORT);
    let dir = test_data_dir(beacon_port);
    let guest_dir = test_data_dir(beacon_port);

    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    cmds.send(UiCommand::HostPublic {
        name: "界面公网房间".into(),
        port: 0,
        cert: None,
        key: None,
        domain: Some("127.0.0.1".into()),
    })
    .unwrap();

    let (public, fingerprint) = read_hosting(recv_until(&events, |e| {
        matches!(
            e,
            UiEvent::Hosting {
                public: Some(_),
                ..
            }
        )
    }));
    assert!(!public.trusted_ca, "未给证书时应自签");
    assert_eq!(public.domain.as_deref(), Some("127.0.0.1"));

    // 填了域名，本机以客户端身份自动接入
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));

    // 第二个客户端按指纹连进来
    let (guest, guest_events) =
        spawn_with_data(beacon_port, Liveness::default(), guest_dir.clone());
    let addr = format!("127.0.0.1:{}", public.port);
    guest
        .send(UiCommand::Connect {
            addr: addr.clone(),
            fingerprint: Some(fingerprint.clone()),
        })
        .unwrap();
    recv_until(&guest_events, |e| matches!(e, UiEvent::Lobby { .. }));
    guest.send(UiCommand::Join { name: "Bob".into() }).unwrap();
    recv_until(&guest_events, |e| matches!(e, UiEvent::Joined { .. }));

    guest
        .send(UiCommand::Send {
            text: "从第二个客户端发来".into(),
            quote: None,
        })
        .unwrap();
    match recv_until(
        &events,
        |e| matches!(e, UiEvent::Message { text, .. } if text == "从第二个客户端发来"),
    ) {
        UiEvent::Message { sender, .. } => assert_eq!(sender, "Bob"),
        _ => unreachable!(),
    }
}

#[test]
fn hosts_a_public_room_with_a_trusted_certificate() {
    let (_guard, beacon_port) = serial(BASE_PORT);
    let dir = test_data_dir(beacon_port);
    std::fs::create_dir_all(&dir).unwrap();
    let guest_dir = test_data_dir(beacon_port);

    let pki = issue(&["127.0.0.1"]);
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let ca = dir.join("ca.pem");
    std::fs::write(&cert, &pki.cert_pem).unwrap();
    std::fs::write(&key, &pki.key_pem).unwrap();
    std::fs::write(&ca, &pki.ca_pem).unwrap();
    let _ca = trust_ca(&ca);

    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    cmds.send(UiCommand::HostPublic {
        name: "受信证书房间".into(),
        port: 0,
        cert: Some(cert),
        key: Some(key),
        domain: Some("127.0.0.1".into()),
    })
    .unwrap();

    let (public, _) = read_hosting(recv_until(&events, |e| {
        matches!(
            e,
            UiEvent::Hosting {
                public: Some(_),
                ..
            }
        )
    }));
    assert!(public.trusted_ca, "给了证书应按受信 CA 展示");

    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));

    // 不给指纹，只靠 CA 链与主机名校验
    let (guest, guest_events) = spawn_with_data(beacon_port, Liveness::default(), guest_dir);
    guest
        .send(UiCommand::Connect {
            addr: format!("127.0.0.1:{}", public.port),
            fingerprint: None,
        })
        .unwrap();
    match recv_until(&guest_events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { name, .. } => assert_eq!(name, "受信证书房间"),
        _ => unreachable!(),
    }
}

#[test]
fn reports_bad_certificate_inputs_and_port_conflicts() {
    let (_guard, beacon_port) = serial(BASE_PORT);
    let dir = test_data_dir(beacon_port);

    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    // 只给证书不给私钥：应该直接报错，而不是起一个半成品服务
    cmds.send(UiCommand::HostPublic {
        name: "坏证书".into(),
        port: 0,
        cert: Some(PathBuf::from("/tmp/does-not-exist-cert.pem")),
        key: None,
        domain: None,
    })
    .unwrap();
    recv_until(
        &events,
        |e| matches!(e, UiEvent::Error(message) if message.contains("同时给出")),
    );

    // 证书文件不存在
    cmds.send(UiCommand::HostPublic {
        name: "坏证书".into(),
        port: 0,
        cert: Some(PathBuf::from("/tmp/does-not-exist-cert.pem")),
        key: Some(PathBuf::from("/tmp/does-not-exist-key.pem")),
        domain: None,
    })
    .unwrap();
    recv_until(
        &events,
        |e| matches!(e, UiEvent::Error(message) if message.contains("读取证书")),
    );

    // 先起一个自签服务，再让另一个后端抢同一个端口
    cmds.send(UiCommand::HostPublic {
        name: "占端口".into(),
        port: 0,
        cert: None,
        key: None,
        domain: None,
    })
    .unwrap();
    let (public, _) = read_hosting(recv_until(&events, |e| {
        matches!(
            e,
            UiEvent::Hosting {
                public: Some(_),
                ..
            }
        )
    }));

    let other_dir = test_data_dir(beacon_port);
    let (other, other_events) = spawn_with_data(beacon_port, Liveness::default(), other_dir);
    other
        .send(UiCommand::HostPublic {
            name: "抢端口".into(),
            port: public.port,
            cert: None,
            key: None,
            domain: None,
        })
        .unwrap();
    recv_until(
        &other_events,
        |e| matches!(e, UiEvent::Error(message) if message.contains("监听端口")),
    );
}

#[test]
fn public_room_identity_survives_restart() {
    let (_guard, beacon_port) = serial(BASE_PORT);
    let dir = test_data_dir(beacon_port);

    let host_once = |dir: &std::path::Path| {
        let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.to_path_buf());
        cmds.send(UiCommand::HostPublic {
            name: "持久公网房间".into(),
            port: 0,
            cert: None,
            key: None,
            domain: Some("127.0.0.1".into()),
        })
        .unwrap();
        let (public, fingerprint) = read_hosting(recv_until(&events, |e| {
            matches!(
                e,
                UiEvent::Hosting {
                    public: Some(_),
                    ..
                }
            )
        }));
        let room = match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
            UiEvent::Lobby { room, .. } => room,
            _ => unreachable!(),
        };
        drop(cmds);
        drop(events);
        (room, fingerprint, public.port)
    };

    let first = host_once(&dir);
    std::thread::sleep(Duration::from_millis(800));
    let second = host_once(&dir);

    assert_eq!(first.0, second.0, "同一数据目录应还原成同一房间");
    assert_eq!(first.1, second.1, "已有消息库时不能换掉自签证书身份");
    assert_ne!(first.2, second.2, "端口 0 会重新分配，不用于身份判断");
}
