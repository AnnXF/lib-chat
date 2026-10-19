use std::collections::VecDeque;

use uuid::Uuid;

use super::protocol::{await_lobby, map_event};
use super::*;

/// 界面要求当前会话做的事。
pub(super) enum SessionCmd {
    Join(String),
    Leave,
    Send(String),
    /// 往前翻一页更早的历史。
    LoadOlder,
    /// 结束会话：先退出房间再关闭连接。
    Stop,
}

/// 当前会话。连接断开后由会话任务自行重连，界面无需感知。
pub(super) struct Active {
    pub(super) room: RoomId,
    pub(super) tx: Sender<SessionCmd>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

/// 等会话结束的上限。正常退出只要几十毫秒，拖这么久说明它卡在某次握手里了。
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

impl Active {
    /// 结束会话。切换房间时先退出旧房间，让对端的在线人数立刻回落。
    pub(super) async fn end(self) {
        let _ = self.tx.try_send(SessionCmd::Stop);
        let mut task = self.task;
        if tokio::time::timeout(STOP_TIMEOUT, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
}

/// 重连退避：先密后疏，最长 5 秒。
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(3)).min(5))
}

/// 会话目标：连到哪里、认哪个房间、用什么保活参数。
///
/// 打包在一起，是因为重连时要用**完全相同**的一组参数再连一次。
struct Target {
    addr: SocketAddr,
    fingerprint: String,
    room: RoomId,
    liveness: Liveness,
    rooms: Option<DiscoveredRooms>,
}

impl Target {
    /// 建立一条连接。首次连接与重连走的是同一条路径。
    async fn connect(&mut self) -> Result<Arc<dyn Connection>, ChatError> {
        if let Some(rooms) = &self.rooms
            && let Some((addr, fingerprint)) = rooms.host_of(self.room).await
            && self.fingerprint == fingerprint
            && let Ok(addr) = addr.parse()
        {
            self.addr = addr;
        }
        let conn = connect_once(self.addr, &self.fingerprint, self.liveness).await?;
        if conn.room_info().room != self.room {
            return Err(ChatError::Protocol("重连目标房间标识已改变".into()));
        }
        Ok(Arc::new(conn))
    }
}

/// 建连超时。房间重建会换端口，房间列表里残留的旧地址就再也没人应了。
///
/// 没有超时的话，一次握手会一直挂到 QUIC 空闲超时（默认 60 秒）；而后端是串行处理指令的，
/// 这一挂就是「点了没反应」——换房间、建房、加入全排不上队。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PENDING_SEND_CAPACITY: usize = 128;
const MAX_TEXT_BYTES: usize = 8 * 1024;

/// 建一条连接，带超时。首次连接与重连共用同一份超时策略。
async fn connect_once(
    addr: SocketAddr,
    fingerprint: &str,
    liveness: Liveness,
) -> Result<QuicConnection, ChatError> {
    tokio::time::timeout(CONNECT_TIMEOUT, connect_with(addr, fingerprint, liveness))
        .await
        .map_err(|_| ChatError::Transport(format!("连接超时（{CONNECT_TIMEOUT:?}）")))?
}

/// 一条连接上的服务是怎么结束的。
enum ServeEnd {
    /// 连接断了，值得重连。
    Dropped,
    /// 界面要求结束（或界面已销毁），会话到此为止。
    Done,
}

pub(super) async fn open_session(
    addr_text: &str,
    fingerprint: Option<&str>,
    events: &Sender<UiEvent>,
    liveness: Liveness,
    expected_room: Option<RoomId>,
    trust: Option<&TrustStore>,
    rooms: Option<DiscoveredRooms>,
) -> Result<(Active, UiEvent), String> {
    let fingerprint = fingerprint.ok_or("连接房间须提供完整证书指纹")?;
    validate_fingerprint(fingerprint).map_err(|e| e.to_string())?;
    let addr: SocketAddr = addr_text
        .parse()
        .map_err(|e| format!("地址「{addr_text}」无效: {e}"))?;
    let fingerprint = fingerprint.to_ascii_lowercase();

    let first = connect_once(addr, &fingerprint, liveness)
        .await
        .map_err(|e| format!("连接失败: {e}"))?;
    let room = first.room_info().room;
    if expected_room.is_some_and(|expected| expected != room) {
        return Err("连接房间标识与选中的房间不一致".into());
    }
    if let Some(trust) = trust
        && let Some(existing) = trust.get(room).map_err(|e| e.to_string())?
        && existing != fingerprint
    {
        return Err(format!("房间 {room} 的证书指纹已变更，已拒绝连接"));
    }
    let target = Target {
        addr,
        fingerprint: fingerprint.clone(),
        room,
        liveness,
        rooms,
    };
    let first: Arc<dyn Connection> = Arc::new(first);

    // 界面要显示「XX 房间」，这条查询只在首次连接时做
    let client = Client::new(Arc::clone(&first));
    client
        .lobby(target.room)
        .await
        .map_err(|e| format!("查询大厅失败: {e}"))?;
    let lobby = await_lobby(&client).await.ok_or("查询大厅超时")?;
    if lobby.room != target.room {
        return Err("房间标识与大厅信息不一致".into());
    }
    if let Some(trust) = trust {
        trust
            .pin(target.room, &fingerprint)
            .map_err(|e| e.to_string())?;
    }
    let lobby_event = UiEvent::Lobby {
        room: target.room,
        name: lobby.name,
        addr: addr_text.to_string(),
        online: lobby.online_count,
    };

    let room = target.room;
    let (tx, rx) = flume::bounded(128);
    let task = tokio::spawn(Session::new(target, first, events.clone(), rx).run());

    Ok((Active { room, tx, task }, lobby_event))
}

/// 会话任务：维持一条到房间的连接。
///
/// 连接意外断开（WiFi 抖动、笔记本休眠、房主重启）时自动重连并恢复身份——用户的心智是
/// 「只有点了退出才离开房间」，网络抖动不该把人踢出去。只有超过 [`RECONNECT_WINDOW`] 仍
/// 连不上，或收到 [`SessionCmd::Stop`]（用户切房间/退出），会话才真正结束。
struct Session {
    /// 连到哪里、认哪个房间、用什么保活参数。
    target: Target,
    events: Sender<UiEvent>,
    cmds: Receiver<SessionCmd>,
    /// 已加入时的昵称，重连后据此恢复身份。
    identity: Option<String>,
    /// 下一条待用连接：首连由调用方给出，之后自己重连。
    next: Option<Arc<dyn Connection>>,
    /// 连续失败几次，用来算退避。
    attempt: u32,
    /// 本轮断线到什么时候放弃重连；连上后清空。
    give_up_at: Option<tokio::time::Instant>,
    /// 已连续补齐的最大序号，只能由按序推送或同步页推进。
    cursor: Seq,
    /// 实时推送或加入应答已知的最新序号，不得当作同步起点。
    newest: Seq,
    syncing: bool,
    /// 首次加入要拉一次最近历史；之后的重连只补缺口。
    needs_history: bool,
    /// 已拿到的最早一条 seq，作为往更早翻的游标。
    oldest: Option<Seq>,
    /// 服务端说更早的消息还有。
    has_older: bool,
    /// 有一页还没回来，避免滚动回调重复触发。
    loading_older: bool,
    /// 保留原消息标识，连接断开后再发送也不会重复落库。
    pending: VecDeque<(Uuid, String)>,
}

impl Session {
    fn new(
        target: Target,
        first: Arc<dyn Connection>,
        events: Sender<UiEvent>,
        cmds: Receiver<SessionCmd>,
    ) -> Self {
        Self {
            target,
            events,
            cmds,
            identity: None,
            next: Some(first),
            attempt: 0,
            give_up_at: None,
            cursor: 0,
            newest: 0,
            syncing: false,
            needs_history: true,
            oldest: None,
            has_older: true,
            loading_older: false,
            pending: VecDeque::new(),
        }
    }

    fn enqueue(&mut self, text: String) -> Result<(Uuid, String), &'static str> {
        if self.identity.is_none() {
            return Err("尚未加入房间");
        }
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_TEXT_BYTES {
            return Err("消息为空或超过长度限制");
        }
        if self.pending.len() >= PENDING_SEND_CAPACITY {
            return Err("待确认消息已满，请稍后再试");
        }
        let message = (Uuid::new_v4(), trimmed.to_string());
        self.pending.push_back(message.clone());
        Ok(message)
    }

    /// 主循环：拿连接 → 握手 → 服务 → 断了就重连，直到该结束。
    async fn run(mut self) {
        loop {
            let Some(conn) = self.acquire().await else {
                return;
            };
            let client = Client::new(conn);

            if !self.handshake(&client).await {
                continue;
            }
            if matches!(self.serve(&client).await, ServeEnd::Done) {
                return;
            }
            self.syncing = false;
            self.loading_older = false;
            let _ = self
                .events
                .send_async(UiEvent::HistoryCancelled {
                    room: self.target.room,
                })
                .await;

            // 连接断了：进入重连，不再把用户踢出房间
            if self.give_up_at.is_none() {
                self.give_up_at = Some(tokio::time::Instant::now() + RECONNECT_WINDOW);
                let _ = self
                    .events
                    .send_async(UiEvent::Reconnecting("连接中断".into()))
                    .await;
            }
        }
    }

    /// 取一条可用连接。返回 `None` 表示会话应当结束。
    ///
    /// 首连由调用方给出；之后就在这里退避重试，直到连上或超过 [`RECONNECT_WINDOW`]。
    async fn acquire(&mut self) -> Option<Arc<dyn Connection>> {
        if let Some(conn) = self.next.take() {
            return Some(conn);
        }

        let deadline = *self
            .give_up_at
            .get_or_insert_with(|| tokio::time::Instant::now() + RECONNECT_WINDOW);

        loop {
            // 退避等待；期间界面仍可能操作，要照常响应
            let until = (tokio::time::Instant::now() + backoff(self.attempt)).min(deadline);
            if !self.wait_before_retry(until).await {
                return None;
            }
            // 等待期间可能已经越过放弃时刻
            if tokio::time::Instant::now() >= deadline {
                let _ = self.events.send_async(UiEvent::Disconnected).await;
                return None;
            }

            match self.target.connect().await {
                Ok(conn) => return Some(conn),
                Err(_) => {
                    self.attempt += 1;
                    let _ = self
                        .events
                        .send_async(UiEvent::Reconnecting(format!(
                            "仍无法连接（已重试 {} 次）",
                            self.attempt
                        )))
                        .await;
                }
            }
        }
    }

    /// 确认这条连接真能用，并恢复之前已加入的身份。
    ///
    /// 返回 `false` 表示这条连接用不了，应当回到主循环重来。
    async fn handshake(&mut self, client: &Client) -> bool {
        // 每次（重）连都问一次大厅：既确认连接可用，界面也靠它拿房间名
        if client.lobby(self.target.room).await.is_err() {
            self.attempt += 1;
            return false;
        }
        // 之前已加入过就恢复身份，用户不会察觉自己掉过线
        if let Some(name) = self.identity.clone()
            && client.join(self.target.room, name).await.is_err()
        {
            self.attempt += 1;
            return false;
        }

        self.attempt = 0;
        self.give_up_at = None;
        true
    }

    /// 一条连接上的事件循环：转发服务端事件、执行界面指令。
    async fn serve(&mut self, client: &Client) -> ServeEnd {
        loop {
            tokio::select! {
                event = client.next_event() => match event {
                    Ok(event) => {
                        self.observe(client, &event).await;
                        // 界面已经没了，会话没必要继续
                        if self.events.send_async(map_event(event)).await.is_err() {
                            let _ = client.close().await;
                            return ServeEnd::Done;
                        }
                    }
                    // 连接断了，交给主循环去重连
                    Err(_) => return ServeEnd::Dropped,
                },
                cmd = self.cmds.recv_async() => match cmd {
                    // 界面已销毁，或明确要求结束
                    Err(_) | Ok(SessionCmd::Stop) => {
                        self.leave_room(client).await;
                        return ServeEnd::Done;
                    }
                    Ok(SessionCmd::Join(name)) => {
                        self.identity = Some(name.clone());
                        if let Err(e) = client.join(self.target.room, name).await {
                            let _ = self.events.send_async(UiEvent::Error(format!("加入失败: {e}"))).await;
                        }
                    }
                    Ok(SessionCmd::Leave) => {
                        self.identity = None;
                        self.pending.clear();
                        if let Err(e) = client.leave(self.target.room).await {
                            let _ = self.events.send_async(UiEvent::Error(format!("退出失败: {e}"))).await;
                        }
                    }
                    Ok(SessionCmd::Send(text)) => match self.enqueue(text) {
                        Ok((id, text)) => {
                            if client.send_text_with_id(self.target.room, id, text).await.is_err() {
                                return ServeEnd::Dropped;
                            }
                        }
                        Err(reason) => {
                            let _ = self.events.send_async(UiEvent::Error(format!("发送失败: {reason}"))).await;
                        }
                    },
                    Ok(SessionCmd::LoadOlder) => self.load_older(client).await,
                },
            }
        }
    }

    async fn observe(&mut self, client: &Client, event: &ServerEvent) {
        match event {
            ServerEvent::Message { message } => {
                self.pending.retain(|(id, _)| *id != message.client_msg_id);
                self.newest = self.newest.max(message.seq);
                if message.seq == self.cursor + 1 {
                    self.cursor = message.seq;
                }
                self.request_sync(client, false).await;
            }
            ServerEvent::Joined { latest_seq, .. } => {
                self.newest = self.newest.max(*latest_seq);
                if self.needs_history {
                    self.needs_history = false;
                    self.cursor = self.cursor.max(*latest_seq);
                    if client
                        .history(self.target.room, None, HISTORY_LIMIT)
                        .await
                        .is_err()
                    {
                        let _ = self
                            .events
                            .send_async(UiEvent::Error("读取最近历史失败".into()))
                            .await;
                    }
                } else {
                    self.request_sync(client, false).await;
                }
                for (id, text) in &self.pending {
                    if client
                        .send_text_with_id(self.target.room, *id, text)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            ServerEvent::History {
                messages, has_more, ..
            } => {
                self.loading_older = false;
                self.has_older = *has_more;
                if let Some(first) = messages.first() {
                    self.oldest = Some(self.oldest.map_or(first.seq, |old| old.min(first.seq)));
                }
                if let Some(last) = messages.last() {
                    self.newest = self.newest.max(last.seq);
                    self.request_sync(client, false).await;
                }
            }
            ServerEvent::Sync {
                messages, has_more, ..
            } => {
                self.syncing = false;
                if let Some(last) = messages.last() {
                    self.cursor = self.cursor.max(last.seq);
                    self.newest = self.newest.max(last.seq);
                    self.request_sync(client, *has_more).await;
                }
            }
            ServerEvent::Error { .. } => {
                self.syncing = false;
                if self.loading_older {
                    self.loading_older = false;
                    let _ = self
                        .events
                        .send_async(UiEvent::HistoryCancelled {
                            room: self.target.room,
                        })
                        .await;
                }
            }
            _ => {}
        }
    }

    async fn request_sync(&mut self, client: &Client, has_more: bool) {
        if self.syncing || (!has_more && self.newest <= self.cursor) {
            return;
        }
        if client
            .sync(self.target.room, self.cursor, SYNC_LIMIT)
            .await
            .is_ok()
        {
            self.syncing = true;
        } else {
            let _ = self
                .events
                .send_async(UiEvent::Error("同步消息失败".into()))
                .await;
        }
    }

    /// 往前翻一页更早的历史。
    ///
    /// 界面滑到顶喊一声就行，能不能翻、从哪翻由这里决定。
    async fn load_older(&mut self, client: &Client) {
        if self.loading_older || !self.has_older {
            return;
        }
        // 初始那一页还没回来，等它把游标定下来
        let Some(oldest) = self.oldest else {
            return;
        };

        self.loading_older = true;
        if client
            .history(self.target.room, Some(oldest), OLDER_PAGE)
            .await
            .is_err()
        {
            self.loading_older = false;
            let _ = self
                .events
                .send_async(UiEvent::HistoryCancelled {
                    room: self.target.room,
                })
                .await;
        }
    }

    /// 明确地离开：先退出房间让对端的在线人数立刻回落，再关闭连接。
    async fn leave_room(&self, client: &Client) {
        if self.identity.is_some() {
            let _ = client.leave(self.target.room).await;
            // 等服务端收下并处理，再关闭连接，否则关得太快会把请求吞掉
            tokio::time::sleep(LEAVE_GRACE).await;
        }
        let _ = client.close().await;
    }

    /// 重连前的退避等待。返回 `false` 表示界面要求结束会话。
    ///
    /// 断线期间保留身份变更，并将待发消息有界缓存到同房间重连成功之后。
    async fn wait_before_retry(&mut self, until: tokio::time::Instant) -> bool {
        loop {
            let remaining = until.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return true;
            }
            match tokio::time::timeout(remaining, self.cmds.recv_async()).await {
                Ok(Err(_)) | Ok(Ok(SessionCmd::Stop)) => return false,
                Ok(Ok(SessionCmd::Join(name))) => self.identity = Some(name),
                Ok(Ok(SessionCmd::Leave)) => {
                    self.identity = None;
                    self.pending.clear();
                    let _ = self
                        .events
                        .send_async(UiEvent::Left {
                            room: self.target.room,
                        })
                        .await;
                }
                Ok(Ok(SessionCmd::Send(text))) => {
                    if let Err(reason) = self.enqueue(text) {
                        let _ = self
                            .events
                            .send_async(UiEvent::Error(format!("发送失败: {reason}")))
                            .await;
                    }
                }
                // 断线期间翻不了历史，重连后界面再滑到顶会重新发
                Ok(Ok(SessionCmd::LoadOlder)) => {}
                Err(_) => return true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn end_cancels_a_session_that_ignores_stop() {
        struct Cancellation(Arc<AtomicBool>);
        impl Drop for Cancellation {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let (tx, _rx) = flume::unbounded();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let task = tokio::spawn(async move {
            let _guard = Cancellation(flag);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        let active = Active {
            room: RoomId::new(),
            tx,
            task,
        };

        let started = tokio::time::Instant::now();
        let ended = tokio::time::timeout(STOP_TIMEOUT * 2, active.end()).await;
        let elapsed = started.elapsed();

        assert!(ended.is_ok(), "end 必须带超时，实际耗时 {elapsed:?}");
        assert!(elapsed < STOP_TIMEOUT * 2, "end 不该等这么久：{elapsed:?}");
        assert!(cancelled.load(Ordering::SeqCst), "超时后旧会话必须结束");
    }

    #[derive(Default)]
    struct RecordedConnection(std::sync::Mutex<Vec<chat_core::ClientRequest>>);

    #[async_trait::async_trait]
    impl Connection for RecordedConnection {
        async fn request(&self, request: chat_core::ClientRequest) -> chat_core::Result<()> {
            self.0.lock().unwrap().push(request);
            Ok(())
        }

        async fn next_event(&self) -> chat_core::Result<ServerEvent> {
            std::future::pending().await
        }

        async fn close(&self) -> chat_core::Result<()> {
            Ok(())
        }
    }

    fn message(room: RoomId, seq: Seq) -> Message {
        Message {
            room,
            seq,
            client_msg_id: uuid::Uuid::new_v4(),
            sender: UserId::new(),
            sender_name: "Alice".into(),
            kind: chat_core::MessageKind::Text,
            text: format!("m{seq}"),
            server_ts_ms: 0,
        }
    }

    #[tokio::test]
    async fn pending_message_reuses_id_until_server_acknowledges_it() {
        let room = RoomId::new();
        let conn = Arc::new(RecordedConnection::default());
        let client = Client::new(conn.clone());
        let (events, _rx) = flume::unbounded();
        let (_tx, cmds) = flume::unbounded();
        let target = Target {
            room,
            addr: "127.0.0.1:54321".parse().unwrap(),
            fingerprint: "a".repeat(64),
            liveness: Liveness::default(),
            rooms: None,
        };
        let mut session = Session::new(target, conn.clone(), events, cmds);
        session.identity = Some("Alice".into());
        session.needs_history = false;
        let (id, text) = session.enqueue("同一条消息".into()).unwrap();
        client.send_text_with_id(room, id, &text).await.unwrap();
        let joined = ServerEvent::Joined {
            room,
            you: UserId::new(),
            online_count: 1,
            latest_seq: 0,
        };
        session.observe(&client, &joined).await;
        let sent: Vec<_> = conn
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|req| match req {
                chat_core::ClientRequest::Send { client_msg_id, .. } => Some(*client_msg_id),
                _ => None,
            })
            .collect();
        assert_eq!(sent, [id, id]);
        assert_eq!(session.pending.len(), 1);
        let acknowledged = ServerEvent::Message {
            message: Message {
                client_msg_id: id,
                text: text.clone(),
                ..message(room, 1)
            },
        };
        session.observe(&client, &acknowledged).await;
        assert!(session.pending.is_empty());
        session.observe(&client, &joined).await;
        let sent_again = conn
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|req| matches!(req, chat_core::ClientRequest::Send { .. }))
            .count();
        assert_eq!(sent_again, 2);
        for _ in 0..PENDING_SEND_CAPACITY {
            session.enqueue("待发".into()).unwrap();
        }
        assert!(session.enqueue("超过容量".into()).is_err());
        assert_eq!(session.pending.len(), PENDING_SEND_CAPACITY);
    }

    #[tokio::test]
    async fn live_message_does_not_skip_unsynced_pages() {
        let room = RoomId::new();
        let conn = Arc::new(RecordedConnection::default());
        let client = Client::new(conn.clone());
        let (events, _rx) = flume::unbounded();
        let (_tx, cmds) = flume::unbounded();
        let target = Target {
            room,
            addr: "127.0.0.1:54321".parse().unwrap(),
            fingerprint: "a".repeat(64),
            liveness: Liveness::default(),
            rooms: None,
        };
        let mut session = Session::new(target, conn.clone(), events, cmds);
        session.needs_history = false;
        session.cursor = 1;
        session.newest = 1;
        session
            .observe(
                &client,
                &ServerEvent::Joined {
                    room,
                    you: UserId::new(),
                    online_count: 1,
                    latest_seq: 401,
                },
            )
            .await;
        session
            .observe(
                &client,
                &ServerEvent::Message {
                    message: message(room, 402),
                },
            )
            .await;
        assert_eq!(session.cursor, 1);
        session
            .observe(
                &client,
                &ServerEvent::Sync {
                    room,
                    messages: (2..=201).map(|seq| message(room, seq)).collect(),
                    has_more: true,
                },
            )
            .await;
        assert_eq!(session.cursor, 201);
        session
            .observe(
                &client,
                &ServerEvent::Sync {
                    room,
                    messages: (202..=401).map(|seq| message(room, seq)).collect(),
                    has_more: false,
                },
            )
            .await;
        assert_eq!(session.cursor, 401);
        session
            .observe(
                &client,
                &ServerEvent::Sync {
                    room,
                    messages: vec![message(room, 402)],
                    has_more: false,
                },
            )
            .await;
        assert_eq!(session.cursor, 402);

        let after: Vec<Seq> = conn
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|req| match req {
                chat_core::ClientRequest::Sync { after, .. } => Some(*after),
                _ => None,
            })
            .collect();
        assert_eq!(after, [1, 201, 401]);
    }
}
