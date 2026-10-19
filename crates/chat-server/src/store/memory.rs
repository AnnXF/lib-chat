//! 进程内消息存储：不落盘，进程结束即消失。

use std::collections::HashMap;

use async_trait::async_trait;
use chat_core::{AppendResult, ChatError, Message, MessageStore, NewMessage, Result, RoomId, Seq};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::now_ms;

#[derive(Default)]
struct RoomLog {
    next_seq: Seq,
    messages: Vec<Message>,
    ids: HashMap<Uuid, usize>,
}

/// 进程内消息存储，按房间维护单调序号。
#[derive(Default)]
pub struct InMemoryStore {
    inner: Mutex<HashMap<RoomId, RoomLog>>,
}

#[async_trait]
impl MessageStore for InMemoryStore {
    async fn append(&self, room: RoomId, msg: NewMessage) -> Result<AppendResult> {
        let mut guard = self.inner.lock().await;
        let log = guard.entry(room).or_default();
        if let Some(&index) = log.ids.get(&msg.client_msg_id) {
            let stored = &log.messages[index];
            if stored.text != msg.text || stored.kind != msg.kind {
                return Err(ChatError::Protocol("消息标识与原始内容不一致".into()));
            }
            return Ok(AppendResult {
                message: stored.clone(),
                inserted: false,
            });
        }
        log.next_seq += 1;
        let stored = Message {
            room,
            seq: log.next_seq,
            client_msg_id: msg.client_msg_id,
            sender: msg.sender,
            sender_name: msg.sender_name,
            kind: msg.kind,
            text: msg.text,
            server_ts_ms: now_ms(),
        };
        log.ids.insert(stored.client_msg_id, log.messages.len());
        log.messages.push(stored.clone());
        Ok(AppendResult {
            message: stored,
            inserted: true,
        })
    }

    async fn history(
        &self,
        room: RoomId,
        before: Option<Seq>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let guard = self.inner.lock().await;
        let Some(log) = guard.get(&room) else {
            return Ok(Vec::new());
        };

        // `messages` 按 seq 递增，所以「早于 cutoff」的部分一定是个前缀，可以二分定位。
        // 只克隆末尾的 limit 条——先 clone 全部再截断会让耗时随房间总量线性增长，
        // 取 50 条也要把几万条消息连同正文复制一遍。
        let cutoff = before.unwrap_or(Seq::MAX);
        let end = log.messages.partition_point(|m| m.seq < cutoff);
        let start = end.saturating_sub(limit);
        Ok(log.messages[start..end].to_vec())
    }

    async fn since(&self, room: RoomId, after: Seq, limit: usize) -> Result<Vec<Message>> {
        let guard = self.inner.lock().await;
        let Some(log) = guard.get(&room) else {
            return Ok(Vec::new());
        };

        // 与 `history` 同理：按 seq 递增二分定位，只克隆命中的那段。
        let start = log.messages.partition_point(|m| m.seq <= after);
        let end = (start + limit).min(log.messages.len());
        Ok(log.messages[start..end].to_vec())
    }

    async fn latest_seq(&self, room: RoomId) -> Result<Seq> {
        let guard = self.inner.lock().await;
        Ok(guard.get(&room).map(|l| l.next_seq).unwrap_or(0))
    }
}
