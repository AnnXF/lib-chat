//! 局域网聊天工具的命令行形态，用于在没有界面时验证整条链路。
//!
//! ```text
//! libchat host [--port 0] [--name 房间名]
//! libchat join [--addr ip:port] [--fingerprint 指纹] [--name 昵称]
//! ```

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chat_client::Client;
use chat_core::{Connection, ConnectionAcceptor};
use chat_discovery::{PROTOCOL_VERSION, RoomAnnouncer, RoomBrowser};
use chat_proto::{RoomId, ServerEvent};
use chat_server::{Acceptor, Server};
use chat_transport_quic::{connect, QuicServer};
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
         libchat join [--addr ip:port] [--fingerprint 指纹] [--name 昵称]"
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
        println!(
            "[状态] 在线 {} / 成员 {}",
            server.online_count().await,
            server.member_count().await
        );
    }
}

async fn join(args: &[String]) -> Result<(), AnyErr> {
    let name = opt(args, "--name").unwrap_or_else(|| "我".into());

    let addr: SocketAddr = match opt(args, "--addr") {
        Some(a) => a.parse()?,
        None => {
            println!("正在搜索局域网房间...");
            let mut browser = RoomBrowser::start()?;
            let ann = tokio::time::timeout(Duration::from_secs(15), browser.next_room())
                .await
                .map_err(|_| "搜索房间超时，可用 --addr 手动指定")??;
            if ann.version != PROTOCOL_VERSION {
                return Err(format!("协议版本不兼容: {}", ann.version).into());
            }
            let addr = *ann.addrs.first().ok_or("房间未提供可用地址")?;
            println!("发现房间「{}」于 {addr}", ann.name);
            addr
        }
    };

    let fingerprint = opt(args, "--fingerprint");
    let conn = Arc::new(connect(addr, fingerprint.as_deref()).await?);
    let room: RoomId = conn.room_info().room;
    println!("已连接房间「{}」（{}）", conn.room_info().name, room);

    let conn_dyn: Arc<dyn Connection> = conn.clone();
    let client = Client::new(conn_dyn);

    // 未加入先说清人数：大厅只给摘要，不给消息
    client.lobby(room).await?;
    if let Some(ServerEvent::Lobby { info }) = next_lobby(&client).await {
        println!(
            "当前在线 {} 人 / 共 {} 人，输入 join 进入房间",
            info.online_count, info.member_count
        );
    }

    // 事件打印任务
    let events_client = client.clone();
    tokio::spawn(async move {
        loop {
            match events_client.next_event().await {
                Ok(ServerEvent::Message { message }) => {
                    println!("[{}] {}", message.sender_name, message.text)
                }
                Ok(ServerEvent::Presence {
                    online_count,
                    member_count,
                    ..
                }) => println!("[系统] 在线 {online_count} / 成员 {member_count}"),
                Ok(ServerEvent::Joined { members, .. }) => {
                    println!("[系统] 已加入，当前成员 {} 人", members.len())
                }
                Ok(ServerEvent::Left { .. }) => println!("[系统] 已退出房间"),
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
