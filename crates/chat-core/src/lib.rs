//! 领域模型与端口定义。上层依赖这些抽象，不依赖任何具体传输或存储实现。

use chat_proto::{
    ClientRequest, DeviceId, Message, MessageKind, RoomId, RoomInfo, Seq, ServerEvent, UserId,
};
use tokio::sync::mpsc;
use uuid::Uuid;

pub type Result<T, E = ChatError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("传输错误: {0}")]
    Transport(String),
    #[error("协议错误: {0}")]
    Protocol(String),
    #[error("连接已关闭")]
    Closed,
}

// ---------------------------------------------------------------- 连接

/// 客户端与服务端之间的一条连接。
///
/// 实现可以是进程内直连、QUIC——上层无需感知差异。
#[async_trait::async_trait]
pub trait Connection: Send + Sync {
    /// 发送请求。响应通过 [`Connection::next_event`] 异步返回。
    async fn request(&self, req: ClientRequest) -> Result<()>;
    /// 等待下一个服务端事件。
    async fn next_event(&self) -> Result<ServerEvent>;
    /// 主动断开连接。
    async fn close(&self) -> Result<()>;
}

/// 服务端向某条连接推送事件的口子。
pub type EventStream = mpsc::UnboundedReceiver<ServerEvent>;

/// 一条连接的请求入口，传输层把收到的请求交给它。
#[async_trait::async_trait]
pub trait RequestSink: Send + Sync {
    async fn dispatch(&self, req: ClientRequest) -> Result<()>;
    async fn detach(&self);
}

/// 服务端接入能力，供传输层在接受新连接时调用。
#[async_trait::async_trait]
pub trait ConnectionAcceptor: Send + Sync {
    /// 托管的房间，握手时下发给客户端，客户端据此发起 lobby/join。
    fn room_info(&self) -> RoomInfo;
    async fn accept(&self, device: DeviceId) -> (Box<dyn RequestSink>, EventStream);
}

// ---------------------------------------------------------------- 存储

/// 一条待写入的新消息（尚未分配 seq）。
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub client_msg_id: Uuid,
    pub sender: UserId,
    pub sender_name: String,
    pub kind: MessageKind,
    pub text: String,
}

/// 消息持久化端口。服务端负责分配会话内单调序号。
#[async_trait::async_trait]
pub trait MessageStore: Send + Sync {
    /// 追加消息，返回分配了 `seq` 的完整消息。
    async fn append(&self, room: RoomId, msg: NewMessage) -> Result<Message>;
    /// 取 `before`（不含）之前的最近 `limit` 条，按 seq 升序返回。
    async fn history(&self, room: RoomId, before: Option<Seq>, limit: usize) -> Result<Vec<Message>>;
    /// 当前最大序号，空房间为 0。
    async fn latest_seq(&self, room: RoomId) -> Result<Seq>;
}
