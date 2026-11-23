use std::time::Duration;

use reqwest::{Client, ClientBuilder};

pub(crate) fn http_client_builder(request_timeout: Duration) -> ClientBuilder {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

    #[expect(
        clippy::disallowed_methods,
        reason = "This shared builder sets the timeouts and User-Agent for all clients."
    )]
    let builder = Client::builder();
    builder
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(request_timeout)
        .user_agent("SRMojangAPI")
}
