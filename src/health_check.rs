//! A client for `GET /health` that the binary runs with `--health-check`. Distroless images
//! have no shell or `curl`, so container health checks call the binary itself.

use std::{net::Ipv4Addr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

/// Covers connecting, sending the request, and reading the status line.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// The status line fits well within this limit.
const MAX_RESPONSE_BYTES: u64 = 1024;

/// Requests `/health` from the server on the local `port` and reports whether it answered
/// `200 OK`.
pub async fn check_health(port: u16) -> bool {
    timeout(HEALTH_CHECK_TIMEOUT, request_health(port))
        .await
        .is_ok_and(|healthy| healthy.unwrap_or(false))
}

async fn request_health(port: u16) -> std::io::Result<bool> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .await?;
    Ok(response.starts_with(b"HTTP/1.1 200 "))
}
