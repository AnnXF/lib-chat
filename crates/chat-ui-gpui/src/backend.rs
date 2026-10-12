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
use chat_transport_quic::{Liveness, QuicServer, connect, connect_with};
use flume::{Receiver, Sender};

/// 房间列表的人数复查间隔。房间要在未加入时也能看到人数，只能周期性探测。
const PROBE_INTERVAL: Duration = Duration::from_secs(3);
/// 单次握手与查询的超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// 房主房间的人数上报间隔。
const PRESENCE_INTERVAL: Duration = Duration::from_secs(2);
/// 退出请求发出后到关闭连接之间的等待，确保请求送达。
const LEAVE_GRACE: Duration = Duration::from_millis(80);
/// 重连窗口：从断开那一刻开始计时，超出就放弃重连并把用户放回大厅。
///
/// 留得足够宽，能覆盖 WiFi 切换与笔记本短暂休眠；又不至于让房间真的消失之后还无限停在
/// 「正在重连」上。
const RECONNECT_WINDOW: Duration = Duration::from_secs(60);

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
    },
    Joined {
        room: RoomId,
        /// 我在这个房间里的身份，用于区分「我发的」与「别人发的」。
        you: UserId,
        online: u32,
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
    },
    /// 连接断开、正在自动重连。房间与聊天记录都应保留，不要把人踢出去。
    Reconnecting(String),
    /// 重连超时，会话到此结束。
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
    spawn_on(chat_discovery::beacon::DEFAULT_BEACON_PORT)
}

/// 指定广播端口启动后端。同机需要跑多套互不干扰的实例（例如并行测试）时使用。
pub fn spawn_on(beacon_port: u16) -> (Sender<UiCommand>, Receiver<UiEvent>) {
    spawn_with(beacon_port, Liveness::default())
}

/// 指定广播端口与连接活性参数启动后端。
///
/// 活性参数主要供测试使用：把空闲超时压到秒级，就能在不必真等 30 秒的前提下检验自动重连。
pub fn spawn_with(beacon_port: u16, liveness: Liveness) -> (Sender<UiCommand>, Receiver<UiEvent>) {
    let (cmd_tx, cmd_rx) = flume::unbounded();
    let (ev_tx, ev_rx) = flume::unbounded();

    // 运行时建不起来也要把原因告诉界面，而不是安静地少掉一个后端线程
    let report = ev_tx.clone();
    std::thread::Builder::new()
        .name("chat-backend".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build();
            let Ok(runtime) = runtime else {
                let _ = report.send(UiEvent::Error("后端运行时启动失败".into()));
                return;
            };
            runtime.block_on(run(ev_tx, cmd_rx, beacon_port, liveness));
        })
        .expect("启动后端线程失败");

    (cmd_tx, ev_rx)
}

/// 界面要求当前会话做的事。
enum SessionCmd {
    Join(String),
    Leave,
    Send(String),
    /// 结束会话：先退出房间再关闭连接。
    Stop,
}

/// 当前会话。连接断开后由会话任务自行重连，界面无需感知。
struct Active {
    room: RoomId,
    tx: Sender<SessionCmd>,
    task: tokio::task::JoinHandle<()>,
}

impl Active {
    /// 结束会话。切换房间时先退出旧房间，让对端的在线人数立刻回落。
    async fn end(self) {
        let _ = self.tx.send(SessionCmd::Stop);
        let _ = self.task.await;
    }
}

/// 重连退避：先密后疏，最长 5 秒。
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(3)).min(5))
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

async fn run(
    events: Sender<UiEvent>,
    cmds: Receiver<UiCommand>,
    beacon_port: u16,
    liveness: Liveness,
) {
    tokio::spawn(browse_loop(events.clone(), beacon_port));

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

                match start_host(&name, &events, beacon_port, liveness).await {
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
                        match open_session(&addr, Some(&fingerprint), &events, liveness).await {
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
                match open_session(&addr, fingerprint.as_deref(), &events, liveness).await {
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
                if let Some(session) = &active {
                    let _ = session.tx.send(SessionCmd::Join(name));
                }
            }

            UiCommand::Leave => {
                if let Some(session) = &active {
                    let _ = session.tx.send(SessionCmd::Leave);
                }
            }

            UiCommand::Send { text } => {
                if let Some(session) = &active {
                    let _ = session.tx.send(SessionCmd::Send(text));
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

async fn start_host(
    name: &str,
    events: &Sender<UiEvent>,
    beacon_port: u16,
    liveness: Liveness,
) -> Result<Started, String> {
    let server = Server::new(name);
    let endpoint = Arc::new(
        QuicServer::bind_with(SocketAddr::from(([0, 0, 0, 0], 0)), liveness)
            .map_err(|e| e.to_string())?,
    );
    let port = endpoint.local_addr().port();
    let fingerprint = endpoint.fingerprint().to_string();

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&endpoint).serve(acceptor));

    let announcer = RoomAnnouncer::start_on(beacon_port, server.room(), name, port, &fingerprint)
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
        },
        ServerEvent::Joined {
            room, you, online_count, ..
        } => UiEvent::Joined {
            room,
            you,
            online: online_count,
        },
        ServerEvent::Left { room } => UiEvent::Left { room },
        ServerEvent::Message { message } => UiEvent::Message {
            room: message.room,
            sender_id: message.sender,
            sender: message.sender_name,
            text: message.text,
        },
        ServerEvent::Presence { room, online_count } => UiEvent::Presence {
            room,
            online: online_count,
        },
        ServerEvent::History { room, .. } => UiEvent::Status(format!("房间 {room} 历史已加载")),
        ServerEvent::Error { message, .. } => UiEvent::Error(message),
    }
}

async fn open_session(
    addr_text: &str,
    fingerprint: Option<&str>,
    events: &Sender<UiEvent>,
    liveness: Liveness,
) -> Result<Active, String> {
    let addr: SocketAddr = addr_text
        .parse()
        .map_err(|e| format!("地址「{addr_text}」无效: {e}"))?;

    // 先连一次：既把「连不上」尽早报给界面，也拿到房间标识。
    // 这条连接直接交给会话任务，不必重复握手。
    let conn = connect_with(addr, fingerprint, liveness)
        .await
        .map_err(|e| format!("连接失败: {e}"))?;
    let room = conn.room_info().room;
    let first: Arc<dyn Connection> = Arc::new(conn);

    // 界面要显示「XX 房间」，这条查询只在首次连接时做
    let client = Client::new(Arc::clone(&first));
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
        });
    }

    let (tx, rx) = flume::unbounded();
    let task = tokio::spawn(session(
        addr,
        fingerprint.map(str::to_string),
        room,
        first,
        events.clone(),
        rx,
        liveness,
    ));

    Ok(Active { room, tx, task })
}

/// 会话任务：维持一条到房间的连接。
///
/// 连接意外断开（WiFi 抖动、笔记本休眠、房主重启）时自动重连并恢复身份——用户的心智是
/// 「只有点了退出才离开房间」，网络抖动不该把人踢出去。只有在 [`RECONNECT_WINDOW`] 内
/// 始终连不上，或收到 [`SessionCmd::Stop`]（用户切房间/退出），会话才真正结束。
async fn session(
    addr: SocketAddr,
    fingerprint: Option<String>,
    room: RoomId,
    first: Arc<dyn Connection>,
    events: Sender<UiEvent>,
    cmds: Receiver<SessionCmd>,
    liveness: Liveness,
) {
    // 已加入时的昵称，重连后据此恢复身份
    let mut identity: Option<String> = None;
    let mut next: Option<Arc<dyn Connection>> = Some(first);
    let mut attempt: u32 = 0;
    // 断开后开始计时，超时就不再无限重试
    let mut give_up_at: Option<tokio::time::Instant> = None;

    loop {
        let conn = if let Some(conn) = next.take() {
            conn
        } else {
            let deadline = *give_up_at
                .get_or_insert_with(|| tokio::time::Instant::now() + RECONNECT_WINDOW);

            // 退避等待。期间仍要响应界面指令，并且不能丢掉「改名 / 退出」这类状态变更
            let until = (tokio::time::Instant::now() + backoff(attempt)).min(deadline);
            let mut stop = false;
            loop {
                let remaining = until.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, cmds.recv_async()).await {
                    Ok(Err(_)) | Ok(Ok(SessionCmd::Stop)) => {
                        stop = true;
                        break;
                    }
                    Ok(Ok(SessionCmd::Join(name))) => identity = Some(name),
                    Ok(Ok(SessionCmd::Leave)) => {
                        identity = None;
                        let _ = events.send(UiEvent::Left { room });
                    }
                    // 断线期间的发消息无从投递，明确告知而不是静默丢弃
                    Ok(Ok(SessionCmd::Send(_))) => {
                        let _ = events.send(UiEvent::Error("发送失败: 尚未重新连接".into()));
                    }
                    Err(_) => break,
                }
            }
            if stop {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = events.send(UiEvent::Disconnected);
                return;
            }

            match connect_with(addr, fingerprint.as_deref(), liveness).await {
                Ok(conn) => Arc::new(conn) as Arc<dyn Connection>,
                Err(_) => {
                    attempt += 1;
                    let _ = events.send(UiEvent::Reconnecting(format!(
                        "仍无法连接（已重试 {attempt} 次）"
                    )));
                    continue;
                }
            }
        };

        let client = Client::new(conn);
        // 每次（重）连都问一次大厅：确认这条连接真的可用，界面的房间名也靠它
        if client.lobby(room).await.is_err() {
            attempt += 1;
            continue;
        }
        // 之前已加入过就恢复身份，用户不会察觉自己掉过线
        if let Some(name) = identity.clone()
            && client.join(room, name).await.is_err()
        {
            attempt += 1;
            continue;
        }
        attempt = 0;
        give_up_at = None;

        loop {
            tokio::select! {
                event = client.next_event() => match event {
                    Ok(event) => {
                        // 界面已经没了，会话没必要继续
                        if events.send(map_event(event)).is_err() {
                            let _ = client.close().await;
                            return;
                        }
                    }
                    Err(_) => break,
                },
                cmd = cmds.recv_async() => match cmd {
                    // 界面已销毁，或明确要求结束
                    Err(_) | Ok(SessionCmd::Stop) => {
                        // 明确的离开：先退出房间，让对端的在线人数立刻回落
                        if identity.is_some() {
                            let _ = client.leave(room).await;
                            // 等服务端收下并处理，再关闭连接，否则关得太快会把请求吞掉
                            tokio::time::sleep(LEAVE_GRACE).await;
                        }
                        let _ = client.close().await;
                        return;
                    }
                    Ok(SessionCmd::Join(name)) => {
                        identity = Some(name.clone());
                        if let Err(e) = client.join(room, name).await {
                            let _ = events.send(UiEvent::Error(format!("加入失败: {e}")));
                        }
                    }
                    Ok(SessionCmd::Leave) => {
                        identity = None;
                        if let Err(e) = client.leave(room).await {
                            let _ = events.send(UiEvent::Error(format!("退出失败: {e}")));
                        }
                    }
                    Ok(SessionCmd::Send(text)) => {
                        if let Err(e) = client.send_text(room, text).await {
                            let _ = events.send(UiEvent::Error(format!("发送失败: {e}")));
                        }
                    }
                },
            }
        }

        // 连接断了：进入重连，不再把用户踢出房间
        if give_up_at.is_none() {
            give_up_at = Some(tokio::time::Instant::now() + RECONNECT_WINDOW);
            let _ = events.send(UiEvent::Reconnecting("连接中断".into()));
        }
    }
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
async fn browse_loop(events: Sender<UiEvent>, beacon_port: u16) {
    let mut browser = match RoomBrowser::start_on(beacon_port) {
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
            });
            return;
        }
    }
}
