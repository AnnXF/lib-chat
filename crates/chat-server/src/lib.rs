//! 服务端：房间状态、在线人数、消息落库与扇出。

mod server;
mod store;

pub use server::{Acceptor, ConnId, InProcConnection, Server, ServerHandle};
pub use store::{BlobStore, DiskBlobs, InMemoryStore, MemoryBlobs, SqliteStore};
