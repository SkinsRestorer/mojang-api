use std::{error::Error, process::ExitCode, sync::Arc};

use mojang_api::{
    AppState, BatchConfig, BatchProcessor, CacheManager, Config, Metrics, MojangHttpClient,
    MojangService, build_router, error::ErrorChain, reporter::DiscordReporter, server,
};
use tokio::{signal, sync::watch};
use tracing::{error, info, level_filters::LevelFilter, warn};
use tracing_subscriber::EnvFilter;

type BoxError = Box<dyn Error + Send + Sync>;

fn main() -> ExitCode {
    // Load .env before starting any runtime threads that could read the environment.
    let dotenv = dotenvy::dotenv();
    let invalid_log_filter = match initialize_tracing() {
        Ok(invalid_log_filter) => invalid_log_filter,
        Err(error) => {
            eprintln!("could not initialize logging: {}", ErrorChain(&*error));
            return ExitCode::FAILURE;
        }
    };
    if invalid_log_filter {
        warn!("RUST_LOG is invalid; using the default info filter");
    }
    if let Err(error) = dotenv
        && !error.not_found()
    {
        // Parse errors can contain the original line, including credentials.
        warn!("could not load .env");
    }

    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!("{}", ErrorChain(&*error));
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<(), BoxError> {
    let config = Config::from_env()?;
    let metrics = Arc::new(Metrics::default());
    let cache = CacheManager::default();
    let mojang: Arc<dyn MojangService> = Arc::new(MojangHttpClient::new(
        config.endpoints.clone(),
        config.proxy_list_file.as_deref(),
        config.request_timeout,
        Arc::clone(&metrics),
    )?);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(config, metrics, cache, mojang))
}

async fn run(
    config: Config,
    metrics: Arc<Metrics>,
    cache: CacheManager,
    mojang: Arc<dyn MojangService>,
) -> Result<(), BoxError> {
    let signals = listen_for_signals();
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
            cache,
            mojang,
            metrics,
        },
        config.server_port,
    )?;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.server_port)).await?;
    info!(
        address = %listener.local_addr()?,
        "server started; Swagger UI is available at /swagger"
    );

    let first_signal = signal_count(signals.clone(), 1);
    let graceful_shutdown = async {
        server::serve(listener, app, first_signal).await;
        reporter.shutdown().await;
        batch_processor.shutdown().await;
    };
    tokio::select! {
        () = graceful_shutdown => {
            info!("server stopped");
            Ok(())
        }
        () = signal_count(signals, 2) => {
            Err("received a second shutdown signal; stopping without draining".into())
        }
    }
}

/// Counts shutdown signals, so a second signal can force an immediate exit.
fn listen_for_signals() -> watch::Receiver<u32> {
    let (sender, receiver) = watch::channel(0_u32);
    tokio::spawn(async move {
        loop {
            shutdown_signal().await;
            sender.send_modify(|count| *count = count.saturating_add(1));
            info!("shutdown signal received");
        }
    });
    receiver
}

async fn signal_count(mut signals: watch::Receiver<u32>, count: u32) {
    if signals
        .wait_for(|received| *received >= count)
        .await
        .is_err()
    {
        std::future::pending::<()>().await;
    }
}

/// Initializes logging and reports whether `RUST_LOG` was invalid.
fn initialize_tracing() -> Result<bool, BoxError> {
    let builder = EnvFilter::builder().with_default_directive(LevelFilter::INFO.into());
    let (filter, invalid) = match builder.from_env() {
        Ok(filter) => (filter, false),
        Err(_) => (EnvFilter::new("info"), true),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()?;
    Ok(invalid)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = signal::ctrl_c().await {
            error!(%error, "could not install Ctrl+C signal handler");
            std::future::pending::<()>().await;
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
}
