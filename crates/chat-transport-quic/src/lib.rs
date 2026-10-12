//! QUIC 传输适配器：把服务端逻辑暴露到局域网，并给客户端提供远程连接。
//!
//! 自签证书 + TOFU 指纹校验，省去局域网内的 CA 依赖。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chat_core::{ChatError, Connection, ConnectionAcceptor, EventStream, Result};
use chat_proto::{ClientRequest, DeviceId, RoomInfo, ServerEvent};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use quinn::{ClientConfig, Endpoint, ServerConfig, TransportConfig};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

/// ALPN 标识，用于隔离本协议与其他 QUIC 流量。
pub const ALPN: &[u8] = b"libchat/1";

/// 协议版本，握手时校验。
pub const PROTOCOL_VERSION: u32 = 1;

/// 单帧上限，防止畸形长度导致巨额分配。
const MAX_FRAME: usize = 4 * 1024 * 1024;

fn transport_err(e: impl std::fmt::Display) -> ChatError {
    ChatError::Transport(e.to_string())
}

/// 连接活性参数。
///
/// quinn 默认 `max_idle_timeout = 30s` 且**不发送保活包**。而安静的房间不产生任何网络
/// 流量——房主的人数定时器只走进程内通道，不发包——于是所有连接会在 30 秒后几乎同时被
/// 判死，用户看到的现象就是「屋里的所有人突然被弹出房间」。
///
/// 这里主动发 PING 续命，并把判死时间放宽以容忍 WiFi 抖动与短暂休眠。判死时间仍然保留，
/// 用于识别真掉线的对端：对端若已死，PING 收不到回应，仍会在该时间内断开。
#[derive(Clone, Copy, Debug)]
pub struct Liveness {
    /// 完全收不到对端数据多久后判定连接已死。
    pub idle_timeout: Duration,
    /// 保活 PING 间隔，需明显小于 `idle_timeout`，留出丢包重试的余量。
    pub keep_alive_interval: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            keep_alive_interval: Duration::from_secs(15),
        }
    }
}

impl Liveness {
    fn transport(&self) -> Arc<TransportConfig> {
        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(Some(
            self.idle_timeout
                .try_into()
                .expect("空闲超时必须为正且不超过 QUIC 上限"),
        ));
        transport.keep_alive_interval(Some(self.keep_alive_interval));
        Arc::new(transport)
    }
}

/// 证书指纹（SHA-256 十六进制小写）。
fn fingerprint_of(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- 线协议

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientFrame {
    Hello { device: DeviceId, version: u32 },
    Request(ClientRequest),
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ServerFrame {
    Welcome { version: u32, room: RoomInfo },
    Event(ServerEvent),
}

async fn write_frame<T: serde::Serialize>(send: &mut quinn::SendStream, frame: &T) -> Result<()> {
    let payload = serde_json::to_vec(frame).map_err(|e| ChatError::Protocol(e.to_string()))?;
    if payload.len() > MAX_FRAME {
        return Err(ChatError::Protocol("帧过大".into()));
    }
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .map_err(transport_err)?;
    send.write_all(&payload).await.map_err(transport_err)?;
    Ok(())
}

async fn read_frame<T: serde::de::DeserializeOwned>(recv: &mut quinn::RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(transport_err)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(ChatError::Protocol("帧过大".into()));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.map_err(transport_err)?;
    serde_json::from_slice(&buf).map_err(|e| ChatError::Protocol(e.to_string()))
}

// ---------------------------------------------------------------- 证书

/// 运行时生成的自签证书。
pub struct SelfSignedCert {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: String,
}

impl SelfSignedCert {
    pub fn generate() -> Result<Self> {
        let issued = rcgen::generate_simple_self_signed(vec!["libchat.local".to_string()])
            .map_err(|e| ChatError::Transport(format!("生成证书失败: {e}")))?;
        let cert = issued.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            issued.signing_key.serialize_der(),
        ));
        let fingerprint = fingerprint_of(&cert);
        Ok(Self {
            cert,
            key,
            fingerprint,
        })
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

// ---------------------------------------------------------------- 客户端校验

/// 首次信任：只校验指纹（若已知），不做 CA 链校验。
#[derive(Debug)]
struct TofuVerifier {
    expected: Option<String>,
    provider: Arc<quinn::rustls::crypto::CryptoProvider>,
}

impl quinn::rustls::client::danger::ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &quinn::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: quinn::rustls::pki_types::UnixTime,
    ) -> std::result::Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error>
    {
        let actual = fingerprint_of(end_entity);
        match &self.expected {
            Some(expected) if expected != &actual => Err(quinn::rustls::Error::General(format!(
                "证书指纹不匹配: 期望 {expected}, 实际 {actual}"
            ))),
            _ => Ok(quinn::rustls::client::danger::ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &quinn::rustls::DigitallySignedStruct,
    ) -> std::result::Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error>
    {
        quinn::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &quinn::rustls::DigitallySignedStruct,
    ) -> std::result::Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error>
    {
        quinn::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn crypto_provider() -> Arc<quinn::rustls::crypto::CryptoProvider> {
    Arc::new(quinn::rustls::crypto::ring::default_provider())
}

// ---------------------------------------------------------------- 服务端

/// QUIC 服务端：接受连接并把请求转给 [`ConnectionAcceptor`]。
pub struct QuicServer {
    endpoint: Endpoint,
    local_addr: SocketAddr,
    fingerprint: String,
}

impl QuicServer {
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        Self::bind_with(addr, Liveness::default())
    }

    /// 指定活性参数绑定。测试用短超时验证保活行为时会用到。
    pub fn bind_with(addr: SocketAddr, liveness: Liveness) -> Result<Self> {
        let cert = SelfSignedCert::generate()?;

        let mut tls = quinn::rustls::ServerConfig::builder_with_provider(crypto_provider())
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])
            .map_err(transport_err)?
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.clone()], cert.key.clone_key())
            .map_err(transport_err)?;
        tls.alpn_protocols = vec![ALPN.to_vec()];

        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(transport_err)?;
        let mut config = ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(liveness.transport());
        let endpoint = Endpoint::server(config, addr).map_err(transport_err)?;
        let local_addr = endpoint.local_addr().map_err(transport_err)?;

        Ok(Self {
            endpoint,
            local_addr,
            fingerprint: cert.fingerprint().to_string(),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// 证书指纹，随 mDNS 一起广播给客户端。
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// 接受连接直到 endpoint 关闭。
    pub async fn serve(self: Arc<Self>, acceptor: Arc<dyn ConnectionAcceptor>) {
        while let Some(incoming) = self.endpoint.accept().await {
            let acceptor = Arc::clone(&acceptor);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(incoming, acceptor).await {
                    eprintln!("连接处理结束: {e}");
                }
            });
        }
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"bye");
    }
}

async fn handle_connection(
    incoming: quinn::Incoming,
    acceptor: Arc<dyn ConnectionAcceptor>,
) -> Result<()> {
    let conn = incoming.await.map_err(transport_err)?;
    let (mut send, mut recv) = conn.accept_bi().await.map_err(transport_err)?;

    // 首帧必须是 hello，据此确定设备身份
    let device = match read_frame::<ClientFrame>(&mut recv).await? {
        ClientFrame::Hello { device, version } if version == PROTOCOL_VERSION => device,
        ClientFrame::Hello { version, .. } => {
            return Err(ChatError::Protocol(format!("协议版本不兼容: {version}")));
        }
        ClientFrame::Request(_) => {
            return Err(ChatError::Protocol("首帧必须是 hello".into()));
        }
    };

    let room = acceptor.room_info();
    let (sink, mut events) = acceptor.accept(device).await;
    write_frame(
        &mut send,
        &ServerFrame::Welcome {
            version: PROTOCOL_VERSION,
            room,
        },
    )
    .await?;

    // 事件出口 → 连接
    let writer = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if write_frame(&mut send, &ServerFrame::Event(ev)).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });

    // 连接 → 请求入口
    loop {
        match read_frame::<ClientFrame>(&mut recv).await {
            Ok(ClientFrame::Request(req)) => {
                if sink.dispatch(req).await.is_err() {
                    break;
                }
            }
            Ok(ClientFrame::Hello { .. }) => {}
            Err(_) => break,
        }
    }

    sink.detach().await;
    writer.abort();
    Ok(())
}

// ---------------------------------------------------------------- 客户端

/// 建立到服务端的 QUIC 连接。
///
/// `expected_fingerprint` 为 `None` 时接受任意证书（首次信任），
/// 为 `Some` 时严格比对，防止中间人。
pub async fn connect(
    addr: SocketAddr,
    expected_fingerprint: Option<&str>,
) -> Result<QuicConnection> {
    connect_with(addr, expected_fingerprint, Liveness::default()).await
}

/// 指定活性参数连接。测试用短超时验证保活行为时会用到。
pub async fn connect_with(
    addr: SocketAddr,
    expected_fingerprint: Option<&str>,
    liveness: Liveness,
) -> Result<QuicConnection> {
    let provider = crypto_provider();
    let mut tls = quinn::rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&quinn::rustls::version::TLS13])
        .map_err(transport_err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TofuVerifier {
            expected: expected_fingerprint.map(str::to_string),
            provider,
        }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(transport_err)?;
    let mut client_config = ClientConfig::new(Arc::new(crypto));
    client_config.transport_config(liveness.transport());

    let mut endpoint =
        Endpoint::client("0.0.0.0:0".parse().expect("合法地址")).map_err(transport_err)?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint
        .connect(addr, "libchat.local")
        .map_err(transport_err)?
        .await
        .map_err(transport_err)?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(transport_err)?;

    let device = DeviceId::new();
    write_frame(
        &mut send,
        &ClientFrame::Hello {
            device,
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    let room = match read_frame::<ServerFrame>(&mut recv).await? {
        ServerFrame::Welcome { version, room } if version == PROTOCOL_VERSION => room,
        _ => return Err(ChatError::Protocol("握手失败".into())),
    };

    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match read_frame::<ServerFrame>(&mut recv).await {
                Ok(ServerFrame::Event(ev)) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Ok(ServerFrame::Welcome { .. }) => {}
                Err(_) => break,
            }
        }
    });

    Ok(QuicConnection {
        room,
        write: Mutex::new(send),
        events: Mutex::new(events),
        endpoint,
        closed: AtomicBool::new(false),
    })
}

/// QUIC 客户端连接。
pub struct QuicConnection {
    room: RoomInfo,
    write: Mutex<quinn::SendStream>,
    events: Mutex<EventStream>,
    endpoint: Endpoint,
    closed: AtomicBool,
}

impl QuicConnection {
    /// 握手时获知的房间信息。
    pub fn room_info(&self) -> &RoomInfo {
        &self.room
    }
}

#[async_trait::async_trait]
impl Connection for QuicConnection {
    async fn request(&self, req: ClientRequest) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(ChatError::Closed);
        }
        let mut send = self.write.lock().await;
        write_frame(&mut send, &ClientFrame::Request(req)).await
    }

    async fn next_event(&self) -> Result<ServerEvent> {
        let mut events = self.events.lock().await;
        events.recv().await.ok_or(ChatError::Closed)
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        self.endpoint.close(0u32.into(), b"bye");
        Ok(())
    }
}
