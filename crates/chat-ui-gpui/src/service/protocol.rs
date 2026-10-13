//! 网络协议 → 界面事件 的翻译。
//!
//! 这是一层刻意的收窄：服务端可能返回的东西很多，界面真正要处理的只有这几种。
//! 收窄集中在这里，[`super::session`] 就只管连接本身。

use super::*;

/// 服务端事件 → 界面事件。
pub(super) fn map_event(event: ServerEvent) -> UiEvent {
    match event {
        ServerEvent::Lobby { info } => UiEvent::Presence {
            room: info.room,
            online: info.online_count,
        },
        ServerEvent::Joined {
            room,
            you,
            online_count,
            ..
        } => UiEvent::Joined {
            room,
            you,
            online: online_count,
        },
        ServerEvent::Left { room } => UiEvent::Left { room },
        ServerEvent::Message { message } => UiEvent::Message {
            room: message.room,
            sender_id: message.sender,
            sender: message.sender_name,
            text: message.text,
        },
        ServerEvent::Presence { room, online_count } => UiEvent::Presence {
            room,
            online: online_count,
        },
        ServerEvent::History { .. } => UiEvent::Status("房间历史已加载".into()),
        ServerEvent::Error { message, .. } => UiEvent::Error(message),
    }
}

/// 从事件流里捞出大厅应答；超时返回 `None`。
///
/// 首次连接与房间探测都要读一次大厅摘要（房间名 + 在线人数），而应答是混在
/// 事件流里回来的，所以要在这里等它。
pub(super) async fn await_lobby(client: &Client) -> Option<LobbyInfo> {
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(ServerEvent::Lobby { info })) => return Some(info),
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
}
