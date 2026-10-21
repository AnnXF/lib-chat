//! 单房间服务：连接状态、在线人数、消息落库与扇出。

mod connection;

pub use connection::{Acceptor, InProcConnection, ServerHandle};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chat_core::{
    sha256_hex, validate_attachment, Attachment, AttachmentId, ChatError, ClientRequest, DeviceId,
    ErrorCode, LobbyInfo, MessageKind, MessageStore, NewMessage, Quote, Result, RoomId,
    ServerEvent, UserId, EVENT_QUEUE_CAPACITY,
};
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::store::{BlobStore, InMemoryStore, MemoryBlobs};

const MAX_TEXT_BYTES: usize = 8 * 1024;

fn sanitize_name(raw: &str, device: DeviceId) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        format!("用户{}", &device.0.to_string()[..4])
    } else {
        trimmed.chars().take(24).collect()
    }
}

/// 引用只保留展示所需的信息，超长部分截断。
fn sanitize_quote(quote: Quote) -> Quote {
    Quote {
        sender: quote
            .sender
            .replace(['\n', '\r'], " ")
            .chars()
            .take(64)
            .collect(),
        text: quote
            .text
            .replace(['\n', '\r'], " ")
            .chars()
            .take(200)
            .collect(),
        seq: quote.seq,
    }
}

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
    tx: mpsc::Sender<ServerEvent>,
    member: Option<Member>,
}

pub struct Server {
    room: RoomId,
    name: String,
    store: Arc<dyn MessageStore>,
    blobs: Arc<dyn BlobStore>,
    conns: RwLock<HashMap<ConnId, Conn>>,
    publish: Mutex<()>,
    next_conn: AtomicU64,
}

impl Server {
    pub fn new(name: impl Into<String>) -> Arc<Self> {
        Self::with_store(name, Arc::new(InMemoryStore::default()))
    }

    pub fn with_store(name: impl Into<String>, store: Arc<dyn MessageStore>) -> Arc<Self> {
        Self::with_room(RoomId::new(), name, store)
    }

    /// 指定房间标识构造。落库场景下房间由库决定，不能每次重新生成，
    /// 否则重启后读不回已有的历史。
    pub fn with_room(
        room: RoomId,
        name: impl Into<String>,
        store: Arc<dyn MessageStore>,
    ) -> Arc<Self> {
        Self::with_blobs(room, name, store, Arc::new(MemoryBlobs::default()))
    }

    /// 同时指定附件存储；房主落盘时用磁盘实现。
    pub fn with_blobs(
        room: RoomId,
        name: impl Into<String>,
        store: Arc<dyn MessageStore>,
        blobs: Arc<dyn BlobStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            room,
            name: name.into(),
            store,
            blobs,
            conns: RwLock::new(HashMap::new()),
            publish: Mutex::new(()),
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
    ) -> (ServerHandle, mpsc::Receiver<ServerEvent>) {
        let (tx, rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let conn = ConnId(self.next_conn.fetch_add(1, Ordering::Relaxed));
        self.conns.write().await.insert(
            conn,
            Conn {
                device,
                tx,
                member: None,
            },
        );
        (ServerHandle::new(Arc::clone(self), conn, device), rx)
    }

    /// 进程内连接，此时尚未加入房间。
    pub async fn connect(self: &Arc<Self>, device: DeviceId) -> Arc<InProcConnection> {
        let (handle, rx) = self.attach(device).await;
        Arc::new(InProcConnection::new(handle, rx))
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
            if tx.try_send(ev).is_err() {
                self.disconnect(conn).await;
            }
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

    /// 把在线人数推给所有连接：成员收到 Presence，旁观者收到 Lobby。
    async fn broadcast_presence(&self) {
        loop {
            let info = self.lobby_info().await;
            let targets: Vec<(ConnId, mpsc::Sender<ServerEvent>, bool)> = self
                .conns
                .read()
                .await
                .iter()
                .map(|(id, c)| (*id, c.tx.clone(), c.member.is_some()))
                .collect();
            let mut slow = Vec::new();
            for (id, tx, joined) in targets {
                let event = if joined {
                    ServerEvent::Presence {
                        room: info.room,
                        online_count: info.online_count,
                    }
                } else {
                    ServerEvent::Lobby { info: info.clone() }
                };
                if tx.try_send(event).is_err() {
                    slow.push(id);
                }
            }
            if slow.is_empty() {
                break;
            }
            let mut conns = self.conns.write().await;
            let mut changed = false;
            for id in slow {
                changed |= conns.remove(&id).is_some_and(|c| c.member.is_some());
            }
            if !changed {
                break;
            }
        }
    }

    async fn handle(&self, conn: ConnId, req: ClientRequest) -> Result<()> {
        if !self.conns.read().await.contains_key(&conn) {
            return Err(chat_core::ChatError::Closed);
        }
        match req {
            ClientRequest::Lobby { room } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                    return Ok(());
                }
                let info = self.lobby_info().await;
                self.send_to(conn, ServerEvent::Lobby { info }).await;
            }

            ClientRequest::Join { room, profile } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                    return Ok(());
                }
                let Some(device) = self.conn_device(conn).await else {
                    return Ok(());
                };
                let name = sanitize_name(&profile.name, device);
                let latest_seq = match self.store.latest_seq(self.room).await {
                    Ok(seq) => seq,
                    Err(_) => {
                        self.error_to(conn, ErrorCode::Internal, "查询房间历史失败")
                            .await;
                        return Ok(());
                    }
                };

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
                    self.error_to(conn, ErrorCode::AlreadyJoined, "已在房间中")
                        .await;
                    return Ok(());
                };

                let online_count = self.online_count().await;
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
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
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
                    self.error_to(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                    return Ok(());
                }
                self.send_to(conn, ServerEvent::Left { room: self.room })
                    .await;
                self.broadcast_presence().await;
            }

            ClientRequest::Send {
                room,
                client_msg_id,
                text,
                quote,
            } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                    return Ok(());
                }
                let Some((user, name)) = self.member_of(conn).await else {
                    self.error_to(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                    return Ok(());
                };
                let text = text.trim();
                if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                    self.error_to(conn, ErrorCode::InvalidRequest, "消息为空或超过长度限制")
                        .await;
                    return Ok(());
                }
                let quote = quote.map(sanitize_quote);
                let _publish = self.publish.lock().await;
                match self
                    .store
                    .append(
                        self.room,
                        NewMessage {
                            client_msg_id,
                            sender: user,
                            sender_name: name,
                            kind: MessageKind::Text,
                            text: text.to_string(),
                            attachment: None,
                            quote,
                        },
                    )
                    .await
                {
                    Ok(result) if result.inserted => {
                        self.broadcast_to_members(ServerEvent::Message {
                            message: result.message,
                        })
                        .await;
                    }
                    Ok(result) => {
                        self.send_to(
                            conn,
                            ServerEvent::Message {
                                message: result.message,
                            },
                        )
                        .await;
                    }
                    Err(chat_core::ChatError::Protocol(_)) => {
                        self.error_to(
                            conn,
                            ErrorCode::InvalidRequest,
                            "消息标识与原消息内容不一致",
                        )
                        .await
                    }
                    Err(_) => {
                        self.error_to(conn, ErrorCode::Internal, "消息保存失败")
                            .await
                    }
                }
            }

            ClientRequest::SendFile {
                room,
                client_msg_id,
                attachment,
            } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                    return Ok(());
                }
                let Some((user, name)) = self.member_of(conn).await else {
                    self.error_to(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                    return Ok(());
                };
                if let Err(e) = validate_attachment(&attachment) {
                    self.error_to(conn, ErrorCode::InvalidRequest, &e.to_string())
                        .await;
                    return Ok(());
                }
                // 只广播已经落盘的附件，避免接收方拿到一个取不到的引用
                if !self.blobs.contains(&attachment.sha256).await {
                    self.error_to(conn, ErrorCode::NotFound, "附件尚未上传完成")
                        .await;
                    return Ok(());
                }
                let _publish = self.publish.lock().await;
                match self
                    .store
                    .append(
                        self.room,
                        NewMessage {
                            client_msg_id,
                            sender: user,
                            sender_name: name,
                            kind: MessageKind::File,
                            text: String::new(),
                            attachment: Some(attachment),
                            quote: None,
                        },
                    )
                    .await
                {
                    Ok(result) if result.inserted => {
                        self.broadcast_to_members(ServerEvent::Message {
                            message: result.message,
                        })
                        .await;
                    }
                    Ok(result) => {
                        self.send_to(
                            conn,
                            ServerEvent::Message {
                                message: result.message,
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        self.error_to(conn, ErrorCode::Internal, "消息保存失败")
                            .await
                    }
                }
            }

            ClientRequest::History {
                room,
                before,
                limit,
            } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                    return Ok(());
                }
                if self.member_of(conn).await.is_none() {
                    self.error_to(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                    return Ok(());
                }
                let limit = limit.clamp(1, 200) as usize;
                let mut messages = match self.store.history(self.room, before, limit + 1).await {
                    Ok(messages) => messages,
                    Err(_) => {
                        self.error_to(conn, ErrorCode::Internal, "读取历史失败")
                            .await;
                        return Ok(());
                    }
                };
                let has_more = messages.len() > limit;
                if has_more {
                    messages.remove(0);
                }
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

            ClientRequest::Sync { room, after, limit } => {
                if room != self.room {
                    self.error_to(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                    return Ok(());
                }
                if self.member_of(conn).await.is_none() {
                    self.error_to(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                    return Ok(());
                }
                let limit = limit.clamp(1, 200) as usize;
                let mut messages = match self.store.since(self.room, after, limit + 1).await {
                    Ok(messages) => messages,
                    Err(_) => {
                        self.error_to(conn, ErrorCode::Internal, "同步消息失败")
                            .await;
                        return Ok(());
                    }
                };
                let has_more = messages.len() > limit;
                messages.truncate(limit);
                self.send_to(
                    conn,
                    ServerEvent::Sync {
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

    /// 接收附件内容。只有房间成员可以上传，且大小与摘要必须与声明一致。
    pub async fn upload(&self, conn: ConnId, attachment: Attachment, data: Vec<u8>) -> Result<()> {
        if self.member_of(conn).await.is_none() {
            return Err(ChatError::Protocol("未加入房间".into()));
        }
        validate_attachment(&attachment)?;
        if data.len() as u64 != attachment.size {
            return Err(ChatError::Protocol("附件大小与声明不一致".into()));
        }
        if sha256_hex(&data) != attachment.sha256 {
            return Err(ChatError::Protocol("附件内容与摘要不一致".into()));
        }
        self.blobs.put(&attachment.sha256, &data).await
    }

    /// 读取附件内容。仅成员可取，且房间内必须存在该附件。
    pub async fn download(&self, conn: ConnId, id: AttachmentId) -> Result<Vec<u8>> {
        if self.member_of(conn).await.is_none() {
            return Err(ChatError::Protocol("未加入房间".into()));
        }
        let Some(attachment) = self.store.attachment(self.room, id).await? else {
            return Err(ChatError::Protocol("附件不存在".into()));
        };
        self.blobs.get(&attachment.sha256).await
    }

    /// 只发给已加入的连接——消息内容不对房间外的人可见。
    async fn broadcast_to_members(&self, ev: ServerEvent) {
        let targets: Vec<(ConnId, mpsc::Sender<ServerEvent>)> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, c)| c.member.is_some())
            .map(|(id, c)| (*id, c.tx.clone()))
            .collect();
        let slow: Vec<ConnId> = targets
            .into_iter()
            .filter_map(|(id, tx)| tx.try_send(ev.clone()).err().map(|_| id))
            .collect();
        if !slow.is_empty() {
            let mut conns = self.conns.write().await;
            let mut changed = false;
            for id in slow {
                changed |= conns.remove(&id).is_some_and(|c| c.member.is_some());
            }
            drop(conns);
            if changed {
                self.broadcast_presence().await;
            }
        }
    }
}
