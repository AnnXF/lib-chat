//! 消息存储的两个实现。
//!
//! - 进程内：不落盘，命令行与测试用
//! - SQLite：房主落库，房间标识与消息同库，重启后历史仍读得回来
//!
//! 服务端只认 `Arc<dyn MessageStore>`，换实现不动房间逻辑。

mod memory;
mod sqlite;

pub use memory::InMemoryStore;
pub use sqlite::SqliteStore;

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前时间戳（毫秒）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
