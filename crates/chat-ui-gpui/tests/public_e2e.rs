//! 公网形态的服务链路：受信 CA 证书、无指纹连接、地址簿自动探测、断线重连。

mod support;

use std::sync::Arc;
use std::time::Duration;

use chat_core::ConnectionAcceptor;
use chat_server::{Acceptor, Server};
use chat_transport_quic::{Liveness, QuicServer};
use chat_ui_gpui::service::{UiCommand, UiEvent, spawn_with_data};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use support::{recv_until, recv_until_keeping_room, serial, test_data_dir};

/// 本文件的端口段，避开 service_e2e 的 53100 段。
const BASE_PORT: u16 = 53_200;

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

/// 让公网连接信任这套测试 CA 的 RAII 标记；用完恢复。
struct CaEnv;

impl Drop for CaEnv {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("LIBCHAT_CA_FILE") };
    }
}

/// 一个受信 CA 证书的独立服务端。
struct PublicRoom {
    runtime: tokio::runtime::Runtime,
    server: Arc<Server>,
    port: u16,
    ca_path: std::path::PathBuf,
}

impl PublicRoom {
    fn start(dir: &std::path::Path) -> Self {
        std::fs::create_dir_all(dir).unwrap();
        let pki = issue(&["127.0.0.1"]);
        let ca_path = dir.join("ca.pem");
        std::fs::write(&ca_path, &pki.ca_pem).unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .unwrap();
        let (server, port) = runtime.block_on(async {
            let server = Server::new("公网房间");
            let quic = Arc::new(
                QuicServer::bind_with_pem(
                    "127.0.0.1:0".parse().unwrap(),
                    Liveness::default(),
                    pki.cert_pem.as_bytes(),
                    pki.key_pem.as_bytes(),
                )
                .unwrap(),
            );
            let port = quic.local_addr().port();
            let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
            tokio::spawn(Arc::clone(&quic).serve(acceptor));
            (server, port)
        });

        Self {
            runtime,
            server,
            port,
            ca_path,
        }
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn online(&self) -> u32 {
        self.runtime.block_on(self.server.online_count())
    }

    /// 让它当作用户目录里的私有 CA。
    fn trust_ca(&self) -> CaEnv {
        unsafe { std::env::set_var("LIBCHAT_CA_FILE", &self.ca_path) };
        CaEnv
    }
}

#[test]
fn connects_to_a_public_server_without_a_fingerprint() {
    let (_guard, beacon_port) = serial(BASE_PORT);
    let dir = test_data_dir(beacon_port);
    let public = PublicRoom::start(&dir);
    let _ca = public.trust_ca();

    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    cmds.send(UiCommand::Connect {
        addr: public.addr(),
        fingerprint: None,
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { name, online, .. } => {
            assert_eq!(name, "公网房间");
            assert_eq!(online, 0);
        }
        _ => unreachable!(),
    }

    // 连上过的地址自动进地址簿，下次启动不必再手输
    let book = dir.join("servers.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(saved) = std::fs::read_to_string(&book)
            && saved.contains(&public.addr())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "地址簿应记下公网服务器"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Joined { .. })) {
        UiEvent::Joined { online, .. } => assert_eq!(online, 1),
        _ => unreachable!(),
    }
    assert_eq!(public.online(), 1);

    cmds.send(UiCommand::Send {
        text: "公网你好".into(),
        quote: None,
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Message { .. })) {
        UiEvent::Message { text, .. } => assert_eq!(text, "公网你好"),
        _ => unreachable!(),
    }

    // 退出房间和局域网一样：人数立刻回落，不留在房间里
    cmds.send(UiCommand::Leave).unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Left { .. }));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while public.online() != 0 {
        assert!(std::time::Instant::now() < deadline, "退出后在线人数应回落");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn saved_public_server_is_listed_after_restart() {
    let (_guard, beacon_port) = serial(BASE_PORT);
    let dir = test_data_dir(beacon_port);
    let public = PublicRoom::start(&dir);
    let _ca = public.trust_ca();

    // 第一次：手动连一次，地址写入地址簿
    {
        let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
        cmds.send(UiCommand::Connect {
            addr: public.addr(),
            fingerprint: None,
        })
        .unwrap();
        recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
        drop(cmds);
        drop(events);
    }
    std::thread::sleep(Duration::from_millis(600));

    // 重启后：地址簿自动探测出房间，侧栏按房间列出
    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    let (room, addr) = match recv_until(&events, |e| matches!(e, UiEvent::RemoteRoom { .. })) {
        UiEvent::RemoteRoom {
            room,
            name,
            addr,
            online,
        } => {
            assert_eq!(name, "公网房间");
            assert_eq!(online, 0);
            (room, addr)
        }
        _ => unreachable!(),
    };
    assert_eq!(addr, public.addr());

    cmds.send(UiCommand::ConnectDiscovered {
        room,
        addr,
        fingerprint: None,
    })
    .unwrap();
    match recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. })) {
        UiEvent::Lobby { room: joined, .. } => assert_eq!(joined, room),
        _ => unreachable!(),
    }
}

#[test]
fn reconnects_to_a_public_server_without_leaving_the_room() {
    let (_guard, beacon_port) = serial(BASE_PORT);
    let dir = test_data_dir(beacon_port);
    let public = PublicRoom::start(&dir);
    let _ca = public.trust_ca();

    let flaky = Liveness {
        idle_timeout: Duration::from_secs(1),
        keep_alive_interval: Duration::from_secs(60),
    };
    let (cmds, events) = spawn_with_data(beacon_port, flaky, dir.clone());
    cmds.send(UiCommand::Connect {
        addr: public.addr(),
        fingerprint: None,
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Lobby { .. }));
    cmds.send(UiCommand::Join {
        name: "Alice".into(),
    })
    .unwrap();
    recv_until(&events, |e| matches!(e, UiEvent::Joined { .. }));

    recv_until_keeping_room(&events, |e| matches!(e, UiEvent::Reconnecting(_)));
    recv_until_keeping_room(&events, |e| matches!(e, UiEvent::Joined { .. }));

    cmds.send(UiCommand::Send {
        text: "掉线之后还能说话".into(),
        quote: None,
    })
    .unwrap();
    match recv_until_keeping_room(&events, |e| matches!(e, UiEvent::Message { .. })) {
        UiEvent::Message { text, .. } => assert_eq!(text, "掉线之后还能说话"),
        _ => unreachable!(),
    }
}

#[test]
fn transfers_files_through_a_public_server() {
    let (_guard, beacon_port) = serial(BASE_PORT);
    let dir = test_data_dir(beacon_port);
    let public = PublicRoom::start(&dir);
    let _ca = public.trust_ca();

    let payload: Vec<u8> = (0..80_000u32).map(|i| (i % 251) as u8).collect();
    let source = dir.join("公网资料.bin");
    std::fs::write(&source, &payload).unwrap();

    let (cmds, events) = spawn_with_data(beacon_port, Liveness::default(), dir.clone());
    cmds.send(UiCommand::Connect {
        addr: public.addr(),
        fingerprint: None,
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

    let target = dir.join("另存为.bin");
    cmds.send(UiCommand::SaveFile {
        id: attachment.id,
        path: target.clone(),
    })
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(saved) = std::fs::read(&target)
            && saved == payload
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "等待附件保存超时");
        std::thread::sleep(Duration::from_millis(50));
        let _ = events.recv_timeout(Duration::from_millis(10));
    }
}
