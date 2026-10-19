//! 客户端门面：对上层暴露带类型的房间操作，屏蔽具体传输实现。
//!
//! 换传输（进程内 / QUIC / WebSocket）不影响调用方。

use std::sync::Arc;

use uuid::Uuid;

use crate::proto::{ClientRequest, Profile, RoomId, Seq, ServerEvent};
use crate::{Connection, Result};

#[derive(Clone)]
pub struct Client {
    conn: Arc<dyn Connection>,
}

impl Client {
    pub fn new(conn: Arc<dyn Connection>) -> Self {
        Self { conn }
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
        self.conn
            .request(ClientRequest::Send {
                room,
                client_msg_id,
                text: text.into(),
            })
            .await
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
