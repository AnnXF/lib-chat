//! 服务端：房间状态、在线人数、消息落库与扇出。

mod accounts;
mod hub;
pub mod obs;
mod server;
mod store;

pub use accounts::{AccountError, Accounts};
pub use hub::{Hub, HubAcceptor, HubConnection, HubHandle};
pub use obs::Counters;
pub use server::{Acceptor, ConnId, InProcConnection, Server, ServerHandle};
pub use store::{BlobStore, DiskBlobs, InMemoryStore, MemoryBlobs, SqliteStore};
