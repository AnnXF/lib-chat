//! 集成测试共用的隔离与等待工具。
//!
//! 各测试文件各持一把串行锁与一段端口区间，避免不同二进制之间抢占广播端口。

#![allow(dead_code)]

use std::sync::MutexGuard;
use std::time::{Duration, Instant};

use chat_transport_quic::Liveness;
use chat_ui_gpui::service::{UiCommand, UiEvent, spawn_with_data};

type Cmds = flume::Sender<UiCommand>;
type Events = flume::Receiver<UiEvent>;

/// 串行执行并分配本文件专属的广播端口。
pub fn serial(base_port: u16) -> (MutexGuard<'static, ()>, u16) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let offset = NEXT_PORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(400));

    let port = base_port + offset;
    let prefix = format!("libchat-e2e-{port}-");
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    (guard, port)
}

/// 每个后端一个独占的库目录。
pub fn test_data_dir(beacon_port: u16) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!("libchat-e2e-{beacon_port}-{n}"))
}

pub fn spawn_test(beacon_port: u16) -> (Cmds, Events) {
    spawn_with_data(beacon_port, Liveness::default(), test_data_dir(beacon_port))
}

pub fn recv_until<F>(events: &Events, pred: F) -> UiEvent
where
    F: Fn(&UiEvent) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = None;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                if pred(&event) {
                    return event;
                }
                last = Some(event);
            }
            Err(_) => continue,
        }
    }
    panic!("未在超时内收到期望事件；最后收到: {last:?}");
}

/// 重连期间的等待：收到 `Disconnected` 直接失败。
pub fn recv_until_keeping_room<F>(events: &Events, pred: F) -> UiEvent
where
    F: Fn(&UiEvent) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(UiEvent::Disconnected) => panic!("重连期间不应把用户踢出房间"),
            Ok(event) => {
                if pred(&event) {
                    return event;
                }
            }
            Err(_) => continue,
        }
    }
    panic!("未在超时内收到期望事件");
}
