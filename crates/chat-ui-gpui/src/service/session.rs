use super::protocol::{await_lobby, map_event};
use super::*;

/// 界面要求当前会话做的事。
pub(super) enum SessionCmd {
    Join(String),
    Leave,
    Send(String),
    /// 结束会话：先退出房间再关闭连接。
    Stop,
}

/// 当前会话。连接断开后由会话任务自行重连，界面无需感知。
pub(super) struct Active {
    pub(super) room: RoomId,
    pub(super) tx: Sender<SessionCmd>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl Active {
    /// 结束会话。切换房间时先退出旧房间，让对端的在线人数立刻回落。
    pub(super) async fn end(self) {
        let _ = self.tx.send(SessionCmd::Stop);
        let _ = self.task.await;
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
    fingerprint: Option<String>,
    room: RoomId,
    liveness: Liveness,
}

impl Target {
    /// 建立一条连接。首次连接与重连走的是同一条路径。
    async fn connect(&self) -> Result<Arc<dyn Connection>, ChatError> {
        let conn = connect_with(self.addr, self.fingerprint.as_deref(), self.liveness).await?;
        Ok(Arc::new(conn))
    }
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
) -> Result<Active, String> {
    let addr: SocketAddr = addr_text
        .parse()
        .map_err(|e| format!("地址「{addr_text}」无效: {e}"))?;
    let fingerprint = fingerprint.map(str::to_string);

    // 先连一次：既把「连不上」尽早报给界面，也拿到房间标识。
    // 这条连接直接交给会话任务，不必重复握手。
    let first = connect_with(addr, fingerprint.as_deref(), liveness)
        .await
        .map_err(|e| format!("连接失败: {e}"))?;
    let target = Target {
        addr,
        fingerprint,
        room: first.room_info().room,
        liveness,
    };
    let first: Arc<dyn Connection> = Arc::new(first);

    // 界面要显示「XX 房间」，这条查询只在首次连接时做
    let client = Client::new(Arc::clone(&first));
    client
        .lobby(target.room)
        .await
        .map_err(|e| format!("查询大厅失败: {e}"))?;
    if let Some(lobby) = await_lobby(&client).await {
        let _ = events.send(UiEvent::Lobby {
            room: target.room,
            name: lobby.name,
            addr: addr_text.to_string(),
            online: lobby.online_count,
        });
    }

    let room = target.room;
    let (tx, rx) = flume::unbounded();
    let task = tokio::spawn(Session::new(target, first, events.clone(), rx).run());

    Ok(Active { room, tx, task })
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
        }
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

            // 连接断了：进入重连，不再把用户踢出房间
            if self.give_up_at.is_none() {
                self.give_up_at = Some(tokio::time::Instant::now() + RECONNECT_WINDOW);
                let _ = self.events.send(UiEvent::Reconnecting("连接中断".into()));
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
                let _ = self.events.send(UiEvent::Disconnected);
                return None;
            }

            match self.target.connect().await {
                Ok(conn) => return Some(conn),
                Err(_) => {
                    self.attempt += 1;
                    let _ = self.events.send(UiEvent::Reconnecting(format!(
                        "仍无法连接（已重试 {} 次）",
                        self.attempt
                    )));
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
                        // 界面已经没了，会话没必要继续
                        if self.events.send(map_event(event)).is_err() {
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
                            let _ = self.events.send(UiEvent::Error(format!("加入失败: {e}")));
                        }
                    }
                    Ok(SessionCmd::Leave) => {
                        self.identity = None;
                        if let Err(e) = client.leave(self.target.room).await {
                            let _ = self.events.send(UiEvent::Error(format!("退出失败: {e}")));
                        }
                    }
                    Ok(SessionCmd::Send(text)) => {
                        if let Err(e) = client.send_text(self.target.room, text).await {
                            let _ = self.events.send(UiEvent::Error(format!("发送失败: {e}")));
                        }
                    }
                },
            }
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
    /// 断线期间界面仍可能操作，这些指令不能丢：`Stop` 立刻结束；`Join` / `Leave` 要记下来，
    /// 否则用户在断线期间改的昵称或退出意图会被吞掉；发消息则明确回一个错误，
    /// 因为此刻根本无从投递。
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
                    let _ = self.events.send(UiEvent::Left {
                        room: self.target.room,
                    });
                }
                Ok(Ok(SessionCmd::Send(_))) => {
                    let _ = self
                        .events
                        .send(UiEvent::Error("发送失败: 尚未重新连接".into()));
                }
                Err(_) => return true,
            }
        }
    }
}
