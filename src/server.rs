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
        .header_read_timeout(HEADER_READ_TIMEOUT);
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
    use std::time::Duration;

    use axum::{Router, extract::ConnectInfo, routing::get};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::oneshot,
    };

    use super::serve;

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
        let router = Router::new().route(
            "/",
            get(
                |ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>| async move {
                    peer.ip().to_string()
                },
            ),
        );
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
}
