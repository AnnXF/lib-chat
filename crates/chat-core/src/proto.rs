//! 客户端与服务端共享的协议定义。与具体传输方式无关。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 会话内单调递增的消息序号，由服务端分配，构成全序。
pub type Seq = i64;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize,
        )]
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
id_type!(AttachmentId);

/// 单个附件的上限。超过就拒绝，避免一次传输把房间拖死。
pub const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    File,
    System,
}

/// 附件元数据。内容本身不随消息下发，按 `id` 单独取。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    /// 原始文件名，仅用于展示与保存时的默认名。
    pub name: String,
    pub size: u64,
    /// 按扩展名猜测的 MIME；猜不出时用 `application/octet-stream`。
    pub mime: String,
    /// 内容 SHA-256（十六进制小写），用于校验与去重。
    pub sha256: String,
}

/// 被引用的消息摘要：只保留展示所需的最少信息，不递归展开引用链。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub sender: String,
    /// 文本消息为正文，文件消息为文件名。
    pub text: String,
    /// 被引用消息的序号，便于定位与去重。
    pub seq: Option<Seq>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub room: RoomId,
    pub seq: Seq,
    pub client_msg_id: Uuid,
    pub sender: UserId,
    pub sender_name: String,
    pub kind: MessageKind,
    /// 文本内容；文件消息为空串。
    pub text: String,
    /// 文件消息附带元数据。
    pub attachment: Option<Attachment>,
    /// 这条消息引用了哪条消息；没有引用为 `None`。
    pub quote: Option<Quote>,
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
    Lobby {
        room: RoomId,
    },
    Join {
        room: RoomId,
        profile: Profile,
    },
    Leave {
        room: RoomId,
    },
    Send {
        room: RoomId,
        client_msg_id: Uuid,
        text: String,
        /// 引用某条消息时带上摘要；普通消息为 `None`。
        quote: Option<Quote>,
    },
    /// 文件消息：附件内容必须已经上传完成，服务端只校验并广播元数据。
    SendFile {
        room: RoomId,
        client_msg_id: Uuid,
        attachment: Attachment,
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

// 消息事件本来就带着整条消息，装箱只会给每条消息多一次分配；
// 其余变体很小，内联更划算。
#[allow(clippy::large_enum_variant)]
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
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotJoined,
    AlreadyJoined,
    RoomMismatch,
    InvalidRequest,
    /// 附件超过 [`MAX_ATTACHMENT_BYTES`]。
    TooLarge,
    /// 附件不存在或尚未上传完成。
    NotFound,
    Internal,
}
