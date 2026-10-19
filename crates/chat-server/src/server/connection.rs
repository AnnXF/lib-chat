//! 服务端连接适配：传输接入点与进程内连接。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chat_core::{
    Attachment, AttachmentId, ChatError, ClientRequest, Connection, ConnectionAcceptor, DeviceId,
    EventStream, RequestSink, Result, RoomId, RoomInfo, ServerEvent,
};
use tokio::sync::{mpsc, Mutex, Notify};

use super::{ConnId, Server};

/// 一条已注册连接的请求入口，传输层把收到的请求交给它即可。
#[derive(Clone)]
pub struct ServerHandle {
    server: Arc<Server>,
    conn: ConnId,
    device: DeviceId,
}

impl ServerHandle {
    pub(super) fn new(server: Arc<Server>, conn: ConnId, device: DeviceId) -> Self {
        Self {
            server,
            conn,
            device,
        }
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }
}

#[async_trait]
impl RequestSink for ServerHandle {
    async fn dispatch(&self, req: ClientRequest) -> Result<()> {
        self.server.handle(self.conn, req).await
    }

    async fn detach(&self) {
        self.server.disconnect(self.conn).await;
    }

    async fn upload(&self, room: RoomId, attachment: Attachment, data: Vec<u8>) -> Result<()> {
        if room != self.server.room() {
            return Err(chat_core::ChatError::Protocol("房间不存在".into()));
        }
        self.server.upload(self.conn, attachment, data).await
    }

    async fn download(&self, room: RoomId, id: AttachmentId) -> Result<Vec<u8>> {
        if room != self.server.room() {
            return Err(chat_core::ChatError::Protocol("房间不存在".into()));
        }
        self.server.download(self.conn, id).await
    }
}

/// 把服务端暴露成传输层可用的接入点。
pub struct Acceptor(pub Arc<Server>);

#[async_trait]
impl ConnectionAcceptor for Acceptor {
    fn room_info(&self) -> RoomInfo {
        RoomInfo {
            room: self.0.room(),
            name: self.0.name().to_string(),
        }
    }

    async fn accept(&self, device: DeviceId) -> (Arc<dyn RequestSink>, EventStream) {
        let (handle, rx) = self.0.attach(device).await;
        (Arc::new(handle), rx)
    }
}

pub struct InProcConnection {
    handle: ServerHandle,
    rx: Mutex<mpsc::Receiver<ServerEvent>>,
    closed: AtomicBool,
    closed_notify: Notify,
}

impl InProcConnection {
    pub(super) fn new(handle: ServerHandle, rx: mpsc::Receiver<ServerEvent>) -> Self {
        Self {
            handle,
            rx: Mutex::new(rx),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
        }
    }
}

#[async_trait]
impl Connection for InProcConnection {
    async fn request(&self, req: ClientRequest) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        self.handle.dispatch(req).await
    }

    async fn next_event(&self) -> Result<ServerEvent> {
        let notified = self.closed_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.closed.load(Ordering::Acquire) {
            return Err(ChatError::Closed);
        }
        let mut rx = self.rx.lock().await;
        tokio::select! {
            ev = rx.recv() => ev.ok_or(ChatError::Closed),
            _ = &mut notified => Err(ChatError::Closed),
        }
    }

    async fn close(&self) -> Result<()> {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.closed_notify.notify_waiters();
            self.handle.detach().await;
        }
        Ok(())
    }

    async fn upload(&self, room: RoomId, attachment: Attachment, data: Vec<u8>) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        self.handle.upload(room, attachment, data).await
    }

    async fn download(&self, room: RoomId, id: AttachmentId) -> Result<Vec<u8>> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        self.handle.download(room, id).await
    }
}

impl Drop for InProcConnection {
    fn drop(&mut self) {
        let handle = self.handle.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { handle.detach().await });
        }
    }
}
