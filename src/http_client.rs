use std::time::Duration;

use reqwest::{Client, ClientBuilder};
use rustls::crypto::{CryptoProvider, ring};

pub(crate) fn http_client_builder(request_timeout: Duration) -> ClientBuilder {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

    install_crypto_provider();
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

/// Installs the process-wide `ring` provider that reqwest's
/// `rustls-no-provider` feature requires. Without one, building a client panics.
fn install_crypto_provider() {
    if CryptoProvider::get_default().is_none() {
        // This fails only if another thread installed a provider first.
        drop(ring::default_provider().install_default());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::http_client_builder;

    #[test]
    fn builds_https_only_client_with_installed_provider() {
        let result = http_client_builder(Duration::from_secs(1))
            .https_only(true)
            .build();

        assert!(result.is_ok(), "client build failed: {result:?}");
    }
}
