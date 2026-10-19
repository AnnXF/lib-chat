//! 后端压测：多客户端并发发送，检查顺序、去重、无丢失与发送时延。
//!
//! 默认跑轻量档（随 `cargo test` 执行，约一两秒）。重量档手动执行：
//! `cargo test -p chat-server --test load -- --ignored --nocapture`

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chat_core::{Client, DeviceId, Message, MessageStore, RoomId, Seq, ServerEvent};
use chat_server::{InMemoryStore, Server, SqliteStore};
use tokio::task::JoinHandle;

struct Report {
    per_client: Vec<Vec<Message>>,
    latencies: Vec<Duration>,
    elapsed: Duration,
}

async fn run_load(server: Arc<Server>, clients: usize, per_client: usize) -> Report {
    let room = server.room();
    let expected = clients * per_client;

    let mut connections = Vec::with_capacity(clients);
    for _ in 0..clients {
        connections.push(Arc::new(Client::new(server.connect(DeviceId::new()).await)));
    }
    for (index, client) in connections.iter().enumerate() {
        client
            .join(room, format!("c{index}"))
            .await
            .expect("加入房间失败");
        await_event(client, |event| matches!(event, ServerEvent::Joined { .. })).await;
    }

    // 每个连接都要持续读走广播，否则会被有界队列当作慢客户端清退
    let readers: Vec<JoinHandle<Vec<Message>>> = connections
        .iter()
        .map(|client| {
            let client = Arc::clone(client);
            tokio::spawn(async move {
                let mut received = Vec::with_capacity(expected);
                while received.len() < expected {
                    let event = tokio::time::timeout(Duration::from_secs(15), client.next_event())
                        .await
                        .expect("等待广播超时")
                        .expect("连接已关闭");
                    if let ServerEvent::Message { message } = event {
                        received.push(message);
                    }
                }
                received
            })
        })
        .collect();

    let started = Instant::now();
    let senders: Vec<JoinHandle<Vec<Duration>>> = connections
        .iter()
        .map(|client| {
            let client = Arc::clone(client);
            tokio::spawn(async move {
                let mut latencies = Vec::with_capacity(per_client);
                for index in 0..per_client {
                    let now = Instant::now();
                    client
                        .send_text(room, format!("压测消息 {index}"))
                        .await
                        .expect("发送失败");
                    latencies.push(now.elapsed());
                }
                latencies
            })
        })
        .collect();

    let mut latencies = Vec::with_capacity(expected);
    for sender in senders {
        latencies.extend(sender.await.expect("发送任务异常"));
    }
    let elapsed = started.elapsed();

    let mut per_client_messages = Vec::with_capacity(clients);
    for reader in readers {
        per_client_messages.push(reader.await.expect("读取任务异常"));
    }

    Report {
        per_client: per_client_messages,
        latencies,
        elapsed,
    }
}

async fn await_event<F>(client: &Client, predicate: F)
where
    F: Fn(&ServerEvent) -> bool,
{
    for _ in 0..32 {
        let event = tokio::time::timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("等待事件超时")
            .expect("连接已关闭");
        if predicate(&event) {
            return;
        }
    }
    panic!("未收到期望事件");
}

#[track_caller]
fn assert_contiguous(messages: &[Message], expected: usize) {
    assert_eq!(messages.len(), expected, "每次广播都应完整到达");
    let mut seqs: Vec<Seq> = messages.iter().map(|message| message.seq).collect();
    seqs.sort_unstable();
    let expected_seqs: Vec<Seq> = (1..=expected as Seq).collect();
    assert_eq!(
        seqs, expected_seqs,
        "序号必须从 1 连续递增，不能有空洞或重复"
    );

    let ids: HashSet<_> = messages
        .iter()
        .map(|message| message.client_msg_id)
        .collect();
    assert_eq!(ids.len(), expected, "消息标识不能重复");
}

fn percentile(latencies: &mut [Duration], percent: f64) -> Duration {
    latencies.sort_unstable();
    let index = ((latencies.len() as f64 - 1.) * percent).round() as usize;
    latencies[index]
}

fn print_report(label: &str, report: &Report, expected: usize) {
    let broadcast = report.per_client.len() * expected;
    let throughput = expected as f64 / report.elapsed.as_secs_f64();
    let mut latencies = report.latencies.clone();
    println!(
        "{label}：{} 个连接并发发送 {} 条，广播 {} 次，耗时 {:.2?}，落库吞吐 {:.0} 条/秒，\
         p50 {:.2?}，p99 {:.2?}",
        report.per_client.len(),
        expected,
        broadcast,
        report.elapsed,
        throughput,
        percentile(&mut latencies, 0.5),
        percentile(&mut latencies, 0.99),
    );
}

async fn history_of(store: &dyn MessageStore, room: RoomId, expected: usize) -> Vec<Message> {
    store
        .history(room, None, expected + 10)
        .await
        .expect("读取历史失败")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_senders_keep_sequence_and_dedup() {
    let clients = 6;
    let per_client = 20;
    let expected = clients * per_client;

    let store = Arc::new(InMemoryStore::default());
    let server = Server::with_room(RoomId::new(), "压测房间", store.clone());
    let report = run_load(Arc::clone(&server), clients, per_client).await;
    print_report("轻量压测", &report, expected);

    for messages in &report.per_client {
        assert_contiguous(messages, expected);
    }

    let mut latencies = report.latencies;
    assert!(
        percentile(&mut latencies, 0.99) < Duration::from_secs(1),
        "单条发送时延 p99 超过 1 秒"
    );

    let history = history_of(store.as_ref(), server.room(), expected).await;
    assert_contiguous(&history, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idempotent_retry_under_concurrency_does_not_double_store() {
    let store = Arc::new(InMemoryStore::default());
    let server = Server::with_room(RoomId::new(), "幂等压测", store.clone());
    let room = server.room();
    let client = Arc::new(Client::new(server.connect(DeviceId::new()).await));
    client.join(room, "Alice").await.unwrap();
    await_event(&client, |event| matches!(event, ServerEvent::Joined { .. })).await;

    let id = uuid::Uuid::new_v4();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let client = Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            client.send_text_with_id(room, id, "同一条").await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }

    let history = history_of(store.as_ref(), room, 1).await;
    assert_eq!(history.len(), 1, "并发重发同一标识只能落库一次");
    assert_eq!(history[0].text, "同一条");
}

/// SQLite 落库路径也要能承受并发送入，且历史与广播完全一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_store_survives_concurrent_senders() {
    let path = std::env::temp_dir().join(format!("libchat-load-{}.db", uuid::Uuid::new_v4()));
    let store = Arc::new(SqliteStore::open(&path).await.expect("打开压测库失败"));
    let room = store.room();
    let server = Server::with_room(room, "落库压测", store.clone());

    let clients = 4;
    let per_client = 20;
    let expected = clients * per_client;
    let report = run_load(Arc::clone(&server), clients, per_client).await;
    print_report("SQLite 压测", &report, expected);

    for messages in &report.per_client {
        assert_contiguous(messages, expected);
    }
    let history = history_of(store.as_ref(), room, expected).await;
    assert_contiguous(&history, expected);

    store.close().await;
    let _ = std::fs::remove_file(path);
}

/// 重量档：更多连接与消息，手动执行。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "重量级压测，手动执行"]
async fn heavy_load_reports_throughput() {
    let clients = 24;
    let per_client = 100;
    let expected = clients * per_client;

    let store = Arc::new(InMemoryStore::default());
    let server = Server::with_room(RoomId::new(), "重量压测", store.clone());
    let report = run_load(Arc::clone(&server), clients, per_client).await;
    print_report("重量压测", &report, expected);

    for messages in &report.per_client {
        assert_contiguous(messages, expected);
    }
    let history = history_of(store.as_ref(), server.room(), expected).await;
    assert_contiguous(&history, expected);
}
