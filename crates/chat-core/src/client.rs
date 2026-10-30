//! 客户端门面：对上层暴露带类型的房间操作，屏蔽具体传输实现。
//!
//! 换传输（进程内 / QUIC / WebSocket）不影响调用方。

use std::sync::Arc;

use uuid::Uuid;

use crate::proto::{
    Attachment, AttachmentId, ClientRequest, Profile, Quote, RoomId, Seq, ServerEvent, UserId,
};
use crate::{Connection, Result};

#[derive(Clone)]
pub struct Client {
    conn: Arc<dyn Connection>,
}

impl Client {
    pub fn new(conn: Arc<dyn Connection>) -> Self {
        Self { conn }
    }

    // ---------------------------------------------------------------- 账号
    pub async fn register(
        &self,
        username: impl Into<String>,
        password: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Result<()> {
        self.conn
            .request(ClientRequest::Register {
                username: username.into(),
                password: password.into(),
                display_name: display_name.into(),
            })
            .await
    }

    pub async fn login(
        &self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<()> {
        self.conn
            .request(ClientRequest::Login {
                username: username.into(),
                password: password.into(),
            })
            .await
    }

    pub async fn resume(&self, token: impl Into<String>) -> Result<()> {
        self.conn
            .request(ClientRequest::Resume {
                token: token.into(),
            })
            .await
    }

    pub async fn logout(&self) -> Result<()> {
        self.conn.request(ClientRequest::Logout).await
    }

    // ---------------------------------------------------------------- 好友与会话
    pub async fn list_friends(&self) -> Result<()> {
        self.conn.request(ClientRequest::ListFriends).await
    }

    pub async fn add_friend(&self, username: impl Into<String>) -> Result<()> {
        self.conn
            .request(ClientRequest::AddFriend {
                username: username.into(),
            })
            .await
    }

    pub async fn remove_friend(&self, user: UserId) -> Result<()> {
        self.conn
            .request(ClientRequest::RemoveFriend { user })
            .await
    }

    pub async fn search_users(&self, query: impl Into<String>) -> Result<()> {
        self.conn
            .request(ClientRequest::SearchUsers {
                query: query.into(),
            })
            .await
    }

    pub async fn edit_message(
        &self,
        room: RoomId,
        seq: Seq,
        text: impl Into<String>,
    ) -> Result<()> {
        self.conn
            .request(ClientRequest::EditMessage {
                room,
                seq,
                text: text.into(),
            })
            .await
    }

    pub async fn mark_read(&self, room: RoomId, seq: Seq) -> Result<()> {
        self.conn
            .request(ClientRequest::MarkRead { room, seq })
            .await
    }

    pub async fn recall_message(&self, room: RoomId, seq: Seq) -> Result<()> {
        self.conn
            .request(ClientRequest::RecallMessage { room, seq })
            .await
    }

    pub async fn search_messages(&self, room: RoomId, query: impl Into<String>) -> Result<()> {
        self.conn
            .request(ClientRequest::SearchMessages {
                room,
                query: query.into(),
            })
            .await
    }

    pub async fn list_rooms(&self) -> Result<()> {
        self.conn.request(ClientRequest::ListRooms).await
    }

    pub async fn create_group(&self, name: impl Into<String>, members: Vec<UserId>) -> Result<()> {
        self.conn
            .request(ClientRequest::CreateGroup {
                name: name.into(),
                members,
            })
            .await
    }

    pub async fn open_direct(&self, user: UserId) -> Result<()> {
        self.conn.request(ClientRequest::OpenDirect { user }).await
    }

    pub async fn room_members(&self, room: RoomId) -> Result<()> {
        self.conn.request(ClientRequest::RoomMembers { room }).await
    }

    pub async fn leave_group(&self, room: RoomId) -> Result<()> {
        self.conn.request(ClientRequest::LeaveGroup { room }).await
    }

    pub async fn remove_member(&self, room: RoomId, user: UserId) -> Result<()> {
        self.conn
            .request(ClientRequest::RemoveMember { room, user })
            .await
    }

    pub async fn dissolve_group(&self, room: RoomId) -> Result<()> {
        self.conn
            .request(ClientRequest::DissolveGroup { room })
            .await
    }

    /// 查询大厅摘要，无需加入房间。
    pub async fn lobby(&self, room: RoomId) -> Result<()> {
        self.conn.request(ClientRequest::Lobby { room }).await
    }

    pub async fn join(&self, room: RoomId, name: impl Into<String>) -> Result<()> {
        self.conn
            .request(ClientRequest::Join {
                room,
                profile: Profile { name: name.into() },
            })
            .await
    }

    pub async fn leave(&self, room: RoomId) -> Result<()> {
        self.conn.request(ClientRequest::Leave { room }).await
    }

    pub async fn send_text(&self, room: RoomId, text: impl Into<String>) -> Result<()> {
        self.send_text_with_id(room, Uuid::new_v4(), text).await
    }

    pub async fn send_text_with_id(
        &self,
        room: RoomId,
        client_msg_id: Uuid,
        text: impl Into<String>,
    ) -> Result<()> {
        self.send_text_quoted(room, client_msg_id, text, None).await
    }

    /// 发送文本，并可附带对被引用消息的摘要。
    pub async fn send_text_quoted(
        &self,
        room: RoomId,
        client_msg_id: Uuid,
        text: impl Into<String>,
        quote: Option<Quote>,
    ) -> Result<()> {
        self.conn
            .request(ClientRequest::Send {
                room,
                client_msg_id,
                text: text.into(),
                quote,
            })
            .await
    }

    /// 发送一条文件消息；附件内容需先用 [`Client::upload`] 上传完成。
    pub async fn send_file(
        &self,
        room: RoomId,
        client_msg_id: Uuid,
        attachment: Attachment,
    ) -> Result<()> {
        self.conn
            .request(ClientRequest::SendFile {
                room,
                client_msg_id,
                attachment,
            })
            .await
    }

    /// 上传附件内容。元数据里的 `size` 与 `sha256` 必须与 `data` 一致。
    pub async fn upload(&self, room: RoomId, attachment: Attachment, data: Vec<u8>) -> Result<()> {
        self.conn.upload(room, attachment, data).await
    }

    /// 下载附件内容。
    pub async fn download(&self, room: RoomId, id: AttachmentId) -> Result<Vec<u8>> {
        self.conn.download(room, id).await
    }

    pub async fn history(&self, room: RoomId, before: Option<Seq>, limit: u32) -> Result<()> {
        self.conn
            .request(ClientRequest::History {
                room,
                before,
                limit,
            })
            .await
    }

    /// 补齐 `after` 之后的增量，用于重连后追平断线期间的消息。
    pub async fn sync(&self, room: RoomId, after: Seq, limit: u32) -> Result<()> {
        self.conn
            .request(ClientRequest::Sync { room, after, limit })
            .await
    }

    /// 等待下一个服务端事件。
    pub async fn next_event(&self) -> Result<ServerEvent> {
        self.conn.next_event().await
    }

    pub async fn close(&self) -> Result<()> {
        self.conn.close().await
    }
}
