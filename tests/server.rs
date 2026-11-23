//! Runs the router through the real TCP server, so these tests cover behavior that depends on
//! `ConnectInfo`, connection handling, and graceful shutdown.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ipnet::IpNet;
use mojang_api::{
    AppState, BatchConfig, BatchProcessor, CacheManager, Metrics, MojangService, build_router,
    mojang::UpstreamError, server, types::SkinProperty,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, oneshot},
    task::JoinHandle,
    time::{Instant, timeout},
};
use uuid::Uuid;

const PLAYER_UUID: &str = "b1ae0778-4817-436c-96a3-a72c67cda060";
const WAIT: Duration = Duration::from_secs(5);

/// Answers like Mojang for one known player and can hold skin lookups until released.
#[derive(Default)]
struct FakeMojang {
    name_batches: Mutex<Vec<Vec<String>>>,
    skin_lookups: AtomicUsize,
    hold_skins: bool,
    skin_started: Notify,
    release_skins: Notify,
}

#[async_trait]
impl MojangService for FakeMojang {
    async fn lookup_names(&self, names: &[String]) -> Result<Vec<(String, Uuid)>, UpstreamError> {
        self.name_batches
            .lock()
            .expect("batch log should not be poisoned")
            .push(names.to_vec());
        Ok(names
            .iter()
            .filter(|name| name.eq_ignore_ascii_case("Pistonmaster"))
            .map(|_| ("Pistonmaster".to_owned(), player_uuid()))
            .collect())
    }

    async fn lookup_skin(&self, uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError> {
        self.skin_lookups.fetch_add(1, Ordering::SeqCst);
        if self.hold_skins {
            self.skin_started.notify_one();
            self.release_skins.notified().await;
        }
        Ok((uuid == player_uuid()).then(|| SkinProperty {
            value: "texture".to_owned(),
            signature: "signature".to_owned(),
        }))
    }
}

fn player_uuid() -> Uuid {
    Uuid::parse_str(PLAYER_UUID).expect("test UUID should parse")
}

struct TestServer {
    address: SocketAddr,
    mojang: Arc<FakeMojang>,
    metrics: Arc<Metrics>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
    batch: BatchProcessor,
}

impl TestServer {
    async fn start(mojang: FakeMojang, trusted_proxies: Vec<IpNet>) -> Self {
        let mojang = Arc::new(mojang);
        let service: Arc<dyn MojangService> = Arc::clone(&mojang) as Arc<dyn MojangService>;
        let metrics = Arc::new(Metrics::default());
        let cache = CacheManager::default();
        let batch = BatchProcessor::start(
            Arc::clone(&service),
            cache.clone(),
            Arc::clone(&metrics),
            BatchConfig::new(10, Duration::from_millis(5), 32, 2)
                .expect("test batch configuration should be valid"),
        );
        let router = build_router(
            AppState {
                batch_lookup: batch.lookup(),
                cache,
                mojang: service,
                metrics: Arc::clone(&metrics),
            },
            trusted_proxies,
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server::serve(listener, router, async {
            drop(stopped.await);
        }));
        Self {
            address,
            mojang,
            metrics,
            stop: Some(stop),
            task,
            batch,
        }
    }

    fn begin_shutdown(&mut self) {
        self.stop
            .take()
            .expect("shutdown should start once")
            .send(())
            .expect("server should be waiting for shutdown");
    }

    async fn stop(mut self) {
        self.begin_shutdown();
        timeout(WAIT, self.task)
            .await
            .expect("server should stop")
            .expect("server task should not panic");
        self.batch.shutdown().await;
    }
}

struct HttpResponse {
    status: u16,
    head: String,
    body: String,
}

impl HttpResponse {
    fn parse(raw: &str) -> Self {
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("response should have a header section: {raw:?}"));
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or_else(|| panic!("response should have a status line: {head:?}"));
        Self {
            status,
            head: head.to_ascii_lowercase(),
            body: body.to_owned(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| *key == name)
                .map(|(_, value)| value.trim())
        })
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("body should be JSON ({error}): {:?}", self.body))
    }
}

fn request_bytes(path: &str, extra_headers: &[(&str, &str)], close: bool) -> String {
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: test\r\n");
    for (name, value) in extra_headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    if close {
        request.push_str("Connection: close\r\n");
    }
    request.push_str("\r\n");
    request
}

async fn send(stream: &mut TcpStream, path: &str, extra_headers: &[(&str, &str)]) {
    stream
        .write_all(request_bytes(path, extra_headers, true).as_bytes())
        .await
        .expect("request should be written");
}

async fn read_response(stream: &mut TcpStream) -> HttpResponse {
    let mut raw = String::new();
    timeout(WAIT, stream.read_to_string(&mut raw))
        .await
        .expect("response should arrive in time")
        .expect("response should be readable");
    HttpResponse::parse(&raw)
}

async fn get_with(address: SocketAddr, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
    let mut stream = TcpStream::connect(address)
        .await
        .expect("test client should connect");
    send(&mut stream, path, headers).await;
    read_response(&mut stream).await
}

async fn get(address: SocketAddr, path: &str) -> HttpResponse {
    get_with(address, path, &[]).await
}

#[tokio::test]
async fn serves_lookups_over_tcp_and_caches_them() {
    let server = TestServer::start(FakeMojang::default(), Vec::new()).await;

    let found = get(server.address, "/mojang/uuid/PISTONMASTER").await;
    assert_eq!(found.status, 200, "{}", found.body);
    assert_eq!(
        found.json(),
        serde_json::json!({"exists": true, "uuid": PLAYER_UUID})
    );
    assert_eq!(found.header("cache-control"), Some("public, max-age=900"));
    assert_eq!(found.header("x-content-type-options"), Some("nosniff"));

    // A different spelling of the same name is served from the cache.
    let cached = get(server.address, "/mojang/uuid/pistonmaster").await;
    assert_eq!(cached.json(), found.json());

    let missing = get(server.address, "/mojang/uuid/nobody").await;
    assert_eq!(
        missing.json(),
        serde_json::json!({"exists": false, "uuid": null})
    );
    assert_eq!(missing.header("cache-control"), Some("public, max-age=60"));

    let skin_path = format!("/mojang/skin/{}", PLAYER_UUID.replace('-', ""));
    for _ in 0..2 {
        let skin = get(server.address, &skin_path).await;
        assert_eq!(skin.status, 200, "{}", skin.body);
        assert_eq!(
            skin.json(),
            serde_json::json!({
                "exists": true,
                "skinProperty": {"value": "texture", "signature": "signature"}
            })
        );
    }

    let batches = server
        .mojang
        .name_batches
        .lock()
        .expect("batch log should not be poisoned")
        .clone();
    assert_eq!(
        batches,
        vec![vec!["pistonmaster".to_owned()], vec!["nobody".to_owned()]],
        "each name should reach Mojang once, in lowercase"
    );
    assert_eq!(
        server.mojang.skin_lookups.load(Ordering::SeqCst),
        1,
        "the second skin request should be a cache hit"
    );

    let metrics = server.metrics.snapshot();
    assert_eq!(metrics.uuid_requests, 3);
    assert_eq!(metrics.uuid_cache_hits, 1);
    assert_eq!(metrics.uuid_cache_misses, 2);
    assert_eq!(metrics.skin_requests, 2);
    assert_eq!(metrics.skin_cache_hits, 1);
    assert_eq!(metrics.skin_cache_misses, 1);

    server.stop().await;
}

/// Sends requests as the client that `headers` identify until the rate limiter rejects one.
///
/// The limiter refills one request every 60 ms, so a little over 1,000 requests get through.
async fn exhaust_rate_limit(address: SocketAddr, headers: &[(&str, &str)]) {
    let mut allowed = 0_u32;
    loop {
        // Invalid UUIDs are rejected without calling Mojang but still count toward the limit.
        let response = get_with(address, "/mojang/skin/invalid", headers).await;
        if response.status == 429 {
            assert_eq!(
                response.json(),
                serde_json::json!({"error": "RATE_LIMITED"})
            );
            assert_eq!(response.header("ratelimit-remaining"), Some("0"));
            assert!(
                response.header("retry-after").is_some(),
                "{}",
                response.head
            );
            break;
        }
        assert_eq!(response.status, 400, "{}", response.body);
        allowed += 1;
        assert!(allowed <= 1_100, "the rate limiter should reject requests");
    }
    assert!(allowed >= 1_000, "only {allowed} requests were allowed");
}

/// Returns `RateLimit-Remaining` for one more request from the client that `headers` identify.
async fn remaining(address: SocketAddr, headers: &[(&str, &str)]) -> u32 {
    let response = get_with(address, "/mojang/skin/invalid", headers).await;
    response
        .header("ratelimit-remaining")
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "response should report the remaining quota: {}",
                response.head
            )
        })
}

#[tokio::test]
async fn rate_limits_by_peer_address() {
    let server = TestServer::start(FakeMojang::default(), Vec::new()).await;

    exhaust_rate_limit(server.address, &[]).await;
    let health = get(server.address, "/health").await;
    assert_eq!(health.status, 200, "health checks should bypass the limit");

    server.stop().await;
}

#[tokio::test]
async fn ignores_forwarding_headers_from_untrusted_peers() {
    let server = TestServer::start(FakeMojang::default(), Vec::new()).await;

    for _ in 0..50 {
        remaining(server.address, &[]).await;
    }
    let spoofed = remaining(server.address, &[("CF-Connecting-IP", "203.0.113.7")]).await;
    // A new client would have 999 requests left. Refills add at most one request per 60 ms.
    assert!(
        spoofed < 990,
        "an untrusted peer must not choose its own rate limit key ({spoofed} left)"
    );

    server.stop().await;
}

#[tokio::test]
async fn rate_limits_forwarded_clients_separately_behind_a_trusted_proxy() {
    let trusted = vec!["127.0.0.1/32".parse().expect("test network should parse")];
    let server = TestServer::start(FakeMojang::default(), trusted).await;

    exhaust_rate_limit(server.address, &[("CF-Connecting-IP", "203.0.113.7")]).await;
    let other_client = remaining(server.address, &[("CF-Connecting-IP", "203.0.113.8")]).await;
    assert_eq!(other_client, 999);
    let proxy_itself = remaining(server.address, &[]).await;
    assert_eq!(
        proxy_itself, 999,
        "requests without the header use the peer address"
    );

    server.stop().await;
}

#[tokio::test]
async fn graceful_shutdown_drains_in_flight_requests_and_closes_idle_connections() {
    let mut server = TestServer::start(
        FakeMojang {
            hold_skins: true,
            ..FakeMojang::default()
        },
        Vec::new(),
    )
    .await;

    // An idle keep-alive connection must not hold up shutdown.
    let mut idle = TcpStream::connect(server.address)
        .await
        .expect("idle client should connect");
    idle.write_all(request_bytes("/health", &[], false).as_bytes())
        .await
        .expect("request should be written");
    let mut first_response = [0_u8; 12];
    idle.read_exact(&mut first_response)
        .await
        .expect("idle client should receive a response");
    assert_eq!(&first_response, b"HTTP/1.1 200");

    let mut in_flight = TcpStream::connect(server.address)
        .await
        .expect("client should connect");
    send(&mut in_flight, &format!("/mojang/skin/{PLAYER_UUID}"), &[]).await;
    timeout(WAIT, server.mojang.skin_started.notified())
        .await
        .expect("the skin lookup should start");

    server.begin_shutdown();
    // The server must keep waiting while a request is still running.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !server.task.is_finished(),
        "shutdown should wait for requests"
    );
    assert!(
        TcpStream::connect(server.address).await.is_err(),
        "the server should stop accepting connections"
    );

    server.mojang.release_skins.notify_one();
    let response = read_response(&mut in_flight).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.json()["exists"], true);

    timeout(WAIT, &mut server.task)
        .await
        .expect("server should stop after draining")
        .expect("server task should not panic");
    let mut rest = Vec::new();
    timeout(WAIT, idle.read_to_end(&mut rest))
        .await
        .expect("idle connection should be closed")
        .expect("idle connection should close cleanly");
    server.batch.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn closes_connections_that_do_not_finish_their_headers() {
    let server = TestServer::start(FakeMojang::default(), Vec::new()).await;
    let mut stream = TcpStream::connect(server.address)
        .await
        .expect("test client should connect");
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: test\r\n")
        .await
        .expect("partial request should be written");

    let started = Instant::now();
    let mut raw = Vec::new();
    timeout(Duration::from_secs(60), stream.read_to_end(&mut raw))
        .await
        .expect("the server should close the connection")
        .expect("the connection should close cleanly");
    assert!(
        started.elapsed() >= Duration::from_secs(10),
        "the connection closed after {:?}",
        started.elapsed()
    );
    let raw = String::from_utf8_lossy(&raw);
    assert!(
        !raw.starts_with("HTTP/1.1 200"),
        "the incomplete request must not be served: {raw}"
    );

    server.stop().await;
}
