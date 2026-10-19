//! 客户端与服务端共享的协议定义。与具体传输方式无关。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 会话内单调递增的消息序号，由服务端分配，构成全序。
pub type Seq = i64;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

id_type!(UserId);
id_type!(DeviceId);
id_type!(RoomId);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub room: RoomId,
    pub seq: Seq,
    pub client_msg_id: Uuid,
    pub sender: UserId,
    pub sender_name: String,
    pub kind: MessageKind,
    pub text: String,
    pub server_ts_ms: i64,
}

/// 大厅摘要：未加入房间者也能获取，不含任何消息内容。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LobbyInfo {
    pub room: RoomId,
    pub name: String,
    /// 当前在线人数。房间没有「总人数」的概念：连接断开即离开。
    pub online_count: u32,
}

/// 服务端当前托管的房间信息，握手时下发给客户端。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    pub room: RoomId,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    /// 查询大厅摘要，无需加入。
    Lobby { room: RoomId },
    Join { room: RoomId, profile: Profile },
    Leave { room: RoomId },
    Send {
        room: RoomId,
        client_msg_id: Uuid,
        text: String,
    },
    History {
        room: RoomId,
        before: Option<Seq>,
        limit: u32,
    },
    /// 补齐缺口：取 `after` 之后的增量，用于重连后追平断线期间的消息。
    Sync {
        room: RoomId,
        after: Seq,
        limit: u32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Lobby {
        info: LobbyInfo,
    },
    Joined {
        room: RoomId,
        you: UserId,
        online_count: u32,
        latest_seq: Seq,
    },
    Left {
        room: RoomId,
    },
    Message {
        message: Message,
    },
    History {
        room: RoomId,
        messages: Vec<Message>,
        has_more: bool,
    },
    /// [`ClientRequest::Sync`] 的应答，按 seq 升序。`has_more` 表示之后还有更新。
    Sync {
        room: RoomId,
        messages: Vec<Message>,
        has_more: bool,
    },
    Presence {
        room: RoomId,
        online_count: u32,
    },
    Error { code: ErrorCode, message: String },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotJoined,
    AlreadyJoined,
    RoomMismatch,
    InvalidRequest,
    Internal,
}
