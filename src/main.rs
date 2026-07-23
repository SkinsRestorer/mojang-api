use std::{error::Error, future::IntoFuture, sync::Arc, time::Duration};

use mojang_api::{
    AppState, BatchConfig, BatchProcessor, CacheManager, Config, Metrics, MojangHttpClient,
    MojangService, build_router, reporter::DiscordReporter,
};
use tokio::{signal, sync::oneshot, time::timeout};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const HTTP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    initialize_tracing()?;

    let config = Config::from_env()?;
    let metrics = Arc::new(Metrics::default());
    let cache = CacheManager::default();
    let mojang: Arc<dyn MojangService> = Arc::new(MojangHttpClient::new(
        config.endpoints.clone(),
        config.proxy_list_file.as_deref(),
        config.request_timeout,
        Arc::clone(&metrics),
    )?);
    let batch_processor = BatchProcessor::start(
        Arc::clone(&mojang),
        cache.clone(),
        Arc::clone(&metrics),
        BatchConfig::new(
            config.batch_size,
            config.batch_interval,
            config.queue_capacity,
            config.max_in_flight_batches,
        )?,
    );
    let reporter = DiscordReporter::start(config.discord_webhook.clone(), Arc::clone(&metrics));
    let app = build_router(
        AppState {
            batch_lookup: batch_processor.lookup(),
            cache: cache.clone(),
            mojang,
            metrics,
        },
        config.server_port,
    )?;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.server_port)).await?;
    info!(
        port = config.server_port,
        "server started; Swagger UI is available at /swagger"
    );

    serve_until_shutdown(listener, app).await;

    reporter.shutdown().await;
    batch_processor.shutdown().await;
    cache.clear().await;
    info!("server stopped");
    Ok(())
}

async fn serve_until_shutdown(listener: tokio::net::TcpListener, app: axum::Router) {
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown_receiver.await;
        })
        .into_future();
    tokio::pin!(server);

    let result = tokio::select! {
        result = &mut server => Some(result),
        () = shutdown_signal() => {
            let _ = shutdown_sender.send(());
            if let Ok(result) = timeout(HTTP_SHUTDOWN_TIMEOUT, &mut server).await {
                Some(result)
            } else {
                error!("HTTP server did not drain before the shutdown deadline");
                None
            }
        }
    };

    if let Some(Err(error)) = result {
        error!(%error, "HTTP server stopped with an error");
    }
}

fn initialize_tracing() -> Result<(), Box<dyn Error + Send + Sync>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).try_init()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = signal::ctrl_c().await {
            error!(%error, "could not install Ctrl+C signal handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                error!(%error, "could not install SIGTERM signal handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    info!("shutdown signal received");
}
