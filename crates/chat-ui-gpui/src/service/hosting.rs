use super::*;
use chat_core::MessageStore;
use chat_server::SqliteStore;

/// 本机房间的库文件：按广播端口分开，一个端口一个房间。
fn room_db_path(data_dir: &Path, beacon_port: u16) -> PathBuf {
    data_dir.join(format!("room-{beacon_port}.db"))
}

/// 看一眼本机是否已经有这个端口的房间；有就返回它的房间标识与消息条数。
///
/// 只做探测，绝不新建文件——没房间时返回 `None`，不会凭空留下一个空库。
///
/// 同机的实例共用同一个库，于是也共用同一个房间标识：拿它去房间发现里问一句
/// 「这个房间是不是已经有人在托管了」，就能避免两个实例各起一个服务器。
pub(super) async fn local_room(data_dir: &Path, beacon_port: u16) -> Option<(RoomId, u64)> {
    let path = room_db_path(data_dir, beacon_port);
    if !path.exists() {
        return None;
    }
    let store = SqliteStore::open(&path).await.ok()?;
    let room = store.room();
    let latest = store.latest_seq(room).await;
    store.close().await;
    Some((room, latest.ok()?.max(0) as u64))
}

/// 本机房间的独占锁：谁拿着它，谁才是本机这个房间的房主。
///
/// 光看广播判断「本机有没有人在托管」有个窗口：第二个实例刚起来、还没发现房间，
/// 那会儿它会再起一个服务器，同一个房间标识下又变成两个服务器。锁是跟着打开实例走的
/// （同一个进程里两次打开也互斥），所以同机多实例一样管用。
pub(super) struct RoomHostLock {
    _file: std::fs::File,
    db_path: PathBuf,
    identity_path: PathBuf,
}

/// 抢占本机房间的房主位置；已经被别的实例占着就返回 `Err`。
pub(super) fn claim_local_room(data_dir: &Path, beacon_port: u16) -> Result<RoomHostLock, String> {
    let path = data_dir.join(format!("room-{beacon_port}.lock"));
    // 抢锁发生在开库之前，目录可能还不存在，这里先把它备好
    std::fs::create_dir_all(data_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // 只是拿它加锁，不动里面的内容
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("打开本机房间的锁失败: {e}"))?;
    match file.try_lock() {
        Ok(()) => Ok(RoomHostLock {
            _file: file,
            db_path: room_db_path(data_dir, beacon_port),
            identity_path: data_dir.join(format!("room-{beacon_port}.identity")),
        }),
        Err(std::fs::TryLockError::WouldBlock) => Err("本机已有实例开着这个房间".into()),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("锁定本机房间失败: {e}")),
    }
}

impl RoomHostLock {
    /// 清空持锁房间的数据库，之后重建会拿到新的标识与空白历史。
    pub(super) fn reset(&self) -> Result<(), String> {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = self.db_path.clone().into_os_string();
            name.push(suffix);
            let file = PathBuf::from(name);
            if file.exists() {
                std::fs::remove_file(&file).map_err(|e| format!("清空本机房间失败: {e}"))?;
            }
        }
        if self.identity_path.exists() {
            std::fs::remove_file(&self.identity_path)
                .map_err(|e| format!("清空房间身份失败: {e}"))?;
        }
        Ok(())
    }
}

/// 本机托管的房间。切换房间或重新建房时必须回收，否则会留下幽灵房间。
pub(super) struct Hosting {
    pub(super) room: RoomId,
    pub(super) endpoint: Arc<QuicServer>,
    pub(super) store: Arc<SqliteStore>,
    /// 仅用于保活：持有它才能持续广播房间。
    pub(super) _announcer: RoomAnnouncer,
    /// 定时上报是否终止（重建房/关闭时置 false）。
    pub(super) alive: Arc<AtomicBool>,
    /// 界面当前是否在展示这个房间；否则上报会污染别的房间的人数。
    pub(super) visible: Arc<AtomicBool>,
    /// 房主位置：重建时移交给新房间，否则随托管释放。
    pub(super) lock: RoomHostLock,
}

impl Hosting {
    pub(super) async fn shutdown(self, events: &Sender<UiEvent>) -> RoomHostLock {
        self.alive.store(false, Ordering::Relaxed);
        self.visible.store(false, Ordering::Relaxed);
        self.endpoint.close();
        self.endpoint.wait_idle().await;
        self.store.close().await;
        let _ = events
            .send_async(UiEvent::RoomGone { room: self.room })
            .await;
        self.lock
    }
}

pub(super) struct Started {
    pub(super) server: Arc<Server>,
    pub(super) store: Arc<SqliteStore>,
    pub(super) endpoint: Arc<QuicServer>,
    pub(super) announcer: RoomAnnouncer,
    pub(super) addr: String,
    pub(super) fingerprint: String,
    pub(super) lock: RoomHostLock,
}

pub(super) async fn start_host(
    name: &str,
    events: &Sender<UiEvent>,
    beacon_port: u16,
    liveness: Liveness,
    lock: RoomHostLock,
) -> Result<Started, String> {
    let path = lock.db_path.clone();
    let identity =
        chat_transport_quic::SelfSignedCert::load_or_create(&lock.identity_path, !path.exists())
            .map_err(|e| format!("房间身份无效: {e}"))?;
    let store = Arc::new(
        SqliteStore::open(&path)
            .await
            .map_err(|e| format!("打开消息库失败: {e}"))?,
    );
    let server = Server::with_room(
        store.room(),
        name,
        Arc::clone(&store) as Arc<dyn MessageStore>,
    );
    let endpoint = Arc::new(
        QuicServer::bind_with_cert(SocketAddr::from(([0, 0, 0, 0], 0)), liveness, identity)
            .map_err(|e| e.to_string())?,
    );
    let port = endpoint.local_addr().port();
    let fingerprint = endpoint.fingerprint().to_string();
    let data_dir = lock.db_path.parent().ok_or("缺少房间数据目录")?;
    chat_transport_quic::TrustStore::new(data_dir)
        .pin(server.room(), &fingerprint)
        .map_err(|e| format!("保存本机房间信任失败: {e}"))?;

    let announcer = RoomAnnouncer::start_on(beacon_port, server.room(), name, port, &fingerprint)
        .map_err(|e| format!("房间广播失败: {e}"))?;

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&endpoint).serve(acceptor));

    let _ = events
        .send_async(UiEvent::Hosting {
            room: server.room(),
            name: name.to_string(),
            addrs: local_addrs(port),
            fingerprint: fingerprint.clone(),
        })
        .await;

    Ok(Started {
        server,
        store,
        endpoint,
        announcer,
        addr: format!("127.0.0.1:{port}"),
        fingerprint,
        lock,
    })
}

/// 本机可被局域网访问的地址，附带回环地址便于单机自测。
fn local_addrs(port: u16) -> Vec<String> {
    let mut addrs = vec![format!("127.0.0.1:{port}")];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for interface in ifs {
            if !interface.is_loopback() && interface.is_oper_up() && interface.ip().is_ipv4() {
                addrs.push(format!("{}:{port}", interface.ip()));
            }
        }
    }
    addrs
}
