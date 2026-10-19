//! 局域网聊天工具的命令行形态，用于在没有界面时验证整条链路。
//!
//! ```text
//! libchat host [--port 0] [--name 房间名]
//! libchat join [--addr ip:port] [--fingerprint 指纹] [--name 昵称]
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chat_core::Client;
use chat_core::{Connection, ConnectionAcceptor};
use chat_core::{RoomId, ServerEvent};
use chat_discovery::{RoomAnnouncement, RoomAnnouncer, RoomBrowser, PROTOCOL_VERSION};
use chat_server::{Acceptor, Server};
use chat_transport_quic::{connect, validate_fingerprint, QuicServer, TrustStore};
use tokio::io::{AsyncBufReadExt, BufReader};

type AnyErr = Box<dyn std::error::Error + Send + Sync>;

fn opt(args: &[String], key: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == key)?;
    args.get(pos + 1).cloned()
}

/// 本机可被局域网访问的 IPv4 地址。
fn local_addrs(port: u16) -> Vec<SocketAddr> {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.into_iter()
                .filter(|i| !i.is_loopback() && i.is_oper_up() && i.ip().is_ipv4())
                .map(|i| SocketAddr::new(i.ip(), port))
                .collect()
        })
        .unwrap_or_default()
}

fn usage() {
    eprintln!(
        "用法:\n  \
         libchat host [--port 0] [--name 房间名]\n  \
         libchat join [--addr ip:port --fingerprint SHA-256] [--name 昵称] [--data-dir 目录]"
    );
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("host") => host(&args[1..]).await,
        Some("join") => join(&args[1..]).await,
        _ => {
            usage();
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn host(args: &[String]) -> Result<(), AnyErr> {
    let name = opt(args, "--name").unwrap_or_else(|| "局域网聊天室".into());
    let port: u16 = opt(args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);

    let server = Server::new(name.clone());
    let quic = Arc::new(QuicServer::bind(SocketAddr::from(([0, 0, 0, 0], port)))?);
    let addr = quic.local_addr();
    let fingerprint = quic.fingerprint().to_string();

    let acceptor: Arc<dyn ConnectionAcceptor> = Arc::new(Acceptor(Arc::clone(&server)));
    tokio::spawn(Arc::clone(&quic).serve(acceptor));

    let _advertiser = match RoomAnnouncer::start(server.room(), &name, addr.port(), &fingerprint) {
        Ok(a) => {
            println!("已广播房间（mDNS + UDP），局域网客户端可直接发现");
            Some(a)
        }
        Err(e) => {
            eprintln!("广播失败，客户端请用 --addr 手动连接: {e}");
            None
        }
    };

    println!("房间: {name}");
    println!("监听: {addr}");
    println!("指纹: {fingerprint}");
    let lan = local_addrs(addr.port());
    if lan.is_empty() {
        eprintln!("未找到局域网 IPv4 地址，客户端请依赖 mDNS 发现");
    }
    for a in &lan {
        println!("加入: libchat join --addr {a} --fingerprint {fingerprint}");
    }
    println!("按 Ctrl+C 退出");

    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        println!("[状态] 在线 {}", server.online_count().await);
    }
}

fn discovered_target(
    ann: RoomAnnouncement,
    pinned_fingerprint: Option<String>,
) -> Result<(SocketAddr, String), AnyErr> {
    if ann.version != PROTOCOL_VERSION {
        return Err(format!("协议版本不兼容: {}", ann.version).into());
    }
    let addr = *ann.addrs.first().ok_or("房间未提供可用地址")?;
    let fingerprint = pinned_fingerprint
        .or_else(|| (!ann.fingerprint.is_empty()).then_some(ann.fingerprint))
        .ok_or("发现的房间缺少证书指纹")?;
    Ok((addr, fingerprint))
}

async fn join(args: &[String]) -> Result<(), AnyErr> {
    let name = opt(args, "--name").unwrap_or_else(|| "我".into());
    let data_dir = match opt(args, "--data-dir") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("无法确定家目录，请指定 --data-dir")?)
            .join(".lib-chat")
            .join("rooms"),
    };
    let trust = TrustStore::new(&data_dir);
    let explicitly_pinned = opt(args, "--fingerprint");

    let (addr, fingerprint, expected_room) = match opt(args, "--addr") {
        Some(a) => (
            a.parse()?,
            explicitly_pinned
                .clone()
                .ok_or("手动连接必须提供 --fingerprint")?,
            None,
        ),
        None => {
            println!("正在搜索局域网房间...");
            let mut browser = RoomBrowser::start()?;
            let ann = tokio::time::timeout(Duration::from_secs(15), browser.next_room())
                .await
                .map_err(|_| "搜索房间超时，可用 --addr 手动指定")??;
            let room = ann.room;
            let announced = ann.fingerprint.clone();
            let room_name = ann.name.clone();
            let (addr, fingerprint) = discovered_target(ann, explicitly_pinned.clone())?;
            if announced != fingerprint && explicitly_pinned.is_none() {
                return Err("发现的房间指纹不一致".into());
            }
            println!("发现房间「{room_name}」于 {addr}");
            (addr, fingerprint, Some(room))
        }
    };
    validate_fingerprint(&fingerprint)?;
    let fingerprint = fingerprint.to_ascii_lowercase();
    if let Some(room) = expected_room {
        match trust.get(room)? {
            Some(pinned) if pinned != fingerprint => {
                return Err(format!("房间 {room} 的指纹已变更，拒绝连接").into())
            }
            Some(_) => {}
            None if explicitly_pinned.is_none() => {
                println!("首次连接房间 {room} 于 {addr}\n证书 SHA-256: {fingerprint}\n请向房主通过其他渠道核对房间标识和完整指纹。输入 yes 确认: ");
                let mut lines = BufReader::new(tokio::io::stdin()).lines();
                if lines.next_line().await?.as_deref() != Some("yes") {
                    return Err("已取消首次信任".into());
                }
            }
            None => {}
        }
    }
    let conn = Arc::new(connect(addr, &fingerprint).await?);
    let room: RoomId = conn.room_info().room;
    if expected_room.is_some_and(|expected| expected != room) {
        return Err("公告房间标识与实际连接不一致".into());
    }
    if let Some(existing) = trust.get(room)? {
        if existing != fingerprint {
            return Err(format!("房间 {room} 的指纹已变更，拒绝连接").into());
        }
    }
    println!("已连接房间「{}」（{}）", conn.room_info().name, room);

    let conn_dyn: Arc<dyn Connection> = conn.clone();
    let client = Client::new(conn_dyn);

    // 未加入先说清人数：大厅只给摘要，不给消息
    client.lobby(room).await?;
    let Some(ServerEvent::Lobby { info }) = next_lobby(&client).await else {
        return Err("房间大厅不可用，拒绝建立信任".into());
    };
    if info.room != room {
        return Err("大厅房间标识与连接不一致".into());
    }
    trust.pin(room, &fingerprint)?;
    println!("当前在线 {} 人，输入 join 进入房间", info.online_count);

    // 事件打印任务
    let events_client = client.clone();
    tokio::spawn(async move {
        loop {
            match events_client.next_event().await {
                Ok(ServerEvent::Message { message }) => {
                    println!("[{}] {}", message.sender_name, message.text)
                }
                Ok(ServerEvent::Presence { online_count, .. }) => {
                    println!("[系统] 在线 {online_count} 人")
                }
                Ok(ServerEvent::Joined { online_count, .. }) => {
                    println!("[系统] 已加入，当前在线 {online_count} 人")
                }
                Ok(ServerEvent::Left { .. }) => println!("[系统] 已退出房间"),
                // 未加入（或刚退出）时服务端推的是摘要，同样要显示人数变化
                Ok(ServerEvent::Lobby { info }) => {
                    println!("[系统] 在线 {} 人", info.online_count)
                }
                Ok(ServerEvent::Error { message, .. }) => println!("[错误] {message}"),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    print!("> ");
    std::io::Write::flush(&mut std::io::stdout())?;
    while let Some(line) = lines.next_line().await? {
        let text = line.trim();
        if text.is_empty() {
            print!("> ");
            std::io::Write::flush(&mut std::io::stdout())?;
            continue;
        }
        match text {
            "join" => client.join(room, name.clone()).await?,
            "leave" => client.leave(room).await?,
            "quit" | "exit" => break,
            other => client.send_text(room, other).await?,
        }
        print!("> ");
        std::io::Write::flush(&mut std::io::stdout())?;
    }

    client.close().await.ok();
    Ok(())
}

async fn next_lobby(client: &Client) -> Option<ServerEvent> {
    for _ in 0..16 {
        let ev = tokio::time::timeout(Duration::from_secs(2), client.next_event())
            .await
            .ok()?
            .ok()?;
        if matches!(ev, ServerEvent::Lobby { .. }) {
            return Some(ev);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announcement(fingerprint: &str) -> RoomAnnouncement {
        RoomAnnouncement {
            room: RoomId::new(),
            name: "测试房间".into(),
            version: PROTOCOL_VERSION,
            fingerprint: fingerprint.into(),
            addrs: vec!["127.0.0.1:45678".parse().unwrap()],
        }
    }

    #[test]
    fn discovered_room_uses_its_fingerprint_unless_explicitly_pinned() {
        let (_, fingerprint) = discovered_target(announcement("advertised"), None).unwrap();
        assert_eq!(fingerprint, "advertised");

        let (_, fingerprint) =
            discovered_target(announcement("advertised"), Some("pinned".into())).unwrap();
        assert_eq!(fingerprint, "pinned");
    }

    #[test]
    fn discovery_without_fingerprint_is_rejected() {
        assert!(discovered_target(announcement(""), None).is_err());
    }
}
