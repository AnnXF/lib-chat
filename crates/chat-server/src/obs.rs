//! 服务端观测：一行一条的结构化日志 + 进程内计数器。
//!
//! 不引入日志框架：直接写 stderr，字段统一 `key=value`，
//! 出问题时 `grep event=message_appended room=...` 就能定位。
//! 计数器只活在进程里，进程退出前打印一次即可。

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

static CONNECTIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static CONNECTIONS_CLOSED: AtomicU64 = AtomicU64::new(0);
static MESSAGES_APPENDED: AtomicU64 = AtomicU64::new(0);
static EVENTS_DROPPED: AtomicU64 = AtomicU64::new(0);
static AUTH_FAILURES: AtomicU64 = AtomicU64::new(0);

/// 打开一条连接。
pub fn connection_opened() {
    CONNECTIONS_OPENED.fetch_add(1, Ordering::Relaxed);
}

/// 关闭一条连接。
pub fn connection_closed() {
    CONNECTIONS_CLOSED.fetch_add(1, Ordering::Relaxed);
}

/// 写入一条消息。
pub fn message_appended() {
    MESSAGES_APPENDED.fetch_add(1, Ordering::Relaxed);
}

/// 事件队列满，丢弃了一条推送。
pub fn event_dropped() {
    EVENTS_DROPPED.fetch_add(1, Ordering::Relaxed);
}

/// 登录或恢复登录失败。
pub fn auth_failed(reason: &str) {
    AUTH_FAILURES.fetch_add(1, Ordering::Relaxed);
    emit("warn", "auth_failed", &[("reason", reason.to_string())]);
}

/// 记录一条普通事件。
pub fn info(event: &str, fields: &[(&str, String)]) {
    emit("info", event, fields);
}

/// 记录一条告警。
pub fn warn(event: &str, fields: &[(&str, String)]) {
    emit("warn", event, fields);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub connections_opened: u64,
    pub connections_closed: u64,
    pub messages_appended: u64,
    pub events_dropped: u64,
    pub auth_failures: u64,
}

impl Counters {
    pub fn snapshot() -> Self {
        Self {
            connections_opened: CONNECTIONS_OPENED.load(Ordering::Relaxed),
            connections_closed: CONNECTIONS_CLOSED.load(Ordering::Relaxed),
            messages_appended: MESSAGES_APPENDED.load(Ordering::Relaxed),
            events_dropped: EVENTS_DROPPED.load(Ordering::Relaxed),
            auth_failures: AUTH_FAILURES.load(Ordering::Relaxed),
        }
    }
}

impl std::fmt::Display for Counters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "连接 {}/{}，消息 {}，丢弃事件 {}，登录失败 {}",
            self.connections_closed,
            self.connections_opened,
            self.messages_appended,
            self.events_dropped,
            self.auth_failures
        )
    }
}

fn emit(level: &str, event: &str, fields: &[(&str, String)]) {
    let mut line = format!("[{level}] event={event}");
    for (key, value) in fields {
        // 值里可能有空格，统一用引号包起来
        line.push(' ');
        line.push_str(key);
        line.push_str("=\"");
        line.push_str(&value.replace('"', "'"));
        line.push('"');
    }
    let _guard = lock();
    let mut out = std::io::stderr().lock();
    let _ = writeln!(out, "{line}");
}

/// 多线程下保证一行不被另一行插断。
fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_only_move_forward() {
        let before = Counters::snapshot();
        connection_opened();
        connection_closed();
        message_appended();
        event_dropped();
        let after = Counters::snapshot();
        assert_eq!(after.connections_opened, before.connections_opened + 1);
        assert_eq!(after.connections_closed, before.connections_closed + 1);
        assert_eq!(after.messages_appended, before.messages_appended + 1);
        assert_eq!(after.events_dropped, before.events_dropped + 1);
    }

    #[test]
    fn counters_print_every_field() {
        let text = Counters::snapshot().to_string();
        for label in ["连接", "消息", "丢弃事件", "登录失败"] {
            assert!(text.contains(label), "{text} 缺少 {label}");
        }
    }
}
