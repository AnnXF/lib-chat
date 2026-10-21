use super::*;
use chat_core::MessageStore;
use chat_server::{Accounts, Hub, HubAcceptor, SqliteStore};

/// 房间的数据文件：局域网按广播端口分区，公网用固定分区名 `public`。
fn room_db_path(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join(format!("room-{key}.db"))
}

/// 每个房间独立的附件目录；清空房间时整目录删除，不影响同机其它房间。
fn room_files_dir(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join(format!("room-{key}.files"))
}

fn room_identity_path(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join(format!("room-{key}.identity"))
}

/// 看一眼本机是否已经有这个端口的房间；有就返回它的房间标识与消息条数。
///
/// 只做探测，绝不新建文件——没房间时返回 `None`，不会凭空留下一个空库。
///
/// 同机的实例共用同一个库，于是也共用同一个房间标识：拿它去房间发现里问一句
/// 「这个房间是不是已经有人在托管了」，就能避免两个实例各起一个服务器。
pub(super) async fn local_room(data_dir: &Path, beacon_port: u16) -> Option<(RoomId, u64)> {
    let hub_dir = data_dir.join(format!("hub-{beacon_port}"));
    let accounts_path = hub_dir.join("accounts.db");
    if !accounts_path.exists() {
        return None;
    }
    let accounts = Accounts::open(&accounts_path).await.ok()?;
    let room = accounts.find_default_room().await.ok()??;
    let messages_path = hub_dir.join("messages.db");
    let messages = if messages_path.exists() {
        let store = SqliteStore::open(&messages_path).await.ok()?;
        let latest = store.latest_seq(room).await.unwrap_or(0).max(0) as u64;
        store.close().await;
        latest
    } else {
        0
    };
    Some((room, messages))
}

/// 房间的独占锁：谁拿着它，谁才是本机这个房间的房主。
///
/// 光看广播判断「本机有没有人在托管」有个窗口：第二个实例刚起来、还没发现房间，
/// 那会儿它会再起一个服务器，同一个房间标识下又变成两个服务器。锁是跟着打开实例走的
/// （同一个进程里两次打开也互斥），所以同机多实例一样管用。
pub(super) struct RoomHostLock {
    _file: std::fs::File,
    db_path: PathBuf,
    identity_path: PathBuf,
    files_dir: PathBuf,
    /// Hub 的账号与消息库目录。
    hub_dir: PathBuf,
}

/// 抢占一个房间分区的房主位置；已经被别的实例占着就返回 `Err`。
pub(super) fn claim_room(data_dir: &Path, key: &str) -> Result<RoomHostLock, String> {
    let path = data_dir.join(format!("room-{key}.lock"));
    // 抢锁发生在开库之前，目录可能还不存在，这里先把它备好
    std::fs::create_dir_all(data_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // 只是拿它加锁，不动里面的内容
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("打开房间的锁失败: {e}"))?;
    match file.try_lock() {
        Ok(()) => Ok(RoomHostLock {
            _file: file,
            db_path: room_db_path(data_dir, key),
            identity_path: room_identity_path(data_dir, key),
            files_dir: room_files_dir(data_dir, key),
            hub_dir: data_dir.join(format!("hub-{key}")),
        }),
        Err(std::fs::TryLockError::WouldBlock) => Err("本机已有实例开着这个房间".into()),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("锁定房间失败: {e}")),
    }
}

pub(super) fn claim_local_room(data_dir: &Path, beacon_port: u16) -> Result<RoomHostLock, String> {
    claim_room(data_dir, &beacon_port.to_string())
}

impl RoomHostLock {
    /// 清空持锁房间的数据库，之后重建会拿到新的标识与空白历史。
    pub(super) fn reset(&self) -> Result<(), String> {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = self.db_path.clone().into_os_string();
            name.push(suffix);
            let file = PathBuf::from(name);
            if file.exists() {
                std::fs::remove_file(&file).map_err(|e| format!("清空房间失败: {e}"))?;
            }
        }
        if self.identity_path.exists() {
            std::fs::remove_file(&self.identity_path)
                .map_err(|e| format!("清空房间身份失败: {e}"))?;
        }
        if self.files_dir.exists() {
            std::fs::remove_dir_all(&self.files_dir)
                .map_err(|e| format!("清空房间附件失败: {e}"))?;
        }
        if self.hub_dir.exists() {
            std::fs::remove_dir_all(&self.hub_dir)
                .map_err(|e| format!("清空 Hub 数据失败: {e}"))?;
        }
        Ok(())
    }

    /// Hub 的账号与消息库目录。
    fn hub_dir(&self) -> &Path {
        &self.hub_dir
    }
}

/// 本机托管的房间。切换房间或重新建房时必须回收，否则会留下幽灵房间。
pub(super) struct Hosting {
    pub(super) room: RoomId,
    pub(super) endpoint: Arc<QuicServer>,
    pub(super) _hub: Arc<Hub>,
    /// 局域网广播；公网托管为 `None`。
    pub(super) _announcer: Option<RoomAnnouncer>,
    /// 定时上报是否终止（重建房/关闭时置 false）。
    pub(super) alive: Arc<AtomicBool>,
    /// 界面当前是否在展示这个房间；否则上报会污染别的房间的人数。
    pub(super) visible: Arc<AtomicBool>,
    /// 局域网房主位置；公网托管没有分区锁之外的移交需求。
    pub(super) lock: Option<RoomHostLock>,
}

impl Hosting {
    pub(super) async fn shutdown(self, events: &Sender<UiEvent>) -> Option<RoomHostLock> {
        self.alive.store(false, Ordering::Relaxed);
        self.visible.store(false, Ordering::Relaxed);
        self.endpoint.close();
        self.endpoint.wait_idle().await;
        let _ = events
            .send_async(UiEvent::RoomGone { room: self.room })
            .await;
        self.lock
    }
}

pub(super) struct Started {
    pub(super) hub: Arc<Hub>,
    pub(super) endpoint: Arc<QuicServer>,
    pub(super) announcer: Option<RoomAnnouncer>,
    pub(super) addr: String,
    pub(super) fingerprint: String,
    pub(super) lock: Option<RoomHostLock>,
    /// 公网托管时携带端口与接入方式；局域网为 `None`。
    pub(super) public: Option<PublicHosting>,
}

pub(super) async fn start_host(
    name: &str,
    events: &Sender<UiEvent>,
    beacon_port: u16,
    liveness: Liveness,
    lock: RoomHostLock,
) -> Result<Started, String> {
    let identity = chat_transport_quic::SelfSignedCert::load_or_create(
        &lock.identity_path,
        !lock.identity_path.exists(),
    )
    .map_err(|e| format!("房间身份无效: {e}"))?;
    let hub = Hub::open(lock.hub_dir(), name).await?;
    let room = hub.default_room();
    let endpoint = Arc::new(
        QuicServer::bind_with_cert(SocketAddr::from(([0, 0, 0, 0], 0)), liveness, identity)
            .map_err(|e| e.to_string())?,
    );
    let port = endpoint.local_addr().port();
    let fingerprint = endpoint.fingerprint().to_string();
    let data_dir = lock.hub_dir().parent().ok_or("缺少房间数据目录")?;
    chat_transport_quic::TrustStore::new(data_dir)
        .pin(room, &fingerprint)
        .map_err(|e| format!("保存本机房间信任失败: {e}"))?;

    let announcer = RoomAnnouncer::start_on(beacon_port, room, name, port, &fingerprint)
        .map_err(|e| format!("房间广播失败: {e}"))?;

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(HubAcceptor(Arc::clone(&hub)));
    tokio::spawn(Arc::clone(&endpoint).serve(acceptor));

    let _ = events
        .send_async(UiEvent::Hosting {
            room,
            name: name.to_string(),
            addrs: local_addrs(port),
            fingerprint: fingerprint.clone(),
            public: None,
        })
        .await;

    Ok(Started {
        hub,
        endpoint,
        announcer: Some(announcer),
        addr: format!("127.0.0.1:{port}"),
        fingerprint,
        lock: Some(lock),
        public: None,
    })
}

/// 启动公网托管：PEM 证书或自签，固定端口，不广播。
///
/// 不给证书时用自签并固定指纹；给了证书就交给受信 CA 校验，客户端无需指纹。
pub(super) struct PublicStart<'a> {
    pub(super) name: &'a str,
    pub(super) port: u16,
    pub(super) cert: Option<&'a Path>,
    pub(super) key: Option<&'a Path>,
    pub(super) domain: Option<&'a str>,
    pub(super) data_dir: &'a Path,
}

pub(super) async fn start_public_host(
    config: PublicStart<'_>,
    events: &Sender<UiEvent>,
    liveness: Liveness,
) -> Result<Started, String> {
    let PublicStart {
        name,
        port,
        cert,
        key,
        domain,
        data_dir,
    } = config;
    let lock = claim_room(data_dir, "public")?;
    // 证书身份是否存在决定是不是新房间；Hub 目录会随第一次打开创建
    let fresh_room = !lock.identity_path.exists();

    // 证书参数有问题时不要先建库，否则会留下「有库无身份」的半成品房间
    let (pem, identity) = match (cert, key) {
        (Some(cert), Some(key)) => {
            let cert_pem = std::fs::read(cert)
                .map_err(|e| format!("读取证书「{}」失败: {e}", cert.display()))?;
            let key_pem = std::fs::read(key)
                .map_err(|e| format!("读取私钥「{}」失败: {e}", key.display()))?;
            (Some((cert_pem, key_pem)), None)
        }
        (None, None) => (
            None,
            Some(
                chat_transport_quic::SelfSignedCert::load_or_create(
                    &lock.identity_path,
                    fresh_room,
                )
                .map_err(|e| format!("生成自签身份失败: {e}"))?,
            ),
        ),
        _ => return Err("证书与私钥必须同时给出".into()),
    };
    let trusted_ca = pem.is_some();

    let hub = Hub::open(lock.hub_dir(), name).await?;
    let room = hub.default_room();

    let quic = match (pem, identity) {
        (Some((cert_pem, key_pem)), _) => QuicServer::bind_with_pem(
            SocketAddr::from(([0, 0, 0, 0], port)),
            liveness,
            &cert_pem,
            &key_pem,
        )
        .map_err(|e| format!("监听端口 {port} 失败（证书可能无效）: {e}"))?,
        (None, Some(identity)) => {
            QuicServer::bind_with_cert(SocketAddr::from(([0, 0, 0, 0], port)), liveness, identity)
                .map_err(|e| format!("监听端口 {port} 失败: {e}"))?
        }
        (None, None) => return Err("证书与私钥必须同时给出".into()),
    };

    let endpoint = Arc::new(quic);
    let port = endpoint.local_addr().port();
    let fingerprint = endpoint.fingerprint().to_string();
    if !trusted_ca {
        chat_transport_quic::TrustStore::new(data_dir)
            .pin(room, &fingerprint)
            .map_err(|e| format!("保存公网房间信任失败: {e}"))?;
    }

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(HubAcceptor(Arc::clone(&hub)));
    tokio::spawn(Arc::clone(&endpoint).serve(acceptor));

    let public = PublicHosting {
        port,
        domain: domain.map(str::to_string),
        trusted_ca,
    };
    let _ = events
        .send_async(UiEvent::Hosting {
            room,
            name: name.to_string(),
            addrs: local_addrs(port),
            fingerprint: fingerprint.clone(),
            public: Some(public.clone()),
        })
        .await;

    Ok(Started {
        hub,
        endpoint,
        announcer: None,
        addr: format!("127.0.0.1:{port}"),
        fingerprint,
        lock: Some(lock),
        public: Some(public),
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
