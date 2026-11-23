use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use axum::{Router, body::Body, extract::ConnectInfo};
use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::graceful::GracefulShutdown,
};
use tokio::{net::TcpListener, sync::Semaphore, time::timeout};
use tower::ServiceExt;
use tracing::{debug, error, warn};

use crate::error::ErrorChain;

/// Closes connections that do not send complete request headers in time, including idle
/// keep-alive connections.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Caps the bytes buffered for each request head. Hyper's default of about 400 KiB would let
/// [`MAX_CONNECTIONS`] slow clients hold over 1.5 GiB. This still exceeds the 32 KiB of
/// request headers that Cloudflare forwards.
const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 4_096;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Serves `router` until `shutdown` completes, then drains open connections.
///
/// Each request carries the peer address as [`ConnectInfo<SocketAddr>`].
pub async fn serve(listener: TcpListener, router: Router, shutdown: impl Future<Output = ()>) {
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .max_buf_size(MAX_REQUEST_HEAD_BYTES);
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);

    loop {
        let permit = tokio::select! {
            () = &mut shutdown => break,
            permit = Arc::clone(&connections).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let (stream, peer) = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Errors such as running out of file descriptors are usually temporary.
                    warn!(error = %ErrorChain(&error), "could not accept a connection");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    continue;
                }
            },
        };

        let router = router.clone();
        let service = service_fn(move |mut request: Request<Incoming>| {
            request
                .extensions_mut()
                .insert(ConnectInfo::<SocketAddr>(peer));
            router.clone().oneshot(request.map(Body::new))
        });
        let connection = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                debug!(%peer, error = %ErrorChain(&error), "connection closed with an error");
            }
            drop(permit);
        });
    }

    drop(listener);
    if timeout(DRAIN_TIMEOUT, graceful.shutdown()).await.is_err() {
        error!("HTTP connections did not drain before the shutdown deadline");
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::{Router, extract::ConnectInfo, routing::get};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{Notify, oneshot},
    };

    use super::{HEADER_READ_TIMEOUT, MAX_REQUEST_HEAD_BYTES, serve};

    const LARGE_BODY_BYTES: usize = 4 * MAX_REQUEST_HEAD_BYTES;

    async fn start() -> (
        std::net::SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let router = Router::new()
            .route(
                "/",
                get(
                    |ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>| async move {
                        peer.ip().to_string()
                    },
                ),
            )
            .route("/large", get(|| async { "x".repeat(LARGE_BODY_BYTES) }));
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve(listener, router, async {
            drop(stopped.await);
        }));
        (address, stop, task)
    }

    #[tokio::test]
    async fn passes_the_peer_address_and_shuts_down() {
        let (address, stop, task) = start().await;
        let mut stream = TcpStream::connect(address)
            .await
            .expect("test client should connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .expect("request should be written");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("response should be readable");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("127.0.0.1"), "{response}");

        stop.send(())
            .expect("server should be waiting for shutdown");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("server should stop")
            .expect("server task should not panic");
    }

    #[tokio::test]
    async fn drains_in_flight_requests_and_closes_idle_connections() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let router = Router::new().route("/", get(|| async { "idle" })).route(
            "/slow",
            get({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move || async move {
                    started.notify_one();
                    release.notified().await;
                    "slow"
                }
            }),
        );
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve(listener, router, async {
            drop(stopped.await);
        }));

        // A keep-alive connection that has finished its request and now sits idle.
        let mut idle = TcpStream::connect(address)
            .await
            .expect("idle client should connect");
        idle.write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .expect("idle request should be written");
        let mut buffer = [0_u8; 1024];
        let read = idle
            .read(&mut buffer)
            .await
            .expect("idle response should be readable");
        assert!(buffer[..read].starts_with(b"HTTP/1.1 200"), "{buffer:?}");

        let mut slow = TcpStream::connect(address)
            .await
            .expect("slow client should connect");
        slow.write_all(b"GET /slow HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .expect("slow request should be written");
        started.notified().await;

        stop.send(())
            .expect("server should be waiting for shutdown");

        // The idle connection closes without waiting for the in-flight request.
        let read = tokio::time::timeout(Duration::from_secs(5), idle.read(&mut buffer))
            .await
            .expect("idle connection should close during the drain")
            .expect("idle connection should close cleanly");
        assert_eq!(read, 0, "idle connection should receive no more data");
        assert!(
            !task.is_finished(),
            "server should wait for the in-flight request"
        );
        assert!(
            TcpStream::connect(address).await.is_err(),
            "server should stop accepting connections"
        );

        release.notify_one();
        let mut response = String::new();
        slow.read_to_string(&mut response)
            .await
            .expect("slow response should be readable");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("slow"), "{response}");

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("server should stop after the drain")
            .expect("server task should not panic");
    }

    /// Reads until the server closes the connection and returns how long that took.
    async fn time_until_closed(stream: &mut TcpStream) -> Duration {
        let started = tokio::time::Instant::now();
        let mut rest = Vec::new();
        stream
            .read_to_end(&mut rest)
            .await
            .expect("connection should close cleanly");
        assert!(rest.is_empty(), "{:?}", String::from_utf8_lossy(&rest));
        started.elapsed()
    }

    // The paused clock skips ahead whenever the runtime waits, so the timeout takes no real time.
    #[tokio::test(start_paused = true)]
    async fn closes_slow_and_idle_connections_after_the_header_read_timeout() {
        let (address, stop, task) = start().await;

        let mut slow = TcpStream::connect(address)
            .await
            .expect("slow client should connect");
        slow.write_all(b"GET / HTTP/1.1\r\nHost: te")
            .await
            .expect("partial request should be written");
        let elapsed = time_until_closed(&mut slow).await;
        assert!(elapsed >= HEADER_READ_TIMEOUT, "closed after {elapsed:?}");

        let mut idle = TcpStream::connect(address)
            .await
            .expect("idle client should connect");
        idle.write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .expect("request should be written");
        let mut buffer = [0_u8; 1024];
        let read = idle
            .read(&mut buffer)
            .await
            .expect("response should be readable");
        assert!(buffer[..read].starts_with(b"HTTP/1.1 200"), "{buffer:?}");
        let elapsed = time_until_closed(&mut idle).await;
        assert!(elapsed >= HEADER_READ_TIMEOUT, "closed after {elapsed:?}");

        stop.send(())
            .expect("server should be waiting for shutdown");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("server should stop")
            .expect("server task should not panic");
    }

    async fn send(address: std::net::SocketAddr, request: &[u8]) -> String {
        let mut stream = TcpStream::connect(address)
            .await
            .expect("test client should connect");
        stream
            .write_all(request)
            .await
            .expect("request should be written");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("response should be readable");
        String::from_utf8(response).expect("response should be UTF-8")
    }

    #[tokio::test]
    async fn rejects_oversized_request_heads_but_sends_large_responses() {
        let (address, stop, task) = start().await;
        // Hyper checks the limit between reads, so it may accept a head slightly over it.
        let padding = "a".repeat(2 * MAX_REQUEST_HEAD_BYTES);
        let oversized = format!(
            "GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\nX-Padding: {padding}\r\n\r\n"
        );
        let response = send(address, oversized.as_bytes()).await;
        assert!(response.starts_with("HTTP/1.1 431"), "{response}");

        let response = send(
            address,
            b"GET /large HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response:.100}");
        assert!(
            response.ends_with(&"x".repeat(LARGE_BODY_BYTES)),
            "the whole response body should arrive"
        );

        stop.send(())
            .expect("server should be waiting for shutdown");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("server should stop")
            .expect("server task should not panic");
    }
}
