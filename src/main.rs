use std::{error::Error, sync::Arc};

use mojang_api::{
    AppState, BatchConfig, BatchProcessor, CacheManager, Config, Metrics, MojangHttpClient,
    MojangService, build_router, reporter::DiscordReporter,
};
use tokio::signal;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    initialize_tracing();

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
        ),
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
    );
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.server_port)).await?;
    info!(
        port = config.server_port,
        "server started; Swagger UI is available at /swagger"
    );

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        error!(%error, "HTTP server stopped with an error");
    }

    reporter.shutdown().await;
    batch_processor.shutdown().await;
    cache.clear().await;
    info!("server stopped");
    Ok(())
}

fn initialize_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
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
