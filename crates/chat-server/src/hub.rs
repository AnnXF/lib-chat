//! 多房间服务端：账号认证、好友、群聊/单聊，以及按房间隔离的消息扇出。
//!
//! 与单房间的 [`crate::Server`] 并行存在：局域网托管仍可走单房间，公网与账号体系走 Hub。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chat_core::{
    sha256_hex, validate_attachment, Attachment, AttachmentId, ChatError, ClientRequest,
    Connection, ConnectionAcceptor, DeviceId, ErrorCode, EventStream, LobbyInfo, MemberRole,
    MessageKind, MessageStore, NewMessage, Quote, RequestSink, Result, RoomId, RoomInfo, RoomKind,
    RoomSummary, ServerEvent, UserId, UserSummary, EVENT_QUEUE_CAPACITY,
};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};

use crate::accounts::{AccountError, Accounts};
use crate::store::{BlobStore, DiskBlobs, SqliteStore};

const MAX_TEXT_BYTES: usize = 8 * 1024;

type ConnId = u64;

struct Conn {
    tx: mpsc::Sender<ServerEvent>,
    user: Option<UserSummary>,
    /// Join 里带的昵称；没给就用账号显示名。
    display_name: Option<String>,
    joined: HashSet<RoomId>,
}

pub struct Hub {
    accounts: Arc<Accounts>,
    messages: Arc<SqliteStore>,
    blobs: Arc<dyn BlobStore>,
    default_room: RoomId,
    default_name: String,
    conns: RwLock<HashMap<ConnId, Conn>>,
    publish: Mutex<()>,
    next_conn: AtomicU64,
}

impl Hub {
    pub async fn open(data_dir: &Path, name: &str) -> std::result::Result<Arc<Self>, String> {
        std::fs::create_dir_all(data_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
        let accounts = Arc::new(
            Accounts::open(&data_dir.join("accounts.db"))
                .await
                .map_err(|e| e.to_string())?,
        );
        let messages = Arc::new(
            SqliteStore::open(&data_dir.join("messages.db"))
                .await
                .map_err(|e| e.to_string())?,
        );
        let blobs: Arc<dyn BlobStore> =
            Arc::new(DiskBlobs::new(data_dir.join("files")).map_err(|e| e.to_string())?);
        let default_room = accounts
            .ensure_default_room(name)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            accounts,
            messages,
            blobs,
            default_room,
            default_name: name.to_string(),
            conns: RwLock::new(HashMap::new()),
            publish: Mutex::new(()),
            next_conn: AtomicU64::new(0),
        }))
    }

    pub fn default_room(&self) -> RoomId {
        self.default_room
    }

    pub fn name(&self) -> &str {
        &self.default_name
    }

    /// 默认大厅的在线人数，用于 CLI 状态行。
    pub async fn online_count(&self) -> u32 {
        self.presence(self.default_room).await
    }

    /// 指定房间的在线人数，用于界面房主的人数上报。
    pub async fn online_in(&self, room: RoomId) -> u32 {
        self.presence(room).await
    }

    async fn attach(self: &Arc<Self>, device: DeviceId) -> (HubHandle, EventStream) {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        self.conns.write().await.insert(
            id,
            Conn {
                tx,
                user: None,
                display_name: None,
                joined: HashSet::new(),
            },
        );
        (
            HubHandle {
                hub: Arc::clone(self),
                conn: id,
                device,
            },
            rx,
        )
    }

    pub async fn connect(self: &Arc<Self>, device: DeviceId) -> Arc<dyn Connection> {
        let (handle, rx) = self.attach(device).await;
        Arc::new(HubConnection {
            handle,
            rx: Mutex::new(rx),
            closed: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    async fn user_of(&self, conn: ConnId) -> Option<UserSummary> {
        self.conns
            .read()
            .await
            .get(&conn)
            .and_then(|state| state.user.clone())
    }

    async fn display_name_of(&self, conn: ConnId) -> Option<String> {
        self.conns
            .read()
            .await
            .get(&conn)
            .and_then(|state| state.display_name.clone())
    }

    /// 把最新好友列表推给某个在线用户的所有连接。
    async fn push_friends(&self, user: UserId) {
        let Ok(users) = self.accounts.friends(user).await else {
            return;
        };
        let targets: Vec<ConnId> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, conn)| conn.user.as_ref().is_some_and(|known| known.id == user))
            .map(|(id, _)| *id)
            .collect();
        for conn in targets {
            self.send_to(
                conn,
                ServerEvent::Friends {
                    users: users.clone(),
                },
            )
            .await;
        }
    }

    async fn joined(&self, conn: ConnId, room: RoomId) -> bool {
        self.conns
            .read()
            .await
            .get(&conn)
            .is_some_and(|state| state.joined.contains(&room))
    }

    async fn send_to(&self, conn: ConnId, event: ServerEvent) {
        let tx = self
            .conns
            .read()
            .await
            .get(&conn)
            .map(|state| state.tx.clone());
        if let Some(tx) = tx {
            // 队列满说明这个客户端已经跟不上，丢掉这条事件即可；
            // 连接本身会在关闭或 detach 时清理，避免在这里递归回 disconnect
            let _ = tx.try_send(event);
        }
    }

    async fn send_error(&self, conn: ConnId, code: ErrorCode, message: &str) {
        self.send_to(
            conn,
            ServerEvent::Error {
                code,
                message: message.to_string(),
            },
        )
        .await;
    }

    async fn authenticate(&self, conn: ConnId, user: UserSummary, token: String) {
        if let Err(e) = self
            .accounts
            .ensure_member(self.default_room, user.id)
            .await
        {
            self.send_error(conn, ErrorCode::Internal, &e.to_string())
                .await;
            return;
        }
        {
            let mut conns = self.conns.write().await;
            if let Some(state) = conns.get_mut(&conn) {
                state.user = Some(user.clone());
            }
        }
        self.send_to(
            conn,
            ServerEvent::Authenticated {
                user: user.clone(),
                token,
            },
        )
        .await;
        self.send_rooms(conn, user.id).await;
        self.send_friends_presence(conn, user.id).await;
        self.broadcast_friend_presence(user.id, true).await;
        self.broadcast_presence(self.default_room).await;
    }

    async fn send_rooms(&self, conn: ConnId, user: UserId) {
        let mut rooms = match self.accounts.rooms(user).await {
            Ok(rooms) => rooms,
            Err(e) => {
                self.send_error(conn, ErrorCode::Internal, &e.to_string())
                    .await;
                return;
            }
        };
        self.fill_summaries(&mut rooms).await;
        self.fill_read_seqs(user, &mut rooms).await;
        self.send_to(conn, ServerEvent::Rooms { rooms }).await;
    }

    /// 会话摘要：最新序号与最后一条消息。
    async fn fill_summaries(&self, rooms: &mut [RoomSummary]) {
        for room in rooms {
            room.last_seq = self.messages.latest_seq(room.room).await.unwrap_or(0);
            if let Ok(recent) = self.messages.history(room.room, None, 1).await {
                if let Some(message) = recent.last() {
                    let text = message.text.replace(['\n', '\r'], " ");
                    room.last_text = Some(if message.kind == MessageKind::File {
                        "[文件]".to_string()
                    } else if room.kind == RoomKind::Group {
                        format!("{}: {text}", message.sender_name)
                    } else {
                        text
                    });
                    room.last_at = Some(message.server_ts_ms / 1000);
                }
            }
        }
    }

    /// 把最新会话列表推给某个在线用户的所有连接。
    async fn push_rooms_to(&self, user: UserId) {
        let Ok(mut rooms) = self.accounts.rooms(user).await else {
            return;
        };
        self.fill_summaries(&mut rooms).await;
        self.fill_read_seqs(user, &mut rooms).await;
        let targets: Vec<ConnId> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, conn)| conn.user.as_ref().is_some_and(|known| known.id == user))
            .map(|(id, _)| *id)
            .collect();
        for conn in targets {
            self.send_to(
                conn,
                ServerEvent::Rooms {
                    rooms: rooms.clone(),
                },
            )
            .await;
        }
    }

    async fn send_mention(&self, room: RoomId, from: &UserSummary, text: &str, target: UserId) {
        let targets: Vec<ConnId> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, conn)| conn.user.as_ref().is_some_and(|known| known.id == target))
            .map(|(id, _)| *id)
            .collect();
        for conn in targets {
            self.send_to(
                conn,
                ServerEvent::Mention {
                    room,
                    from: from.clone(),
                    text: text.to_string(),
                },
            )
            .await;
        }
    }

    /// 新消息/编辑/撤回后刷新所有成员（包括不在当前房间的人）的会话摘要。
    async fn push_room_summaries(&self, room: RoomId) {
        let Ok(members) = self.accounts.members_with_roles(room).await else {
            return;
        };
        for member in members {
            self.push_rooms_to(member.user.id).await;
        }
    }

    /// 填充每个会话的已读位置；首次没有记录时，把当前历史视为已读。
    async fn fill_read_seqs(&self, user: UserId, rooms: &mut [RoomSummary]) {
        for room in rooms {
            let read = match self.accounts.read_seq(user, room.room).await {
                Ok(Some(seq)) => seq,
                Ok(None) => {
                    let latest = self.messages.latest_seq(room.room).await.unwrap_or(0);
                    let _ = self.accounts.set_read_seq(user, room.room, latest).await;
                    latest
                }
                Err(_) => 0,
            };
            room.read_seq = read.min(room.last_seq);
        }
    }

    /// 把最新成员列表推给房间内所有在线成员。
    async fn push_members(&self, room: RoomId) {
        let Ok(members) = self.accounts.members_with_roles(room).await else {
            return;
        };
        let targets: Vec<ConnId> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, conn)| conn.joined.contains(&room))
            .map(|(id, _)| *id)
            .collect();
        for conn in targets {
            self.send_to(
                conn,
                ServerEvent::RoomMembers {
                    room,
                    members: members.clone(),
                },
            )
            .await;
        }
    }

    async fn is_online(&self, user: UserId) -> bool {
        self.conns
            .read()
            .await
            .values()
            .any(|conn| conn.user.as_ref().is_some_and(|known| known.id == user))
    }

    /// 把每个好友的在线状态推给这个连接。
    async fn send_friends_presence(&self, conn: ConnId, user: UserId) {
        let Ok(friends) = self.accounts.friends(user).await else {
            return;
        };
        for friend in friends {
            let online = self.is_online(friend.id).await;
            self.send_to(
                conn,
                ServerEvent::FriendPresence {
                    user: friend.id,
                    online,
                },
            )
            .await;
        }
    }

    /// 把某个用户的上下线状态推给他的好友。
    async fn broadcast_friend_presence(&self, user: UserId, online: bool) {
        let Ok(friends) = self.accounts.friends(user).await else {
            return;
        };
        let friend_ids: Vec<UserId> = friends.iter().map(|friend| friend.id).collect();
        let targets: Vec<ConnId> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, conn)| {
                conn.user
                    .as_ref()
                    .is_some_and(|known| friend_ids.contains(&known.id))
            })
            .map(|(id, _)| *id)
            .collect();
        for target in targets {
            self.send_to(target, ServerEvent::FriendPresence { user, online })
                .await;
        }
    }

    async fn send_friends(&self, conn: ConnId, user: UserId) {
        match self.accounts.friends(user).await {
            Ok(users) => self.send_to(conn, ServerEvent::Friends { users }).await,
            Err(e) => {
                self.send_error(conn, ErrorCode::Internal, &e.to_string())
                    .await;
            }
        }
    }

    async fn presence(&self, room: RoomId) -> u32 {
        self.conns
            .read()
            .await
            .values()
            .filter(|conn| conn.user.is_some() && conn.joined.contains(&room))
            .count() as u32
    }

    async fn broadcast_presence(&self, room: RoomId) {
        let online = self.presence(room).await;
        let targets: Vec<ConnId> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, conn)| conn.joined.contains(&room))
            .map(|(id, _)| *id)
            .collect();
        for conn in targets {
            self.send_to(
                conn,
                ServerEvent::Presence {
                    room,
                    online_count: online,
                },
            )
            .await;
        }
    }

    async fn broadcast_room(&self, room: RoomId, event: ServerEvent) {
        let targets: Vec<ConnId> = self
            .conns
            .read()
            .await
            .iter()
            .filter(|(_, conn)| conn.joined.contains(&room))
            .map(|(id, _)| *id)
            .collect();
        for conn in targets {
            self.send_to(conn, event.clone()).await;
        }
    }

    async fn disconnect(&self, conn: ConnId) {
        let mut rooms = Vec::new();
        let mut user = None;
        if let Some(state) = self.conns.write().await.remove(&conn) {
            rooms.extend(state.joined);
            user = state.user.map(|known| known.id);
        }
        for room in rooms {
            self.broadcast_presence(room).await;
        }
        if let Some(user) = user {
            if !self.is_online(user).await {
                self.broadcast_friend_presence(user, false).await;
            }
        }
    }

    async fn handle_upload(
        &self,
        conn: ConnId,
        room: RoomId,
        attachment: Attachment,
        data: Vec<u8>,
    ) -> Result<()> {
        let user = self
            .user_of(conn)
            .await
            .ok_or_else(|| ChatError::Protocol("未登录".into()))?;
        if !self
            .accounts
            .is_member(room, user.id)
            .await
            .map_err(|e| ChatError::Storage(e.to_string()))?
        {
            return Err(ChatError::Protocol("房间不存在".into()));
        }
        validate_attachment(&attachment)?;
        if attachment.size != data.len() as u64 || attachment.sha256 != sha256_hex(&data) {
            return Err(ChatError::Protocol("附件摘要与内容不一致".into()));
        }
        self.blobs.put(&attachment.sha256, &data).await
    }

    async fn handle_download(
        &self,
        conn: ConnId,
        room: RoomId,
        id: AttachmentId,
    ) -> Result<Vec<u8>> {
        let user = self
            .user_of(conn)
            .await
            .ok_or_else(|| ChatError::Protocol("未登录".into()))?;
        if !self
            .accounts
            .is_member(room, user.id)
            .await
            .map_err(|e| ChatError::Storage(e.to_string()))?
        {
            return Err(ChatError::Protocol("房间不存在".into()));
        }
        let attachment = self
            .messages
            .attachment(room, id)
            .await?
            .ok_or_else(|| ChatError::Protocol("附件不存在".into()))?;
        self.blobs.get(&attachment.sha256).await
    }

    async fn handle(&self, conn: ConnId, req: ClientRequest) {
        match req {
            ClientRequest::Register {
                username,
                password,
                display_name,
            } => match self
                .accounts
                .register(&username, &password, &display_name)
                .await
            {
                Ok((user, token)) => self.authenticate(conn, user, token).await,
                Err(e) => self.account_error(conn, e).await,
            },
            ClientRequest::Login { username, password } => {
                match self.accounts.login(&username, &password).await {
                    Ok((user, token)) => self.authenticate(conn, user, token).await,
                    Err(e) => self.account_error(conn, e).await,
                }
            }
            ClientRequest::Resume { token } => match self.accounts.resume(&token).await {
                Ok(Some(user)) => self.authenticate(conn, user, token).await,
                Ok(None) => self.send_to(conn, ServerEvent::AuthRequired).await,
                Err(e) => self.account_error(conn, e).await,
            },
            ClientRequest::Logout => {
                let user = self.user_of(conn).await;
                if let Some(state) = self.conns.write().await.get_mut(&conn) {
                    state.user = None;
                    state.joined.clear();
                }
                if let Some(user) = user {
                    if !self.is_online(user.id).await {
                        self.broadcast_friend_presence(user.id, false).await;
                    }
                }
                self.send_to(conn, ServerEvent::AuthRequired).await;
            }

            ClientRequest::ListFriends => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                self.send_friends(conn, user.id).await;
            }
            ClientRequest::AddFriend { username } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                match self.accounts.add_friend(user.id, &username).await {
                    Ok(users) => {
                        let friend = users
                            .iter()
                            .find(|known| known.username == username.trim().to_ascii_lowercase())
                            .cloned();
                        self.send_to(conn, ServerEvent::Friends { users }).await;
                        // 双方都更新，不必等对方重新登录
                        if let Some(friend) = friend {
                            self.push_friends(friend.id).await;
                        }
                        self.send_friends_presence(conn, user.id).await;
                        self.broadcast_friend_presence(user.id, true).await;
                    }
                    Err(e) => self.account_error(conn, e).await,
                }
            }

            ClientRequest::RemoveFriend { user: target } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if let Err(e) = self.accounts.remove_friend(user.id, target).await {
                    return self.account_error(conn, e).await;
                }
                self.push_friends(user.id).await;
                self.push_friends(target).await;
            }

            ClientRequest::SearchUsers { query } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                match self.accounts.search_users(user.id, &query).await {
                    Ok(users) => self.send_to(conn, ServerEvent::UserSearch { users }).await,
                    Err(e) => self.account_error(conn, e).await,
                }
            }

            ClientRequest::SearchMessages { room, query } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self
                    .accounts
                    .is_member(room, user.id)
                    .await
                    .unwrap_or(false)
                {
                    return self
                        .send_error(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                }
                match self.messages.search(room, &query, 50).await {
                    Ok(messages) => {
                        self.send_to(conn, ServerEvent::SearchResults { room, messages })
                            .await
                    }
                    Err(e) => {
                        self.send_error(conn, ErrorCode::Internal, &e.to_string())
                            .await
                    }
                }
            }

            ClientRequest::EditMessage { room, seq, text } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self.joined(conn, room).await {
                    return self
                        .send_error(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                }
                let text = text.trim();
                if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                    return self
                        .send_error(conn, ErrorCode::InvalidRequest, "消息为空或超过长度限制")
                        .await;
                }
                match self
                    .messages
                    .edit(room, seq, user.id, text.to_string())
                    .await
                {
                    Ok(Some(message)) => {
                        self.broadcast_room(room, ServerEvent::MessageEdited { message })
                            .await;
                        self.push_room_summaries(room).await;
                    }
                    Ok(None) => {
                        self.send_error(conn, ErrorCode::NotFound, "消息不存在或不是自己发的")
                            .await
                    }
                    Err(e) => {
                        self.send_error(conn, ErrorCode::Internal, &e.to_string())
                            .await
                    }
                }
            }

            ClientRequest::RecallMessage { room, seq } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self.joined(conn, room).await {
                    return self
                        .send_error(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                }
                match self.messages.recall(room, seq, user.id).await {
                    Ok(true) => {
                        self.broadcast_room(room, ServerEvent::MessageRecalled { room, seq })
                            .await;
                        self.push_room_summaries(room).await;
                    }
                    Ok(false) => {
                        self.send_error(conn, ErrorCode::NotFound, "消息不存在或不是自己发的")
                            .await
                    }
                    Err(e) => {
                        self.send_error(conn, ErrorCode::Internal, &e.to_string())
                            .await
                    }
                }
            }

            ClientRequest::MarkRead { room, seq } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self
                    .accounts
                    .is_member(room, user.id)
                    .await
                    .unwrap_or(false)
                {
                    return self
                        .send_error(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                }
                let latest = self.messages.latest_seq(room).await.unwrap_or(0);
                if let Err(e) = self
                    .accounts
                    .set_read_seq(user.id, room, seq.min(latest))
                    .await
                {
                    self.send_error(conn, ErrorCode::Internal, &e.to_string())
                        .await;
                }
            }

            ClientRequest::ListRooms => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                self.send_rooms(conn, user.id).await;
            }
            ClientRequest::CreateGroup { name, members } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                match self.accounts.create_group(user.id, &name, &members).await {
                    Ok(_) => {
                        let mut ids = Vec::with_capacity(members.len() + 1);
                        ids.push(user.id);
                        ids.extend(members);
                        ids.sort_by_key(|id| id.to_string());
                        ids.dedup();
                        for id in ids {
                            self.push_rooms_to(id).await;
                        }
                    }
                    Err(e) => self.account_error(conn, e).await,
                }
            }
            ClientRequest::OpenDirect { user: other } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                match self.accounts.open_direct(user.id, other).await {
                    Ok(_) => {
                        self.push_rooms_to(user.id).await;
                        self.push_rooms_to(other).await;
                    }
                    Err(e) => self.account_error(conn, e).await,
                }
            }
            ClientRequest::RoomMembers { room } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self
                    .accounts
                    .is_member(room, user.id)
                    .await
                    .unwrap_or(false)
                {
                    return self
                        .send_error(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                }
                match self.accounts.members_with_roles(room).await {
                    Ok(members) => {
                        self.send_to(conn, ServerEvent::RoomMembers { room, members })
                            .await
                    }
                    Err(e) => self.account_error(conn, e).await,
                }
            }

            ClientRequest::LeaveGroup { room } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                match self.accounts.member_role(room, user.id).await {
                    Ok(Some(MemberRole::Owner)) => {
                        self.send_error(conn, ErrorCode::Forbidden, "群主需先解散群聊")
                            .await;
                    }
                    Ok(Some(MemberRole::Member)) => {
                        if let Err(e) = self.accounts.remove_member(room, user.id).await {
                            return self.account_error(conn, e).await;
                        }
                        self.push_rooms_to(user.id).await;
                        self.send_to(conn, ServerEvent::Left { room }).await;
                        self.push_members(room).await;
                    }
                    Ok(None) | Err(_) => {
                        self.send_error(conn, ErrorCode::RoomMismatch, "房间不存在")
                            .await;
                    }
                }
            }

            ClientRequest::RemoveMember { room, user: target } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self.accounts.is_owner(room, user.id).await.unwrap_or(false) {
                    return self
                        .send_error(conn, ErrorCode::Forbidden, "只有群主能移出成员")
                        .await;
                }
                if target == user.id {
                    return self
                        .send_error(conn, ErrorCode::InvalidRequest, "不能移出自己")
                        .await;
                }
                match self.accounts.member_role(room, target).await {
                    Ok(Some(_)) => {
                        if let Err(e) = self.accounts.remove_member(room, target).await {
                            return self.account_error(conn, e).await;
                        }
                        self.push_rooms_to(target).await;
                        self.push_rooms_to(user.id).await;
                        let members = self
                            .accounts
                            .members_with_roles(room)
                            .await
                            .unwrap_or_default();
                        self.send_to(
                            conn,
                            ServerEvent::RoomMembers {
                                room,
                                members: members.clone(),
                            },
                        )
                        .await;
                        self.push_members(room).await;
                    }
                    Ok(None) | Err(_) => {
                        self.send_error(conn, ErrorCode::RoomMismatch, "成员不存在")
                            .await;
                    }
                }
            }

            ClientRequest::DissolveGroup { room } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self.accounts.is_owner(room, user.id).await.unwrap_or(false) {
                    return self
                        .send_error(conn, ErrorCode::Forbidden, "只有群主能解散群聊")
                        .await;
                }
                let members = self
                    .accounts
                    .members_with_roles(room)
                    .await
                    .unwrap_or_default();
                if let Err(e) = self.accounts.dissolve_group(room).await {
                    return self.account_error(conn, e).await;
                }
                for member in members {
                    self.push_rooms_to(member.user.id).await;
                }
            }

            ClientRequest::Join { room, profile } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self
                    .accounts
                    .is_member(room, user.id)
                    .await
                    .unwrap_or(false)
                {
                    return self
                        .send_error(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                }
                if let Some(state) = self.conns.write().await.get_mut(&conn) {
                    state.display_name = Some(profile.name);
                    state.joined.insert(room);
                }
                let latest_seq = self.messages.latest_seq(room).await.unwrap_or(0);
                let online_count = self.presence(room).await;
                self.send_to(
                    conn,
                    ServerEvent::Joined {
                        room,
                        you: user.id,
                        online_count,
                        latest_seq,
                    },
                )
                .await;
                self.broadcast_presence(room).await;
            }
            ClientRequest::Leave { room } => {
                if let Some(state) = self.conns.write().await.get_mut(&conn) {
                    state.joined.remove(&room);
                }
                self.send_to(conn, ServerEvent::Left { room }).await;
                self.broadcast_presence(room).await;
            }
            ClientRequest::Lobby { room } => {
                // 大厅摘要允许匿名查询：公网地址簿靠它在未登录时探测房间名与人数
                let Some(name) = self.accounts.room_name(room).await.unwrap_or(None) else {
                    return self
                        .send_error(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                };
                self.send_to(
                    conn,
                    ServerEvent::Lobby {
                        info: LobbyInfo {
                            room,
                            name,
                            online_count: self.presence(room).await,
                        },
                    },
                )
                .await;
            }

            ClientRequest::Send {
                room,
                client_msg_id,
                text,
                quote,
            } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self
                    .accounts
                    .is_member(room, user.id)
                    .await
                    .unwrap_or(false)
                    || !self.joined(conn, room).await
                {
                    return self
                        .send_error(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                }
                let text = text.trim();
                if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                    return self
                        .send_error(conn, ErrorCode::InvalidRequest, "消息为空或超过长度限制")
                        .await;
                }
                let sender_name = self
                    .display_name_of(conn)
                    .await
                    .unwrap_or_else(|| user.display_name.clone());
                let mentions = mentioned_usernames(text);
                let mention_text = text.to_string();
                let from = user.clone();
                let _publish = self.publish.lock().await;
                let msg = NewMessage {
                    client_msg_id,
                    sender: user.id,
                    sender_name,
                    kind: MessageKind::Text,
                    text: text.to_string(),
                    attachment: None,
                    quote: quote.map(sanitize_quote),
                };
                match self.messages.append(room, msg).await {
                    Ok(result) => {
                        if result.inserted {
                            self.broadcast_room(
                                room,
                                ServerEvent::Message {
                                    message: result.message,
                                },
                            )
                            .await;
                            for name in mentions {
                                if let Ok(Some(target)) = self.accounts.find_user(&name).await {
                                    if target.id != from.id {
                                        self.send_mention(room, &from, &mention_text, target.id)
                                            .await;
                                    }
                                }
                            }
                            self.push_room_summaries(room).await;
                        } else {
                            self.send_to(
                                conn,
                                ServerEvent::Message {
                                    message: result.message,
                                },
                            )
                            .await;
                        }
                    }
                    Err(ChatError::Protocol(_)) => {
                        self.send_error(
                            conn,
                            ErrorCode::InvalidRequest,
                            "消息标识与原消息内容不一致",
                        )
                        .await
                    }
                    Err(_) => {
                        self.send_error(conn, ErrorCode::Internal, "消息保存失败")
                            .await
                    }
                }
            }
            ClientRequest::SendFile {
                room,
                client_msg_id,
                attachment,
            } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self
                    .accounts
                    .is_member(room, user.id)
                    .await
                    .unwrap_or(false)
                    || !self.joined(conn, room).await
                {
                    return self
                        .send_error(conn, ErrorCode::NotJoined, "未加入房间")
                        .await;
                }
                if let Err(e) = validate_attachment(&attachment) {
                    return self
                        .send_error(conn, ErrorCode::InvalidRequest, &e.to_string())
                        .await;
                }
                if !self.blobs.contains(&attachment.sha256).await {
                    return self
                        .send_error(conn, ErrorCode::NotFound, "附件尚未上传完成")
                        .await;
                }
                let sender_name = self
                    .display_name_of(conn)
                    .await
                    .unwrap_or_else(|| user.display_name.clone());
                let _publish = self.publish.lock().await;
                let msg = NewMessage {
                    client_msg_id,
                    sender: user.id,
                    sender_name,
                    kind: MessageKind::File,
                    text: String::new(),
                    attachment: Some(attachment),
                    quote: None,
                };
                match self.messages.append(room, msg).await {
                    Ok(result) if result.inserted => {
                        self.broadcast_room(
                            room,
                            ServerEvent::Message {
                                message: result.message,
                            },
                        )
                        .await;
                        self.push_room_summaries(room).await;
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
                        self.send_error(conn, ErrorCode::Internal, "消息保存失败")
                            .await
                    }
                }
            }
            ClientRequest::History {
                room,
                before,
                limit,
            } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self
                    .accounts
                    .is_member(room, user.id)
                    .await
                    .unwrap_or(false)
                {
                    return self
                        .send_error(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                }
                match self.messages.history(room, before, limit as usize).await {
                    Ok(messages) => {
                        let has_more = messages.len() == limit as usize;
                        self.send_to(
                            conn,
                            ServerEvent::History {
                                room,
                                messages,
                                has_more,
                            },
                        )
                        .await;
                    }
                    Err(_) => {
                        self.send_error(conn, ErrorCode::Internal, "读取历史失败")
                            .await
                    }
                }
            }
            ClientRequest::Sync { room, after, limit } => {
                let Some(user) = self.user_of(conn).await else {
                    return self.send_to(conn, ServerEvent::AuthRequired).await;
                };
                if !self
                    .accounts
                    .is_member(room, user.id)
                    .await
                    .unwrap_or(false)
                {
                    return self
                        .send_error(conn, ErrorCode::RoomMismatch, "房间不存在")
                        .await;
                }
                match self.messages.since(room, after, limit as usize).await {
                    Ok(messages) => {
                        let has_more = messages.len() == limit as usize;
                        self.send_to(
                            conn,
                            ServerEvent::Sync {
                                room,
                                messages,
                                has_more,
                            },
                        )
                        .await;
                    }
                    Err(_) => self.send_error(conn, ErrorCode::Internal, "同步失败").await,
                }
            }
        }
    }

    async fn account_error(&self, conn: ConnId, error: AccountError) {
        let (code, message) = match &error {
            AccountError::Invalid(message) => (ErrorCode::InvalidRequest, message.clone()),
            AccountError::UsernameTaken => (ErrorCode::Conflict, error.to_string()),
            AccountError::BadCredentials | AccountError::Unauthorized => {
                (ErrorCode::Unauthorized, error.to_string())
            }
            AccountError::Storage(message) => (ErrorCode::Internal, message.clone()),
        };
        self.send_error(conn, code, &message).await;
    }
}

fn mentioned_usernames(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|word| word.strip_prefix('@'))
        .map(|name| {
            name.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_ascii_lowercase()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

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

/// 服务端一侧的请求入口，QUIC 与进程内测试共用。
#[derive(Clone)]
pub struct HubHandle {
    hub: Arc<Hub>,
    conn: ConnId,
    device: DeviceId,
}

impl HubHandle {
    pub fn device(&self) -> DeviceId {
        self.device
    }
}

#[async_trait]
impl RequestSink for HubHandle {
    async fn dispatch(&self, req: ClientRequest) -> Result<()> {
        self.hub.handle(self.conn, req).await;
        Ok(())
    }

    async fn detach(&self) {
        self.hub.disconnect(self.conn).await;
    }

    async fn upload(&self, room: RoomId, attachment: Attachment, data: Vec<u8>) -> Result<()> {
        self.hub
            .handle_upload(self.conn, room, attachment, data)
            .await
    }

    async fn download(&self, room: RoomId, id: AttachmentId) -> Result<Vec<u8>> {
        self.hub.handle_download(self.conn, room, id).await
    }
}

pub struct HubAcceptor(pub Arc<Hub>);

#[async_trait]
impl ConnectionAcceptor for HubAcceptor {
    fn room_info(&self) -> RoomInfo {
        RoomInfo {
            room: self.0.default_room,
            name: self.0.name().to_string(),
        }
    }

    async fn accept(&self, device: DeviceId) -> (Arc<dyn RequestSink>, EventStream) {
        let (handle, rx) = self.0.attach(device).await;
        (Arc::new(handle), rx)
    }
}

pub struct HubConnection {
    handle: HubHandle,
    rx: Mutex<mpsc::Receiver<ServerEvent>>,
    closed: AtomicBool,
    notify: Notify,
}

#[async_trait]
impl Connection for HubConnection {
    async fn request(&self, req: ClientRequest) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        self.handle.dispatch(req).await
    }

    async fn next_event(&self) -> Result<ServerEvent> {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.closed.load(Ordering::Acquire) {
            return Err(ChatError::Closed);
        }
        let mut rx = self.rx.lock().await;
        tokio::select! {
            event = rx.recv() => event.ok_or(ChatError::Closed),
            _ = &mut notified => Err(ChatError::Closed),
        }
    }

    async fn close(&self) -> Result<()> {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
            self.handle.detach().await;
        }
        Ok(())
    }

    async fn upload(&self, room: RoomId, attachment: Attachment, data: Vec<u8>) -> Result<()> {
        self.handle.upload(room, attachment, data).await
    }

    async fn download(&self, room: RoomId, id: AttachmentId) -> Result<Vec<u8>> {
        self.handle.download(room, id).await
    }
}

impl Drop for HubConnection {
    fn drop(&mut self) {
        let handle = self.handle.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { handle.detach().await });
        }
    }
}
