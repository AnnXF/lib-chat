//! 后端会话：在独立 tokio 运行时里托管服务端与客户端，通过消息通道与界面交互。
//!
//! 界面线程不直接触碰网络，只发送 [`UiCommand`]、接收 [`UiEvent`]。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chat_client::Client;
use chat_core::{Connection, ConnectionAcceptor};
use chat_discovery::{RoomAnnouncer, RoomBrowser};
use chat_proto::{LobbyInfo, RoomId, ServerEvent, UserId};
use chat_server::{Acceptor, Server};
use chat_transport_quic::{connect, QuicServer};
use flume::{Receiver, Sender};

/// mDNS 房间的复查间隔。
const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// 单次握手与查询的超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// 房主房间的人数上报间隔。
const PRESENCE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub enum UiEvent {
    Status(String),
    Error(String),
    /// 局域网里发现（或刷新到）的房间。同一房间可能经多条通道被播报，按 `room` 去重。
    RoomFound {
        room: RoomId,
        name: String,
        addr: String,
        fingerprint: String,
        online: u32,
        members: u32,
    },
    /// 本机已作为房主启动。
    Hosting {
        name: String,
        addrs: Vec<String>,
        fingerprint: String,
    },
    /// 已连上某个房间，但尚未加入。
    Lobby {
        room: RoomId,
        name: String,
        addr: String,
        online: u32,
        members: u32,
    },
    Joined {
        room: RoomId,
        /// 我在这个房间里的身份，用于区分「我发的」与「别人发的」。
        you: UserId,
        members: usize,
    },
    Left {
        room: RoomId,
    },
    Message {
        room: RoomId,
        sender_id: UserId,
        sender: String,
        text: String,
    },
    Presence {
        room: RoomId,
        online: u32,
        members: u32,
    },
    Disconnected,
}

#[derive(Clone, Debug)]
pub enum UiCommand {
    Host {
        name: String,
    },
    Connect {
        addr: String,
        fingerprint: Option<String>,
    },
    Join {
        name: String,
    },
    Leave,
    Send {
        text: String,
    },
}

/// 启动后端线程，返回指令通道与事件通道。
pub fn spawn() -> (Sender<UiCommand>, Receiver<UiEvent>) {
    let (cmd_tx, cmd_rx) = flume::unbounded();
    let (ev_tx, ev_rx) = flume::unbounded();

    std::thread::Builder::new()
        .name("chat-backend".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
                .expect("创建 tokio 运行时失败");
            rt.block_on(run(ev_tx, cmd_rx));
        })
        .expect("启动后端线程失败");

    (cmd_tx, ev_rx)
}

/// 一条已建立的会话。
struct Active {
    client: Client,
    room: RoomId,
    pump: tokio::task::JoinHandle<()>,
}

impl Active {
    async fn end(self) {
        self.pump.abort();
        let _ = self.client.close().await;
    }
}

/// 本机托管的房间。切换房间或重新建房时必须回收，否则会留下幽灵房间。
struct Hosting {
    room: RoomId,
    endpoint: Arc<QuicServer>,
    /// 仅用于保活：持有它才能持续广播房间。
    _announcer: RoomAnnouncer,
    /// 定时上报是否终止（重建房/关闭时置 false）。
    alive: Arc<AtomicBool>,
    /// 界面当前是否在展示这个房间；否则上报会污染别的房间的人数。
    visible: Arc<AtomicBool>,
}

impl Hosting {
    fn shutdown(self) {
        self.alive.store(false, Ordering::Relaxed);
        self.visible.store(false, Ordering::Relaxed);
        self.endpoint.close();
        // announcer 随 self 一起析构，撤销 mDNS 注册并停止广播应答
    }
}

async fn run(events: Sender<UiEvent>, cmds: Receiver<UiCommand>) {
    tokio::spawn(browse_loop(events.clone()));

    let mut active: Option<Active> = None;
    let mut hosting: Option<Hosting> = None;

    while let Ok(cmd) = cmds.recv_async().await {
        match cmd {
            UiCommand::Host { name } => {
                // 先回收旧资源：旧房间的 endpoint、广播、人数定时器
                if let Some(previous) = hosting.take() {
                    previous.shutdown();
                }
                if let Some(previous) = active.take() {
                    previous.end().await;
                }

                match start_host(&name, &events).await {
                    Ok(started) => {
                        let alive = Arc::new(AtomicBool::new(true));
                        let visible = Arc::new(AtomicBool::new(true));
                        spawn_presence_ticker(
                            Arc::clone(&started.server),
                            started.server.room(),
                            Arc::clone(&alive),
                            Arc::clone(&visible),
                            events.clone(),
                        );

                        let addr = started.addr.clone();
                        let fingerprint = started.fingerprint.clone();
                        hosting = Some(Hosting {
                            room: started.server.room(),
                            endpoint: Arc::clone(&started.endpoint),
                            _announcer: started.announcer,
                            alive,
                            visible,
                        });

                        // 房主自己也接入房间，否则同一个窗口里无法参与聊天
                        match open_session(&addr, Some(&fingerprint), &events).await {
                            Ok(session) => active = Some(session),
                            Err(e) => {
                                let _ = events.send(UiEvent::Error(e));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = events.send(UiEvent::Error(e));
                    }
                }
            }

            UiCommand::Connect { addr, fingerprint } => {
                if let Some(previous) = active.take() {
                    previous.end().await;
                }
                match open_session(&addr, fingerprint.as_deref(), &events).await {
                    Ok(session) => {
                        // 已切到别的房间，停止用旧房间的人数覆盖界面
                        if let Some(current) = &hosting {
                            current
                                .visible
                                .store(current.room == session.room, Ordering::Relaxed);
                        }
                        active = Some(session);
                    }
                    Err(e) => {
                        let _ = events.send(UiEvent::Error(e));
                    }
                }
            }

            UiCommand::Join { name } => {
                if let Some(session) = &active
                    && let Err(e) = session.client.join(session.room, name).await
                {
                    let _ = events.send(UiEvent::Error(format!("加入失败: {e}")));
                }
            }

            UiCommand::Leave => {
                if let Some(session) = &active {
                    let _ = session.client.leave(session.room).await;
                }
            }

            UiCommand::Send { text } => {
                if let Some(session) = &active
                    && let Err(e) = session.client.send_text(session.room, text).await
                {
                    let _ = events.send(UiEvent::Error(format!("发送失败: {e}")));
                }
            }
        }
    }
}

/// 房主房间的人数上报：只在该房间正被展示时推送。
fn spawn_presence_ticker(
    server: Arc<Server>,
    room: RoomId,
    alive: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    events: Sender<UiEvent>,
) {
    tokio::spawn(async move {
        while alive.load(Ordering::Relaxed) {
            tokio::time::sleep(PRESENCE_INTERVAL).await;
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            if !visible.load(Ordering::Relaxed) {
                continue;
            }
            let event = UiEvent::Presence {
                room,
                online: server.online_count().await,
                members: server.member_count().await,
            };
            if events.send(event).is_err() {
                return;
            }
        }
    });
}

struct Started {
    server: Arc<Server>,
    endpoint: Arc<QuicServer>,
    announcer: RoomAnnouncer,
    addr: String,
    fingerprint: String,
}

async fn start_host(name: &str, events: &Sender<UiEvent>) -> Result<Started, String> {
    let server = Server::new(name);
    let endpoint = Arc::new(
        QuicServer::bind(SocketAddr::from(([0, 0, 0, 0], 0))).map_err(|e| e.to_string())?,
    );
    let port = endpoint.local_addr().port();
    let fingerprint = endpoint.fingerprint().to_string();

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&endpoint).serve(acceptor));

    let announcer = RoomAnnouncer::start(server.room(), name, port, &fingerprint)
        .map_err(|e| format!("房间广播失败: {e}"))?;

    let _ = events.send(UiEvent::Hosting {
        name: name.to_string(),
        addrs: local_addrs(port),
        fingerprint: fingerprint.clone(),
    });

    Ok(Started {
        server,
        endpoint,
        announcer,
        addr: format!("127.0.0.1:{port}"),
        fingerprint,
    })
}

/// 本机可被局域网访问的地址，附带回环地址便于单机自测。
fn local_addrs(port: u16) -> Vec<String> {
    let mut addrs = vec![format!("127.0.0.1:{port}")];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for interface in ifs {
            if !interface.is_loopback() && interface.is_oper_up() && interface.ip().is_ipv4() {
                addrs.push(format!("{}:{port}", interface.ip()));
            }
        }
    }
    addrs
}

fn map_event(event: ServerEvent) -> UiEvent {
    match event {
        ServerEvent::Lobby { info } => UiEvent::Presence {
            room: info.room,
            online: info.online_count,
            members: info.member_count,
        },
        ServerEvent::Joined {
            room, you, members, ..
        } => UiEvent::Joined {
            room,
            you,
            members: members.len(),
        },
        ServerEvent::Left { room } => UiEvent::Left { room },
        ServerEvent::Message { message } => UiEvent::Message {
            room: message.room,
            sender_id: message.sender,
            sender: message.sender_name,
            text: message.text,
        },
        ServerEvent::Presence {
            room,
            online_count,
            member_count,
        } => UiEvent::Presence {
            room,
            online: online_count,
            members: member_count,
        },
        ServerEvent::History { room, .. } => UiEvent::Status(format!("房间 {room} 历史已加载")),
        ServerEvent::Error { message, .. } => UiEvent::Error(message),
    }
}

async fn open_session(
    addr_text: &str,
    fingerprint: Option<&str>,
    events: &Sender<UiEvent>,
) -> Result<Active, String> {
    let addr: SocketAddr = addr_text
        .parse()
        .map_err(|e| format!("地址「{addr_text}」无效: {e}"))?;

    let conn = Arc::new(connect(addr, fingerprint).await.map_err(|e| format!("连接失败: {e}"))?);
    let info = conn.room_info().clone();
    let room = info.room;

    let dyn_conn: Arc<dyn Connection> = conn.clone();
    let client = Client::new(dyn_conn);

    client
        .lobby(room)
        .await
        .map_err(|e| format!("查询大厅失败: {e}"))?;
    if let Some(lobby) = await_lobby(&client).await {
        let _ = events.send(UiEvent::Lobby {
            room,
            name: lobby.name,
            addr: addr_text.to_string(),
            online: lobby.online_count,
            members: lobby.member_count,
        });
    }

    let pump = {
        let client = client.clone();
        let events = events.clone();
        tokio::spawn(async move {
            loop {
                match client.next_event().await {
                    Ok(event) => {
                        if events.send(map_event(event)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = events.send(UiEvent::Disconnected);
                        break;
                    }
                }
            }
        })
    };

    Ok(Active { client, room, pump })
}

async fn await_lobby(client: &Client) -> Option<LobbyInfo> {
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(ServerEvent::Lobby { info })) => return Some(info),
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
}

/// 已发现的房间。候选地址按可达性排序，非回环优先。
#[derive(Clone)]
struct KnownRoom {
    name: String,
    fingerprint: String,
    addrs: Vec<SocketAddr>,
}

/// 持续浏览局域网房间，并周期复查各自人数。
async fn browse_loop(events: Sender<UiEvent>) {
    let mut browser = match RoomBrowser::start() {
        Ok(browser) => browser,
        Err(e) => {
            let _ = events.send(UiEvent::Error(format!("房间发现不可用: {e}")));
            return;
        }
    };

    // 按房间标识去重：mDNS 与 UDP 广播会重复播报同一个房间
    let known: Arc<tokio::sync::Mutex<HashMap<RoomId, KnownRoom>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    {
        let known = Arc::clone(&known);
        let events = events.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PROBE_INTERVAL).await;
                let rooms: Vec<(RoomId, KnownRoom)> = known
                    .lock()
                    .await
                    .iter()
                    .map(|(room, entry)| (*room, entry.clone()))
                    .collect();
                for (room, entry) in rooms {
                    probe_room(room, &entry, &events).await;
                }
            }
        });
    }

    while let Ok(announcement) = browser.next_room().await {
        let entry = {
            let mut known = known.lock().await;
            let entry = known.entry(announcement.room).or_insert_with(|| KnownRoom {
                name: announcement.name.clone(),
                fingerprint: announcement.fingerprint.clone(),
                addrs: Vec::new(),
            });
            entry.name = announcement.name.clone();
            entry.fingerprint = announcement.fingerprint.clone();
            for addr in &announcement.addrs {
                if !entry.addrs.contains(addr) {
                    entry.addrs.push(*addr);
                }
            }
            // 非回环优先：跨机必须用它，同机也照样可用
            entry.addrs.sort_by_key(|addr| addr.ip().is_loopback());
            entry.clone()
        };
        probe_room(announcement.room, &entry, &events).await;
    }
}

/// 连一下房间只为读大厅摘要，随后断开——未加入者拿不到任何消息。
///
/// 依次尝试候选地址，只播报第一个能拿到摘要的地址，避免同一房间出现多条记录。
async fn probe_room(room: RoomId, known: &KnownRoom, events: &Sender<UiEvent>) {
    let expected = (!known.fingerprint.is_empty()).then_some(known.fingerprint.as_str());

    for addr in &known.addrs {
        let Ok(conn) = connect(*addr, expected).await else {
            continue;
        };
        let conn = Arc::new(conn);
        let dyn_conn: Arc<dyn Connection> = conn.clone();
        let client = Client::new(dyn_conn);
        if client.lobby(room).await.is_err() {
            continue;
        }
        let info = await_lobby(&client).await;
        let _ = client.close().await;

        if let Some(info) = info {
            let _ = events.send(UiEvent::RoomFound {
                room,
                name: if info.name.is_empty() {
                    known.name.clone()
                } else {
                    info.name
                },
                addr: addr.to_string(),
                fingerprint: known.fingerprint.clone(),
                online: info.online_count,
                members: info.member_count,
            });
            return;
        }
    }
}
