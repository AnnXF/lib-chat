//! 聊天服务：在独立 tokio 运行时里托管连接与房间，通过消息通道与界面交互。
//!
//! 界面线程不直接触碰网络，只发送 [`UiCommand`]、接收 [`UiEvent`]。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`session`] | 会话：建连、身份恢复、断线重连 |
//! | [`protocol`] | 网络协议 → 界面事件 |
//! | [`hosting`] | 本机托管房间：监听、广播、人数上报 |
//! | [`discovery`] | 局域网房间发现与人数探测 |
//! | [`remote`] | 公网地址簿：保存地址、周期探测 |

mod discovery;
mod hosting;
mod protocol;
mod remote;
mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use protocol::to_history_message;

use chat_core::Client;
use chat_core::{Attachment, AttachmentId};
use chat_core::{ChatError, Connection, ConnectionAcceptor};
use chat_core::{
    LobbyInfo, Message, Quote, RoomId, RoomMember, RoomSummary, Seq, ServerEvent, UserId,
    UserSummary,
};
use chat_discovery::{RoomAnnouncer, RoomBrowser};
use chat_server::Hub;
use chat_transport_quic::{
    Liveness, PublicCa, QuicServer, ServerIdentity, TrustStore, connect_to, parse_host_port,
    validate_fingerprint,
};
use discovery::{DiscoveredRooms, browse_loop};
use flume::{Receiver, Sender};
use hosting::{
    Hosting, PublicStart, RoomHostLock, claim_local_room, local_room, start_host, start_public_host,
};
use remote::RemoteServers;
use session::{Active, Auth, SessionCmd, await_auth, open_session};

/// 房间列表的人数复查间隔。房间要在未加入时也能看到人数，只能周期性探测。
const PROBE_INTERVAL: Duration = Duration::from_secs(3);
/// 单次探测的总期限。
///
/// 必须长于传输层对单个地址的握手期限，并留出域名解析出多个地址时逐个尝试的余量；
/// 否则探测会先取消，服务端只能看到一条握手到一半就消失的连接。
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
/// 建房：起监听与广播、开人数上报，并把房主自己也接进去。
///
/// 自己接不进去不算建房失败——房间已经在广播了，别人照样能连，只是本窗口暂时不进房。
async fn start_hosting(
    name: &str,
    events: &Sender<UiEvent>,
    beacon_port: u16,
    liveness: Liveness,
    lock: RoomHostLock,
) -> Result<(Hosting, Option<Active>), String> {
    let started = start_host(name, events, beacon_port, liveness, lock).await?;
    let room = started.hub.default_room();

    let alive = Arc::new(AtomicBool::new(true));
    let visible = Arc::new(AtomicBool::new(true));
    spawn_presence_ticker(
        Arc::clone(&started.hub),
        room,
        Arc::clone(&alive),
        Arc::clone(&visible),
        events.clone(),
    );

    let hosting = Hosting {
        room,
        endpoint: Arc::clone(&started.endpoint),
        _hub: Arc::clone(&started.hub),
        _announcer: started.announcer,
        alive,
        visible,
        lock: started.lock,
    };

    // 房主自己也接入房间，否则同一个窗口里无法参与聊天；本地 Hub 用自动房主账号
    let session = match open_session(
        &started.addr,
        &ServerIdentity::fingerprint(started.fingerprint.clone()),
        events,
        liveness,
        Some(room),
        None,
        None,
        None,
        Some(Auth::Ensure {
            username: "owner".into(),
            password: "libchat-owner".into(),
        }),
    )
    .await
    {
        Ok((session, lobby, _)) => {
            let _ = events.send_async(lobby).await;
            Some(session)
        }
        Err(e) => {
            let _ = events.send_async(UiEvent::Error(e)).await;
            None
        }
    };

    Ok((hosting, session))
}

/// 房主房间的人数上报间隔。
const PRESENCE_INTERVAL: Duration = Duration::from_secs(2);
/// 退出请求发出后到关闭连接之间的等待，确保请求送达。
const LEAVE_GRACE: Duration = Duration::from_millis(80);
/// 重连窗口：从断开那一刻开始计时，超出就放弃重连并把用户放回大厅。
///
/// 留得足够宽，能覆盖 WiFi 切换与笔记本短暂休眠；又不至于让房间真的消失之后还无限停在
/// 「正在重连」上。
const RECONNECT_WINDOW: Duration = Duration::from_secs(60);

/// 加入房间时拉取的最近消息条数。
const HISTORY_LIMIT: u32 = 50;
/// 往上翻一页的条数。
const OLDER_PAGE: u32 = 50;
/// 重连后每次追平的条数上限；还有更多就继续拉。
const SYNC_LIMIT: u32 = 200;

/// 一条补齐回来的消息。
#[derive(Clone, Debug)]
pub struct HistoryMessage {
    pub seq: Seq,
    pub sender_id: UserId,
    pub sender: String,
    pub at: i64,
    pub text: String,
    pub attachment: Option<Attachment>,
    pub quote: Option<Quote>,
}

/// 公网托管的接入信息：客户端据此决定填域名还是填指纹。
#[derive(Clone, Debug)]
pub struct PublicHosting {
    pub port: u16,
    /// 用户填写的域名；留空时客户端需要自行用可解析的域名连接。
    pub domain: Option<String>,
    /// 使用受信 CA 签发的证书；否则是自签 + 指纹固定。
    pub trusted_ca: bool,
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Status(String),
    Error(String),
    TrustRequired {
        room: RoomId,
        addr: String,
        fingerprint: String,
    },
    /// 局域网里发现（或刷新到）的房间。同一房间可能经多条通道被播报，按 `room` 去重。
    RoomFound {
        room: RoomId,
        name: String,
        addr: String,
        fingerprint: String,
        online: u32,
    },
    /// 公网服务器探测到的房间：没有广播，靠保存的地址周期探测。
    RemoteRoom {
        room: RoomId,
        name: String,
        addr: String,
        online: u32,
    },
    /// 本机已作为房主启动。
    Hosting {
        room: RoomId,
        name: String,
        addrs: Vec<String>,
        fingerprint: String,
        /// 公网托管时的端口与接入方式；局域网为 `None`。
        public: Option<PublicHosting>,
    },
    /// 本机已经有一个可恢复的房间（房间标识 + 已存多少条消息）。界面据此给出入口。
    LocalRoom {
        room: RoomId,
        messages: u64,
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
        seq: Seq,
        sender_id: UserId,
        sender: String,
        at: i64,
        text: String,
        attachment: Option<Attachment>,
        quote: Option<Quote>,
    },
    /// 取回的一页历史（比当前会话更早），界面应插到列表最前。
    History {
        room: RoomId,
        messages: Vec<HistoryMessage>,
        /// 还能继续往前翻更早的消息。
        has_more: bool,
    },
    /// 重连后追平的漏收消息（晚于当前会话），界面应追加到列表末尾。
    Missed {
        room: RoomId,
        messages: Vec<HistoryMessage>,
    },
    HistoryCancelled {
        room: RoomId,
    },
    Presence {
        room: RoomId,
        online: u32,
    },
    /// 某个房间已经探不到了，应从列表里摘掉。
    ///
    /// 房主被强杀时不会有任何告别信号，只能靠连续探测失败发现。
    RoomGone {
        room: RoomId,
    },
    /// 连接断开、正在自动重连。房间与聊天记录都应保留，不要把人踢出去。
    Reconnecting(String),
    /// 重连超时，会话到此结束。
    Disconnected,
    /// 登录/注册成功。
    Authenticated {
        user: UserSummary,
        token: String,
    },
    /// 我的会话列表（群聊 + 单聊）。
    Conversations {
        rooms: Vec<RoomSummary>,
    },
    Friends {
        users: Vec<UserSummary>,
    },
    /// 用户搜索结果。
    UserSearch {
        users: Vec<UserSummary>,
    },
    /// 当前房间的消息搜索结果。
    SearchResults {
        room: RoomId,
        messages: Vec<HistoryMessage>,
    },
    MessageRecalled {
        room: RoomId,
        seq: Seq,
    },
    MessageEdited {
        room: RoomId,
        message: HistoryMessage,
    },
    Mention {
        room: RoomId,
        from: UserSummary,
        text: String,
    },
    /// 好友上下线。
    FriendPresence {
        user: UserId,
        online: bool,
    },
    /// 已退出登录。
    LoggedOut,
    RoomMembers {
        room: RoomId,
        members: Vec<RoomMember>,
    },
    AuthRequired,
}

#[derive(Clone, Debug)]
pub enum UiCommand {
    Host {
        name: String,
    },
    /// 在本机启动公网服务端：PEM 证书或自签，固定端口，不广播。
    HostPublic {
        name: String,
        port: u16,
        cert: Option<PathBuf>,
        key: Option<PathBuf>,
        /// 填了就在本机以客户端身份自动接入，并显示为加入地址。
        domain: Option<String>,
    },
    /// 清空本机房间的历史，从空白重新开一个。
    NewRoom {
        name: String,
    },
    Connect {
        addr: String,
        /// 自签房间的指纹；为空表示公网受信 CA。
        fingerprint: Option<String>,
    },
    ConnectDiscovered {
        room: RoomId,
        addr: String,
        /// 局域网广播一定带指纹；公网地址簿不带。
        fingerprint: Option<String>,
    },
    ConfirmTrust {
        room: RoomId,
        addr: String,
        fingerprint: String,
    },
    Join {
        name: String,
    },
    Leave,
    Send {
        text: String,
        /// 当前输入框上方的引用；没有引用为 `None`。
        quote: Option<Quote>,
    },
    /// 发送一个本地文件。
    SendFile {
        path: PathBuf,
    },
    /// 把聊天里的附件另存到本地。
    SaveFile {
        id: AttachmentId,
        path: PathBuf,
    },
    /// 往前翻一页更早的历史。
    LoadOlder,
    /// 连接服务端并登录/注册；成功后自动进入默认大厅。
    Login {
        addr: String,
        /// 自签服务端的指纹；受信 CA 留空。
        fingerprint: Option<String>,
        username: String,
        password: String,
        register: bool,
    },
    /// 切换到会话列表里的某个房间。
    OpenRoom {
        room: RoomId,
    },
    RefreshRooms,
    AddFriend {
        username: String,
    },
    RemoveFriend {
        user: UserId,
    },
    SearchUsers {
        query: String,
    },
    SearchMessages {
        room: RoomId,
        query: String,
    },
    RecallMessage {
        room: RoomId,
        seq: Seq,
    },
    EditMessage {
        room: RoomId,
        seq: Seq,
        text: String,
    },
    MarkRead {
        room: RoomId,
        seq: Seq,
    },
    Logout,
    OpenDirect {
        user: UserId,
    },
    CreateGroup {
        name: String,
        members: Vec<UserId>,
    },
    RoomMembers {
        room: RoomId,
    },
    LeaveGroup {
        room: RoomId,
    },
    RemoveMember {
        room: RoomId,
        user: UserId,
    },
    DissolveGroup {
        room: RoomId,
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
    spawn_with_data(beacon_port, liveness, default_data_dir())
}

/// 指定数据目录启动后端。
///
/// 消息库落在该目录下，文件名带广播端口——所以**数据目录加端口**才是房间的真正边界。
/// 测试用它把库关进临时目录，不去碰真实数据。
pub fn spawn_with_data(
    beacon_port: u16,
    liveness: Liveness,
    data_dir: PathBuf,
) -> (Sender<UiCommand>, Receiver<UiEvent>) {
    let (cmd_tx, cmd_rx) = flume::bounded(128);
    let (ev_tx, ev_rx) = flume::bounded(512);

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
                let _ = report.try_send(UiEvent::Error("后端运行时启动失败".into()));
                return;
            };
            runtime.block_on(run(ev_tx, cmd_rx, beacon_port, liveness, data_dir));
        })
        .expect("启动后端线程失败");

    (cmd_tx, ev_rx)
}

/// 消息库所在目录：`$HOME/.lib-chat/rooms`，家目录不可用时退回临时目录。
fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".lib-chat").join("rooms")
}

enum RoomRequest {
    Reopen(String),
    New(String),
}

/// 建房或重建房间：从抢锁到重新托管始终持有同一把锁。
async fn start_new_room(
    request: RoomRequest,
    hosting: &mut Option<Hosting>,
    active: &mut Option<Active>,
    events: &Sender<UiEvent>,
    beacon_port: u16,
    liveness: Liveness,
    data_dir: &Path,
) {
    let (name, reset) = match request {
        RoomRequest::Reopen(name) => (name, false),
        RoomRequest::New(name) => (name, true),
    };
    // 当前托管的是公网房间（没有局域网分区锁）时，得先拿一把新的
    let needs_lock = hosting
        .as_ref()
        .is_none_or(|current| current.lock.is_none());
    let fresh_lock = if needs_lock {
        match claim_local_room(data_dir, beacon_port) {
            Ok(lock) => Some(lock),
            Err(e) => {
                let _ = events.send_async(UiEvent::Error(e)).await;
                return;
            }
        }
    } else {
        None
    };

    if let Some(previous) = active.take() {
        previous.end().await;
    }
    let previous_lock = if let Some(previous) = hosting.take() {
        previous.shutdown(events).await
    } else {
        None
    };
    let lock = previous_lock
        .or(fresh_lock)
        .expect("必须持有房主锁才能建房");

    if reset && let Err(e) = lock.reset() {
        let _ = events.send_async(UiEvent::Error(e)).await;
        return;
    }

    match start_hosting(&name, events, beacon_port, liveness, lock).await {
        Ok((started, session)) => {
            *hosting = Some(started);
            *active = session;
        }
        Err(e) => {
            let _ = events.send_async(UiEvent::Error(e)).await;
        }
    }
}

/// 启动公网服务端；填了域名就以客户端身份接进去，否则只起服务、等外部客户端连。
#[allow(clippy::too_many_arguments)]
async fn start_public_room(
    name: String,
    port: u16,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    domain: Option<String>,
    hosting: &mut Option<Hosting>,
    active: &mut Option<Active>,
    events: &Sender<UiEvent>,
    liveness: Liveness,
    data_dir: &Path,
) {
    if let Some(previous) = active.take() {
        previous.end().await;
    }
    if let Some(previous) = hosting.take() {
        previous.shutdown(events).await;
    }

    let started = match start_public_host(
        PublicStart {
            name: &name,
            port,
            cert: cert.as_deref(),
            key: key.as_deref(),
            domain: domain.as_deref(),
            data_dir,
        },
        events,
        liveness,
    )
    .await
    {
        Ok(started) => started,
        Err(e) => {
            let _ = events.send_async(UiEvent::Error(e)).await;
            return;
        }
    };

    let alive = Arc::new(AtomicBool::new(true));
    let visible = Arc::new(AtomicBool::new(true));
    let room = started.hub.default_room();
    spawn_presence_ticker(
        Arc::clone(&started.hub),
        room,
        Arc::clone(&alive),
        Arc::clone(&visible),
        events.clone(),
    );

    let public = started.public.clone().expect("公网托管必须带接入信息");
    let fallback_addr = started.addr.clone();
    let fingerprint = started.fingerprint.clone();
    *hosting = Some(Hosting {
        room,
        endpoint: Arc::clone(&started.endpoint),
        _hub: Arc::clone(&started.hub),
        _announcer: started.announcer,
        alive,
        visible,
        lock: started.lock,
    });

    // 受信证书 + 没填域名：本地拿 127.0.0.1 连会因为主机名不符被拒，干脆不自动接入
    let target = match (&public.domain, public.trusted_ca) {
        (Some(domain), true) => {
            let addr = format!("{domain}:{}", public.port);
            match manual_identity(&addr, None) {
                Ok(identity) => Some((addr, identity)),
                Err(e) => {
                    let _ = events.send_async(UiEvent::Error(e)).await;
                    None
                }
            }
        }
        (Some(domain), false) => Some((
            format!("{domain}:{}", public.port),
            ServerIdentity::fingerprint(fingerprint.clone()),
        )),
        (None, false) => Some((
            fallback_addr,
            ServerIdentity::fingerprint(fingerprint.clone()),
        )),
        (None, true) => None,
    };

    *active = match target {
        Some((addr, identity)) => {
            match open_session(
                &addr,
                &identity,
                events,
                liveness,
                Some(room),
                None,
                None,
                None,
                Some(Auth::Ensure {
                    username: "owner".into(),
                    password: "libchat-owner".into(),
                }),
            )
            .await
            {
                Ok((session, lobby, _)) => {
                    let _ = events.send_async(lobby).await;
                    Some(session)
                }
                Err(e) => {
                    let _ = events.send_async(UiEvent::Error(e)).await;
                    None
                }
            }
        }
        None => None,
    };
}

struct ConnectPolicy<'a> {
    expected_room: Option<RoomId>,
    trust: &'a TrustStore,
    rooms: &'a DiscoveredRooms,
}

/// 手动连接的校验方式：给了指纹就按固定指纹，否则按公网受信 CA。
fn manual_identity(addr: &str, fingerprint: Option<String>) -> Result<ServerIdentity, String> {
    match fingerprint {
        Some(fingerprint) => {
            let fingerprint = fingerprint.trim().to_ascii_lowercase();
            validate_fingerprint(&fingerprint).map_err(|e| e.to_string())?;
            Ok(ServerIdentity::fingerprint(fingerprint))
        }
        None => {
            let (host, _) = parse_host_port(addr).map_err(|e| e.to_string())?;
            Ok(public_identity(host))
        }
    }
}

/// 公网身份：默认信任系统内置公共根；私有 CA 用 `LIBCHAT_CA_FILE` 指定 PEM。
fn public_identity(server_name: impl Into<String>) -> ServerIdentity {
    let mut public = PublicCa {
        server_name: server_name.into(),
        roots_pem: None,
    };
    if let Some(path) = std::env::var_os("LIBCHAT_CA_FILE")
        && let Ok(pem) = std::fs::read(path)
    {
        public.roots_pem = Some(pem);
    }
    ServerIdentity::PublicCa(public)
}

/// 候选房间准备好后再关闭旧会话；失败时仍留在原房间。
async fn take_room(
    addr: &str,
    identity: &ServerIdentity,
    hosting: &Option<Hosting>,
    active: &mut Option<Active>,
    events: &Sender<UiEvent>,
    liveness: Liveness,
    policy: &ConnectPolicy<'_>,
) -> Result<RoomId, String> {
    let (session, lobby, _) = open_session(
        addr,
        identity,
        events,
        liveness,
        policy.expected_room,
        None,
        Some(policy.trust),
        Some(policy.rooms.clone()),
        Some(Auth::Ensure {
            username: "owner".into(),
            password: "libchat-owner".into(),
        }),
    )
    .await?;
    if let Some(previous) = active.take() {
        previous.end().await;
    }
    if let Some(current) = hosting.as_ref() {
        current
            .visible
            .store(current.room == session.room, Ordering::Relaxed);
    }
    let room = session.room;
    let _ = events.send_async(lobby).await;
    *active = Some(session);
    Ok(room)
}

/// 进入一个房间，进不去就把原因报给界面。
async fn connect_room(
    addr: String,
    identity: ServerIdentity,
    hosting: &mut Option<Hosting>,
    active: &mut Option<Active>,
    events: &Sender<UiEvent>,
    liveness: Liveness,
    policy: &ConnectPolicy<'_>,
) -> bool {
    match take_room(&addr, &identity, hosting, active, events, liveness, policy).await {
        Ok(_) => true,
        Err(e) => {
            let _ = events.send_async(UiEvent::Error(e)).await;
            false
        }
    }
}

/// 房主的广播可能要等一轮才被认出来：广播 2 秒一轮，多给一点。
const LOCAL_HOST_WAIT: Duration = Duration::from_secs(4);

/// 本机已经有实例当着房主吗？广播可能还没跟上，所以直接问库旁边的锁。
///
/// 能拿到锁就说明本机没有房主，拿到立刻还回去——这里只是问一句。
fn local_room_has_host(data_dir: &Path, beacon_port: u16) -> bool {
    claim_local_room(data_dir, beacon_port).is_err()
}

/// 「我的房间」：本机已经有实例开着这个房间就接过去，返回是否接上了。
///
/// 本机房间按广播端口共享同一个库，所以同一个房间标识可能同时被两个实例「开」。各自起
/// 服务器的话，消息只落库、只推给自己那半边的连接，看起来就是「不实时，退出再进才看得见」。
///
/// 广播会在房主退出后残留一会儿，所以这里真连一次再算数：连不上（残留、或是它已经不在
/// 那个指纹后面了）就当没人在开，照常自己开一个。
async fn adopt_local_room(
    rooms: &DiscoveredRooms,
    data_dir: &Path,
    beacon_port: u16,
    hosting: &Option<Hosting>,
    active: &mut Option<Active>,
    events: &Sender<UiEvent>,
    liveness: Liveness,
) -> bool {
    let Some((room, _)) = local_room(data_dir, beacon_port).await else {
        return false;
    };
    // 自己就是房主时不必「接」：下面走的还是原来的重建逻辑
    if hosting.as_ref().is_some_and(|current| current.room == room) {
        return false;
    }

    // 锁被占着就说明本机确实有房主，只是第二个实例刚起来还没在广播里看到它，值多等一轮
    let deadline = local_room_has_host(data_dir, beacon_port)
        .then(|| tokio::time::Instant::now() + LOCAL_HOST_WAIT);

    loop {
        if let Some((addr, fingerprint)) = rooms.host_of(room).await
            && take_room(
                &addr,
                &ServerIdentity::fingerprint(fingerprint.clone()),
                hosting,
                active,
                events,
                liveness,
                &ConnectPolicy {
                    expected_room: Some(room),
                    trust: &TrustStore::new(data_dir),
                    rooms,
                },
            )
            .await
            .is_ok()
        {
            return true;
        }
        let Some(deadline) = deadline else {
            return false;
        };
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// 已登录的账号：切换房间时用它保存的地址、身份与令牌重新连接。
struct AccountState {
    addr: String,
    identity: ServerIdentity,
    token: String,
    user: UserSummary,
}

/// 用保存的令牌建立一条临时连接，执行好友/会话类请求。
async fn account_client(account: &AccountState, liveness: Liveness) -> Result<Client, String> {
    let (host, port) = parse_host_port(&account.addr).map_err(|e| e.to_string())?;
    let conn = connect_to(&host, port, &account.identity, liveness)
        .await
        .map_err(|e| format!("连接失败: {e}"))?;
    let client = Client::new(Arc::new(conn));
    client
        .resume(&account.token)
        .await
        .map_err(|e| format!("恢复登录失败: {e}"))?;
    await_auth(&client).await?;
    Ok(client)
}

async fn run(
    events: Sender<UiEvent>,
    cmds: Receiver<UiCommand>,
    beacon_port: u16,
    liveness: Liveness,
    data_dir: PathBuf,
) {
    let rooms = DiscoveredRooms::default();
    tokio::spawn(browse_loop(
        events.clone(),
        beacon_port,
        rooms.clone(),
        data_dir.clone(),
    ));
    // 公网没有广播，靠地址簿周期探测拿房间名与人数
    let remote = RemoteServers::new(&data_dir);
    tokio::spawn(remote::probe_loop(events.clone(), remote.clone(), liveness));

    // 本机是不是已经有这个端口的房间了？有就在侧栏给一个「回到我的房间」的入口
    if let Some((room, messages)) = local_room(&data_dir, beacon_port).await {
        let _ = events
            .send_async(UiEvent::LocalRoom { room, messages })
            .await;
    }

    let mut active: Option<Active> = None;
    let mut hosting: Option<Hosting> = None;
    let mut account: Option<AccountState> = None;
    let trust = TrustStore::new(&data_dir);
    let mut pending_trust: Option<(RoomId, String, String)> = None;

    while let Ok(cmd) = cmds.recv_async().await {
        match cmd {
            UiCommand::Login {
                addr,
                fingerprint,
                username,
                password,
                register,
            } => {
                let identity = match manual_identity(&addr, fingerprint) {
                    Ok(identity) => identity,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                let auth = Auth::Login {
                    username,
                    password,
                    register,
                };
                match open_session(
                    &addr,
                    &identity,
                    &events,
                    liveness,
                    None,
                    None,
                    None,
                    None,
                    Some(auth),
                )
                .await
                {
                    Ok((session, lobby, auth)) => {
                        let Some(auth) = auth else {
                            let _ = session.end().await;
                            let _ = events
                                .send_async(UiEvent::Error("登录没有返回身份".into()))
                                .await;
                            continue;
                        };
                        let display = auth.user.display_name.clone();
                        let state = AccountState {
                            addr,
                            identity,
                            token: auth.token.clone(),
                            user: auth.user.clone(),
                        };
                        account = Some(state);
                        let _ = events.send_async(lobby).await;
                        let _ = events
                            .send_async(UiEvent::Authenticated {
                                user: auth.user,
                                token: auth.token,
                            })
                            .await;
                        // open_session 的 lobby 查询会顺手吃掉 Rooms 推送，这里主动拉一次
                        if let Some(current) = &account
                            && let Ok(client) = account_client(current, liveness).await
                        {
                            let _ = client.list_rooms().await;
                            let _ = client.list_friends().await;
                            // 两条应答都读完就回到主循环；不要在这里无限等，否则后续命令全被堵住
                            for _ in 0..4 {
                                match tokio::time::timeout(
                                    Duration::from_millis(300),
                                    client.next_event(),
                                )
                                .await
                                {
                                    Ok(Ok(ServerEvent::Rooms { rooms })) => {
                                        let _ = events
                                            .send_async(UiEvent::Conversations { rooms })
                                            .await;
                                    }
                                    Ok(Ok(ServerEvent::Friends { users })) => {
                                        let _ = events.send_async(UiEvent::Friends { users }).await;
                                    }
                                    Ok(Ok(ServerEvent::Error { .. })) | Ok(Err(_)) | Err(_) => {
                                        break;
                                    }
                                    Ok(Ok(_)) => {}
                                }
                            }
                        }
                        if let Some(previous) = active.take() {
                            previous.end().await;
                        }
                        active = Some(session);
                        if let Some(session) = &active {
                            send_session(session, SessionCmd::Join(display), &events).await;
                        }
                    }
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                    }
                }
            }
            UiCommand::OpenRoom { room } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let auth = Auth::Resume(current.token.clone());
                match open_session(
                    &current.addr,
                    &current.identity,
                    &events,
                    liveness,
                    None,
                    Some(room),
                    None,
                    None,
                    Some(auth),
                )
                .await
                {
                    Ok((session, lobby, _)) => {
                        let display = current.user.display_name.clone();
                        let _ = events.send_async(lobby).await;
                        if let Some(previous) = active.take() {
                            previous.end().await;
                        }
                        active = Some(session);
                        if let Some(session) = &active {
                            send_session(session, SessionCmd::Join(display), &events).await;
                        }
                    }
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                    }
                }
            }
            UiCommand::RefreshRooms
            | UiCommand::AddFriend { .. }
            | UiCommand::OpenDirect { .. }
            | UiCommand::CreateGroup { .. } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let client = match account_client(current, liveness).await {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                let sent = match cmd {
                    UiCommand::RefreshRooms => client.list_rooms().await,
                    UiCommand::AddFriend { username } => client.add_friend(username).await,
                    UiCommand::OpenDirect { user } => client.open_direct(user).await,
                    UiCommand::CreateGroup { name, members } => {
                        client.create_group(name, members).await
                    }
                    _ => unreachable!(),
                };
                if let Err(e) = sent {
                    let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                    continue;
                }
                for _ in 0..12 {
                    match client.next_event().await {
                        Ok(ServerEvent::Rooms { rooms }) => {
                            let _ = events.send_async(UiEvent::Conversations { rooms }).await;
                            break;
                        }
                        Ok(ServerEvent::Friends { users }) => {
                            let _ = events.send_async(UiEvent::Friends { users }).await;
                            break;
                        }
                        Ok(ServerEvent::Error { message, .. }) => {
                            let _ = events.send_async(UiEvent::Error(message)).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                            break;
                        }
                    }
                }
            }
            UiCommand::RoomMembers { room } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let client = match account_client(current, liveness).await {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                if let Err(e) = client.room_members(room).await {
                    let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                    continue;
                }
                for _ in 0..8 {
                    match client.next_event().await {
                        Ok(ServerEvent::RoomMembers { room, members }) => {
                            let _ = events
                                .send_async(UiEvent::RoomMembers { room, members })
                                .await;
                            break;
                        }
                        Ok(ServerEvent::Error { message, .. }) => {
                            let _ = events.send_async(UiEvent::Error(message)).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                            break;
                        }
                    }
                }
            }
            UiCommand::LeaveGroup { room } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let client = match account_client(current, liveness).await {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                if let Err(e) = client.leave_group(room).await {
                    let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                    continue;
                }
                let mut left = false;
                for _ in 0..12 {
                    match client.next_event().await {
                        Ok(ServerEvent::Rooms { rooms }) => {
                            let _ = events.send_async(UiEvent::Conversations { rooms }).await;
                        }
                        Ok(ServerEvent::Left { .. }) => {
                            left = true;
                            break;
                        }
                        Ok(ServerEvent::Error { message, .. }) => {
                            let _ = events.send_async(UiEvent::Error(message)).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                if left {
                    if let Some(previous) = active.take() {
                        previous.end().await;
                    }
                    let _ = events.send_async(UiEvent::Left { room }).await;
                }
            }
            UiCommand::RemoveMember { room, user } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let client = match account_client(current, liveness).await {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                if let Err(e) = client.remove_member(room, user).await {
                    let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                    continue;
                }
                for _ in 0..12 {
                    match client.next_event().await {
                        Ok(ServerEvent::Rooms { rooms }) => {
                            let _ = events.send_async(UiEvent::Conversations { rooms }).await;
                        }
                        Ok(ServerEvent::RoomMembers { room, members }) => {
                            let _ = events
                                .send_async(UiEvent::RoomMembers { room, members })
                                .await;
                            break;
                        }
                        Ok(ServerEvent::Error { message, .. }) => {
                            let _ = events.send_async(UiEvent::Error(message)).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            UiCommand::DissolveGroup { room } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let client = match account_client(current, liveness).await {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                if let Err(e) = client.dissolve_group(room).await {
                    let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                    continue;
                }
                for _ in 0..12 {
                    match tokio::time::timeout(Duration::from_millis(300), client.next_event())
                        .await
                    {
                        Ok(Ok(ServerEvent::Rooms { rooms })) => {
                            let _ = events.send_async(UiEvent::Conversations { rooms }).await;
                        }
                        Ok(Ok(ServerEvent::Error { message, .. })) => {
                            let _ = events.send_async(UiEvent::Error(message)).await;
                            break;
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(_)) | Err(_) => break,
                    }
                }
                if active.as_ref().is_some_and(|session| session.room == room) {
                    if let Some(previous) = active.take() {
                        previous.end().await;
                    }
                    let _ = events.send_async(UiEvent::Left { room }).await;
                }
            }
            UiCommand::RemoveFriend { user } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let client = match account_client(current, liveness).await {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                if let Err(e) = client.remove_friend(user).await {
                    let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                    continue;
                }
                for _ in 0..8 {
                    match client.next_event().await {
                        Ok(ServerEvent::Friends { users }) => {
                            let _ = events.send_async(UiEvent::Friends { users }).await;
                            break;
                        }
                        Ok(ServerEvent::Error { message, .. }) => {
                            let _ = events.send_async(UiEvent::Error(message)).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            UiCommand::SearchUsers { query } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let client = match account_client(current, liveness).await {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                if let Err(e) = client.search_users(query).await {
                    let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                    continue;
                }
                for _ in 0..8 {
                    match client.next_event().await {
                        Ok(ServerEvent::UserSearch { users }) => {
                            let _ = events.send_async(UiEvent::UserSearch { users }).await;
                            break;
                        }
                        Ok(ServerEvent::Error { message, .. }) => {
                            let _ = events.send_async(UiEvent::Error(message)).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            UiCommand::SearchMessages { room, query } => {
                let Some(current) = &account else {
                    let _ = events.send_async(UiEvent::Error("请先登录".into())).await;
                    continue;
                };
                let client = match account_client(current, liveness).await {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                if let Err(e) = client.search_messages(room, query).await {
                    let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                    continue;
                }
                for _ in 0..8 {
                    match client.next_event().await {
                        Ok(ServerEvent::SearchResults { room, messages }) => {
                            let messages = messages.into_iter().map(to_history_message).collect();
                            let _ = events
                                .send_async(UiEvent::SearchResults { room, messages })
                                .await;
                            break;
                        }
                        Ok(ServerEvent::Error { message, .. }) => {
                            let _ = events.send_async(UiEvent::Error(message)).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            UiCommand::RecallMessage { room, seq } => {
                let Some(session) = active.as_ref().filter(|session| session.room == room) else {
                    let _ = events
                        .send_async(UiEvent::Error("只能撤回当前会话里的消息".into()))
                        .await;
                    continue;
                };
                send_session(session, SessionCmd::Recall(seq), &events).await;
            }
            UiCommand::EditMessage { room, seq, text } => {
                let Some(session) = active.as_ref().filter(|session| session.room == room) else {
                    let _ = events
                        .send_async(UiEvent::Error("只能编辑当前会话里的消息".into()))
                        .await;
                    continue;
                };
                send_session(session, SessionCmd::Edit { seq, text }, &events).await;
            }
            UiCommand::MarkRead { room, seq } => {
                if let Some(session) = active.as_ref().filter(|session| session.room == room) {
                    send_session(session, SessionCmd::MarkRead(seq), &events).await;
                }
            }
            UiCommand::Logout => {
                let Some(current) = account.take() else {
                    let _ = events.send_async(UiEvent::LoggedOut).await;
                    continue;
                };
                if let Ok(client) = account_client(&current, liveness).await {
                    let _ = client.logout().await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            match client.next_event().await {
                                Ok(ServerEvent::AuthRequired) | Ok(ServerEvent::Error { .. }) => {
                                    break;
                                }
                                Ok(_) => {}
                                Err(_) => break,
                            }
                        }
                    })
                    .await;
                }
                if let Some(previous) = active.take() {
                    previous.end().await;
                }
                let _ = events.send_async(UiEvent::LoggedOut).await;
            }
            UiCommand::Host { name } => {
                // 「我的房间」：本机已经有实例开着这个房间时接过去，没人开才自己开
                let adopted = adopt_local_room(
                    &rooms,
                    &data_dir,
                    beacon_port,
                    &hosting,
                    &mut active,
                    &events,
                    liveness,
                )
                .await;
                if !adopted {
                    start_new_room(
                        RoomRequest::Reopen(name),
                        &mut hosting,
                        &mut active,
                        &events,
                        beacon_port,
                        liveness,
                        &data_dir,
                    )
                    .await;
                }
            }

            UiCommand::HostPublic {
                name,
                port,
                cert,
                key,
                domain,
            } => {
                start_public_room(
                    name,
                    port,
                    cert,
                    key,
                    domain,
                    &mut hosting,
                    &mut active,
                    &events,
                    liveness,
                    &data_dir,
                )
                .await;
            }

            UiCommand::NewRoom { name } => {
                start_new_room(
                    RoomRequest::New(name),
                    &mut hosting,
                    &mut active,
                    &events,
                    beacon_port,
                    liveness,
                    &data_dir,
                )
                .await;
            }

            UiCommand::Connect { addr, fingerprint } => {
                pending_trust = None;
                let identity = match manual_identity(&addr, fingerprint.clone()) {
                    Ok(identity) => identity,
                    Err(e) => {
                        let _ = events.send_async(UiEvent::Error(e)).await;
                        continue;
                    }
                };
                let public = fingerprint.is_none();
                let connected = connect_room(
                    addr.clone(),
                    identity,
                    &mut hosting,
                    &mut active,
                    &events,
                    liveness,
                    &ConnectPolicy {
                        expected_room: None,
                        trust: &trust,
                        rooms: &rooms,
                    },
                )
                .await;
                // 连上过的公网地址记进地址簿，下次启动自动探测与展示
                if connected && public {
                    remote.remember(&addr).await;
                }
            }
            UiCommand::ConnectDiscovered {
                room,
                addr,
                fingerprint,
            } => {
                pending_trust = None;
                match fingerprint {
                    Some(fingerprint) => {
                        if let Err(e) = validate_fingerprint(&fingerprint) {
                            let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                            continue;
                        }
                        let fingerprint = fingerprint.to_ascii_lowercase();
                        match trust.get(room) {
                            Ok(Some(pinned)) if pinned != fingerprint => {
                                let _ = events
                                    .send_async(UiEvent::Error(format!(
                                        "房间 {room} 的证书指纹已变更，已拒绝连接"
                                    )))
                                    .await;
                            }
                            Ok(Some(_)) => {
                                connect_room(
                                    addr,
                                    ServerIdentity::fingerprint(fingerprint),
                                    &mut hosting,
                                    &mut active,
                                    &events,
                                    liveness,
                                    &ConnectPolicy {
                                        expected_room: Some(room),
                                        trust: &trust,
                                        rooms: &rooms,
                                    },
                                )
                                .await;
                            }
                            Ok(None) => {
                                pending_trust = Some((room, addr.clone(), fingerprint.clone()));
                                let _ = events
                                    .send_async(UiEvent::TrustRequired {
                                        room,
                                        addr,
                                        fingerprint,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                let _ = events.send_async(UiEvent::Error(e.to_string())).await;
                            }
                        }
                    }
                    None => match manual_identity(&addr, None) {
                        Ok(identity) => {
                            connect_room(
                                addr,
                                identity,
                                &mut hosting,
                                &mut active,
                                &events,
                                liveness,
                                &ConnectPolicy {
                                    expected_room: Some(room),
                                    trust: &trust,
                                    rooms: &rooms,
                                },
                            )
                            .await;
                        }
                        Err(e) => {
                            let _ = events.send_async(UiEvent::Error(e)).await;
                        }
                    },
                }
            }
            UiCommand::ConfirmTrust {
                room,
                addr,
                fingerprint,
            } => {
                if pending_trust.take() == Some((room, addr.clone(), fingerprint.clone())) {
                    connect_room(
                        addr,
                        ServerIdentity::fingerprint(fingerprint),
                        &mut hosting,
                        &mut active,
                        &events,
                        liveness,
                        &ConnectPolicy {
                            expected_room: Some(room),
                            trust: &trust,
                            rooms: &rooms,
                        },
                    )
                    .await;
                } else {
                    let _ = events
                        .send_async(UiEvent::Error("待确认房间已失效，请重新选择".into()))
                        .await;
                }
            }

            UiCommand::Join { name } => match &active {
                Some(session) => {
                    send_session(session, SessionCmd::Join(name), &events).await;
                }
                // 没有会话时点「加入」不能一声不吭，否则看起来就是界面卡住了
                None => {
                    let _ = events
                        .send_async(UiEvent::Error("还没有连上房间".into()))
                        .await;
                }
            },

            UiCommand::Leave => {
                if let Some(session) = &active {
                    send_session(session, SessionCmd::Leave, &events).await;
                }
            }

            UiCommand::Send { text, quote } => {
                if let Some(session) = &active {
                    send_session(session, SessionCmd::Send { text, quote }, &events).await;
                }
            }

            UiCommand::LoadOlder => {
                if let Some(session) = &active {
                    send_session(session, SessionCmd::LoadOlder, &events).await;
                }
            }

            UiCommand::SendFile { path } => match &active {
                Some(session) => {
                    send_session(session, SessionCmd::SendFile(path), &events).await;
                }
                None => {
                    let _ = events
                        .send_async(UiEvent::Error("还没有连上房间".into()))
                        .await;
                }
            },

            UiCommand::SaveFile { id, path } => match &active {
                Some(session) => {
                    send_session(session, SessionCmd::SaveFile { id, path }, &events).await;
                }
                None => {
                    let _ = events
                        .send_async(UiEvent::Error("还没有连上房间".into()))
                        .await;
                }
            },
        }
    }
}

async fn send_session(session: &Active, cmd: SessionCmd, events: &Sender<UiEvent>) {
    if session.tx.try_send(cmd).is_err() {
        let _ = events
            .send_async(UiEvent::Error("会话繁忙或已断开，请稍后重试".into()))
            .await;
    }
}

/// 房主房间的人数上报：只在该房间正被展示时推送。
fn spawn_presence_ticker(
    hub: Arc<Hub>,
    room: RoomId,
    alive: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    events: Sender<UiEvent>,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PRESENCE_INTERVAL).await;
            // 房间可能在这段睡眠里被关掉了
            if !alive.load(Ordering::Relaxed) {
                return;
            }
            // 界面正在展示别的房间时不上报，否则会污染那个房间的人数
            if !visible.load(Ordering::Relaxed) {
                continue;
            }
            let event = UiEvent::Presence {
                room,
                online: hub.online_in(room).await,
            };
            if events.send_async(event).await.is_err() {
                return;
            }
        }
    });
}
