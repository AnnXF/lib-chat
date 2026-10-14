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

mod discovery;
mod hosting;
mod protocol;
mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chat_client::Client;
use chat_core::{ChatError, Connection, ConnectionAcceptor};
use chat_discovery::{RoomAnnouncer, RoomBrowser};
use chat_proto::{LobbyInfo, Message, RoomId, Seq, ServerEvent, UserId};
use chat_server::{Acceptor, Server};
use chat_transport_quic::{Liveness, QuicServer, connect, connect_with};
use discovery::browse_loop;
use flume::{Receiver, Sender};
use hosting::{Hosting, start_host};
use session::{Active, SessionCmd, open_session};

/// 房间列表的人数复查间隔。房间要在未加入时也能看到人数，只能周期性探测。
const PROBE_INTERVAL: Duration = Duration::from_secs(3);
/// 单次握手与查询的超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// 建房：起监听与广播、开人数上报，并把房主自己也接进去。
///
/// 自己接不进去不算建房失败——房间已经在广播了，别人照样能连，只是本窗口暂时不进房。
async fn start_hosting(
    name: &str,
    events: &Sender<UiEvent>,
    beacon_port: u16,
    liveness: Liveness,
    data_dir: &Path,
) -> Result<(Hosting, Option<Active>), String> {
    let started = start_host(name, events, beacon_port, liveness, data_dir).await?;

    let alive = Arc::new(AtomicBool::new(true));
    let visible = Arc::new(AtomicBool::new(true));
    spawn_presence_ticker(
        Arc::clone(&started.server),
        started.server.room(),
        Arc::clone(&alive),
        Arc::clone(&visible),
        events.clone(),
    );

    let hosting = Hosting {
        room: started.server.room(),
        endpoint: Arc::clone(&started.endpoint),
        _announcer: started.announcer,
        alive,
        visible,
    };

    // 房主自己也接入房间，否则同一个窗口里无法参与聊天
    let session =
        match open_session(&started.addr, Some(&started.fingerprint), events, liveness).await {
            Ok(session) => Some(session),
            Err(e) => {
                let _ = events.send(UiEvent::Error(e));
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
    pub sender_id: UserId,
    pub sender: String,
    pub text: String,
}

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
    /// 往前翻一页更早的历史。
    LoadOlder,
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

async fn run(
    events: Sender<UiEvent>,
    cmds: Receiver<UiCommand>,
    beacon_port: u16,
    liveness: Liveness,
    data_dir: PathBuf,
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

                match start_hosting(&name, &events, beacon_port, liveness, &data_dir).await {
                    Ok((started, session)) => {
                        hosting = Some(started);
                        active = session;
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

            UiCommand::LoadOlder => {
                if let Some(session) = &active {
                    let _ = session.tx.send(SessionCmd::LoadOlder);
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
                online: server.online_count().await,
            };
            if events.send(event).is_err() {
                return;
            }
        }
    });
}
