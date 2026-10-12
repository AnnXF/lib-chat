//! 服务端：房间状态、在线人数、消息落库与扇出。
//!
//! 房间没有「总人数」的概念：**连接断开即离开**，在线人数完全由当前连接派生。
//! 这样切换房间、掉线、重连都不会留下任何残留计数。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chat_core::{
    ChatError, Connection, ConnectionAcceptor, EventStream, MessageStore, NewMessage, RequestSink,
    Result,
};
use chat_proto::{
    ClientRequest, DeviceId, ErrorCode, LobbyInfo, Message, MessageKind, RoomId, RoomInfo, Seq,
    ServerEvent, UserId,
};
use tokio::sync::{mpsc, Mutex, RwLock};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sanitize_name(raw: &str, device: DeviceId) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        format!("用户{}", &device.0.to_string()[..4])
    } else {
        trimmed.chars().take(24).collect()
    }
}

// ---------------------------------------------------------------- 存储

#[derive(Default)]
struct RoomLog {
    next_seq: Seq,
    messages: Vec<Message>,
}

/// 进程内消息存储，按房间维护单调序号。
#[derive(Default)]
pub struct InMemoryStore {
    inner: Mutex<HashMap<RoomId, RoomLog>>,
}

#[async_trait]
impl MessageStore for InMemoryStore {
    async fn append(&self, room: RoomId, msg: NewMessage) -> Result<Message> {
        let mut guard = self.inner.lock().await;
        let log = guard.entry(room).or_default();
        log.next_seq += 1;
        let stored = Message {
            room,
            seq: log.next_seq,
            client_msg_id: msg.client_msg_id,
            sender: msg.sender,
            sender_name: msg.sender_name,
            kind: msg.kind,
            text: msg.text,
            server_ts_ms: now_ms(),
        };
        log.messages.push(stored.clone());
        Ok(stored)
    }

    async fn history(&self, room: RoomId, before: Option<Seq>, limit: usize) -> Result<Vec<Message>> {
        let guard = self.inner.lock().await;
        let Some(log) = guard.get(&room) else {
            return Ok(Vec::new());
        };
        let cutoff = before.unwrap_or(Seq::MAX);
        let mut out: Vec<Message> = log
            .messages
            .iter()
            .filter(|m| m.seq < cutoff)
            .cloned()
            .collect();
        if out.len() > limit {
            out.drain(..out.len() - limit);
        }
        Ok(out)
    }

    async fn latest_seq(&self, room: RoomId) -> Result<Seq> {
        let guard = self.inner.lock().await;
        Ok(guard.get(&room).map(|l| l.next_seq).unwrap_or(0))
    }
}

// ---------------------------------------------------------------- 服务端

/// 一条连接的唯一标识。同一设备可同时存在多条连接。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConnId(u64);

/// 说到说话时挂在连接上的身份，断开即消失。
struct Member {
    user: UserId,
    name: String,
}

struct Conn {
    device: DeviceId,
    tx: mpsc::UnboundedSender<ServerEvent>,
    member: Option<Member>,
}

pub struct Server {
    room: RoomId,
    name: String,
    store: Arc<dyn MessageStore>,
    conns: RwLock<HashMap<ConnId, Conn>>,
    next_conn: AtomicU64,
}

impl Server {
    pub fn new(name: impl Into<String>) -> Arc<Self> {
        Self::with_store(name, Arc::new(InMemoryStore::default()))
    }

    pub fn with_store(name: impl Into<String>, store: Arc<dyn MessageStore>) -> Arc<Self> {
        Arc::new(Self {
            room: RoomId::new(),
            name: name.into(),
            store,
            conns: RwLock::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
        })
    }

    pub fn room(&self) -> RoomId {
        self.room
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn online_count(&self) -> u32 {
        self.conns
            .read()
            .await
            .values()
            .filter(|c| c.member.is_some())
            .count() as u32
    }

    /// 注册一条连接，返回请求入口与事件出口。
    ///
    /// 进程内直连与远程连接都走这里，区别只在事件出口由谁消费。
    pub async fn attach(
        self: &Arc<Self>,
        device: DeviceId,
    ) -> (ServerHandle, mpsc::UnboundedReceiver<ServerEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = ConnId(self.next_conn.fetch_add(1, Ordering::Relaxed));
        self.conns.write().await.insert(
            conn,
            Conn {
                device,
                tx,
                member: None,
            },
        );
        (
            ServerHandle {
                server: Arc::clone(self),
                conn,
                device,
            },
            rx,
        )
    }

    /// 进程内连接，此时尚未加入房间。
    pub async fn connect(self: &Arc<Self>, device: DeviceId) -> Arc<InProcConnection> {
        let (handle, rx) = self.attach(device).await;
        Arc::new(InProcConnection {
            handle,
            rx: Mutex::new(rx),
            closed: AtomicBool::new(false),
        })
    }

    async fn disconnect(&self, conn: ConnId) {
        let was_member = self
            .conns
            .write()
            .await
            .remove(&conn)
            .map(|c| c.member.is_some())
            .unwrap_or(false);
        if was_member {
            self.broadcast_presence().await;
        }
    }

    async fn lobby_info(&self) -> LobbyInfo {
        LobbyInfo {
            room: self.room,
            name: self.name.clone(),
            online_count: self.online_count().await,
        }
    }

    async fn send_to(&self, conn: ConnId, ev: ServerEvent) {
        let tx = self.conns.read().await.get(&conn).map(|c| c.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(ev);
        }
    }

    async fn error_to(&self, conn: ConnId, code: ErrorCode, message: &str) {
        self.send_to(
            conn,
            ServerEvent::Error {
                code,
                message: message.to_string(),
            },
        )
        .await;
    }

    /// 该连接已加入时的身份信息。
    async fn member_of(&self, conn: ConnId) -> Option<(UserId, String)> {
        self.conns
            .read()
            .await
            .get(&conn)
            .and_then(|c| c.member.as_ref())
            .map(|m| (m.user, m.name.clone()))
    }

    async fn conn_device(&self, conn: ConnId) -> Option<DeviceId> {
        self.conns.read().await.get(&conn).map(|c| c.device)
    }

    /// 把在线人数推给**所有**连接。
    ///
    /// 已加入者收到 `Presence`，未加入者只收到摘要 `Lobby`——人数本来就是公开的，
    /// 而消息内容只给已加入者。若只推给已加入者，停留在房间外的人会一直停在旧数字上。
    async fn broadcast_presence(&self) {
        let info = self.lobby_info().await;
        let targets: Vec<(mpsc::UnboundedSender<ServerEvent>, bool)> = self
            .conns
            .read()
            .await
            .values()
            .map(|c| (c.tx.clone(), c.member.is_some()))
            .collect();

        for (tx, joined) in targets {
            let event = if joined {
                ServerEvent::Presence {
                    room: info.room,
                    online_count: info.online_count,
                }
            } else {
                ServerEvent::Lobby { info: info.clone() }
            };
            let _ = tx.send(event);
        }
    }

    async fn handle(&self, conn: ConnId, req: ClientRequest) -> Result<()> {
        match req {
            ClientRequest::Lobby { room } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在").await;
                    return Ok(());
                }
                let info = self.lobby_info().await;
                self.send_to(conn, ServerEvent::Lobby { info }).await;
            }

            ClientRequest::Join { room, profile } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在").await;
                    return Ok(());
                }
                let Some(device) = self.conn_device(conn).await else {
                    return Ok(());
                };
                let name = sanitize_name(&profile.name, device);

                let user = {
                    let mut conns = self.conns.write().await;
                    match conns.get_mut(&conn) {
                        Some(existing) if existing.member.is_none() => {
                            let user = UserId::new();
                            existing.member = Some(Member {
                                user,
                                name: name.clone(),
                            });
                            Some(user)
                        }
                        _ => None,
                    }
                };
                let Some(user) = user else {
                    self.error_to(conn, ErrorCode::AlreadyJoined, "已在房间中").await;
                    return Ok(());
                };

                let online_count = self.online_count().await;
                let latest_seq = self.store.latest_seq(self.room).await?;
                self.send_to(
                    conn,
                    ServerEvent::Joined {
                        room: self.room,
                        you: user,
                        online_count,
                        latest_seq,
                    },
                )
                .await;
                self.broadcast_presence().await;
            }

            ClientRequest::Leave { room } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在").await;
                    return Ok(());
                }
                let was_member = {
                    let mut conns = self.conns.write().await;
                    match conns.get_mut(&conn) {
                        Some(existing) if existing.member.is_some() => {
                            existing.member = None;
                            true
                        }
                        _ => false,
                    }
                };
                if !was_member {
                    self.error_to(conn, ErrorCode::NotJoined, "未加入房间").await;
                    return Ok(());
                }
                self.send_to(conn, ServerEvent::Left { room: self.room }).await;
                self.broadcast_presence().await;
            }

            ClientRequest::Send {
                room,
                client_msg_id,
                text,
            } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在").await;
                    return Ok(());
                }
                let Some((user, name)) = self.member_of(conn).await else {
                    self.error_to(conn, ErrorCode::NotJoined, "未加入房间").await;
                    return Ok(());
                };
                let text = text.trim();
                if text.is_empty() {
                    self.error_to(conn, ErrorCode::InvalidRequest, "消息内容为空").await;
                    return Ok(());
                }
                let stored = self
                    .store
                    .append(
                        self.room,
                        NewMessage {
                            client_msg_id,
                            sender: user,
                            sender_name: name,
                            kind: MessageKind::Text,
                            text: text.to_string(),
                        },
                    )
                    .await?;
                self.broadcast_to_members(ServerEvent::Message { message: stored })
                    .await;
            }

            ClientRequest::History { room, before, limit } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在").await;
                    return Ok(());
                }
                if self.member_of(conn).await.is_none() {
                    self.error_to(conn, ErrorCode::NotJoined, "未加入房间").await;
                    return Ok(());
                }
                let limit = limit.clamp(1, 200) as usize;
                let messages = self.store.history(self.room, before, limit).await?;
                let has_more = messages.len() == limit;
                self.send_to(
                    conn,
                    ServerEvent::History {
                        room: self.room,
                        messages,
                        has_more,
                    },
                )
                .await;
            }
        }
        Ok(())
    }

    /// 只发给已加入的连接——消息内容不对房间外的人可见。
    async fn broadcast_to_members(&self, ev: ServerEvent) {
        let targets: Vec<mpsc::UnboundedSender<ServerEvent>> = self
            .conns
            .read()
            .await
            .values()
            .filter(|c| c.member.is_some())
            .map(|c| c.tx.clone())
            .collect();
        for tx in targets {
            let _ = tx.send(ev.clone());
        }
    }
}

// ---------------------------------------------------------------- 连接入口

/// 一条已注册连接的请求入口，传输层把收到的请求交给它即可。
#[derive(Clone)]
pub struct ServerHandle {
    server: Arc<Server>,
    conn: ConnId,
    device: DeviceId,
}

impl ServerHandle {
    pub fn device(&self) -> DeviceId {
        self.device
    }
}

#[async_trait]
impl RequestSink for ServerHandle {
    async fn dispatch(&self, req: ClientRequest) -> Result<()> {
        self.server.handle(self.conn, req).await
    }

    async fn detach(&self) {
        self.server.disconnect(self.conn).await;
    }
}

/// 把服务端暴露成传输层可用的接入点。
pub struct Acceptor(pub Arc<Server>);

#[async_trait]
impl ConnectionAcceptor for Acceptor {
    fn room_info(&self) -> RoomInfo {
        RoomInfo {
            room: self.0.room(),
            name: self.0.name().to_string(),
        }
    }

    async fn accept(&self, device: DeviceId) -> (Box<dyn RequestSink>, EventStream) {
        let (handle, rx) = self.0.attach(device).await;
        (Box::new(handle), rx)
    }
}

// ---------------------------------------------------------------- 进程内连接

pub struct InProcConnection {
    handle: ServerHandle,
    rx: Mutex<mpsc::UnboundedReceiver<ServerEvent>>,
    closed: AtomicBool,
}

#[async_trait]
impl Connection for InProcConnection {
    async fn request(&self, req: ClientRequest) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        self.handle.dispatch(req).await
    }

    async fn next_event(&self) -> Result<ServerEvent> {
        let mut rx = self.rx.lock().await;
        rx.recv().await.ok_or(ChatError::Closed)
    }

    async fn close(&self) -> Result<()> {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.handle.detach().await;
        }
        Ok(())
    }
}

impl Drop for InProcConnection {
    fn drop(&mut self) {
        let handle = self.handle.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { handle.detach().await });
        }
    }
}
