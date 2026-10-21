//! 界面后端的账号链路：连接 Hub → 注册/登录 → 会话列表 → 默认大厅收发。

use std::sync::Arc;
use std::time::Duration;

use chat_core::ConnectionAcceptor;
use chat_server::{Hub, HubAcceptor};
use chat_transport_quic::{Liveness, QuicServer};
use chat_ui_gpui::service::{UiCommand, UiEvent, spawn_with_data};

fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("libchat-hub-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn recv_until<F>(events: &flume::Receiver<UiEvent>, pred: F) -> UiEvent
where
    F: Fn(&UiEvent) -> bool,
{
    for _ in 0..128 {
        let event = events
            .recv_timeout(Duration::from_secs(3))
            .expect("等待界面事件超时");
        if pred(&event) {
            return event;
        }
    }
    panic!("未收到期望事件");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_list_rooms_and_chat_through_hub() {
    let dir = temp_dir();
    let hub = Hub::open(&dir.join("server"), "测试大厅").await.unwrap();
    let quic = Arc::new(QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(HubAcceptor(Arc::clone(&hub)));
    tokio::spawn(Arc::clone(&quic).serve(acceptor));

    let (cmds, events) = spawn_with_data(0, Liveness::default(), dir.join("client"));

    let addr = format!("127.0.0.1:{}", quic.local_addr().port());
    cmds.send(UiCommand::Login {
        addr,
        fingerprint: Some(quic.fingerprint().to_string()),
        username: "alice".into(),
        password: "password123".into(),
        register: true,
    })
    .unwrap();

    // Lobby 在 Authenticated 之前发出，先等它，免得被后一次等待丢弃
    recv_until(&events, |event| matches!(event, UiEvent::Lobby { .. }));

    let user = match recv_until(&events, |event| {
        matches!(event, UiEvent::Authenticated { .. })
    }) {
        UiEvent::Authenticated { user, .. } => user,
        _ => unreachable!(),
    };
    assert_eq!(user.username, "alice");

    let rooms = match recv_until(&events, |event| {
        matches!(event, UiEvent::Conversations { .. })
    }) {
        UiEvent::Conversations { rooms } => rooms,
        _ => unreachable!(),
    };
    assert!(
        rooms.iter().any(|room| room.room == hub.default_room()),
        "登录后应能看到默认大厅"
    );

    cmds.send(UiCommand::Send {
        text: "登录后的第一条".into(),
        quote: None,
    })
    .unwrap();
    let message = match recv_until(&events, |event| matches!(event, UiEvent::Message { .. })) {
        UiEvent::Message { text, sender, .. } => (text, sender),
        _ => unreachable!(),
    };
    assert_eq!(message.0, "登录后的第一条");
    assert_eq!(message.1, "alice");

    let _ = std::fs::remove_dir_all(dir);
}
