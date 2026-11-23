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
        config.proxy_list_file.as_deref(),
        Arc::clone(&metrics),
    )?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run(config, metrics, cache, mojang));
    // Dropping the runtime would wait for running blocking tasks, such as DNS lookups.
    // Graceful shutdown has already awaited all required work, and a forced exit must
    // not wait at all.
    runtime.shutdown_background();
    result
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
        BatchConfig::default(),
    );
    let reporter = DiscordReporter::start(config.discord_webhook, Arc::clone(&metrics));
    let app = build_router(
        AppState {
            batch_lookup: batch_processor.lookup(),
            cache,
            mojang,
            metrics,
        },
        config.trusted_proxies,
    );
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
    // Install the handlers once, before returning. Installing them inside the task
    // would leave the default handlers active until the task is first polled, so an
    // early signal would kill the process, and signals that arrive while no handler
    // is subscribed would be lost.
    let mut signals = ShutdownSignals::install();
    tokio::spawn(async move {
        loop {
            signals.recv().await;
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

#[cfg(unix)]
struct ShutdownSignals {
    interrupt: Option<signal::unix::Signal>,
    terminate: Option<signal::unix::Signal>,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn install() -> Self {
        use signal::unix::{SignalKind, signal};

        let install = |kind, name| {
            signal(kind)
                .inspect_err(|error| error!(%error, "could not install {name} signal handler"))
                .ok()
        };
        Self {
            interrupt: install(SignalKind::interrupt(), "SIGINT"),
            terminate: install(SignalKind::terminate(), "SIGTERM"),
        }
    }

    async fn recv(&mut self) {
        async fn next(signal: Option<&mut signal::unix::Signal>) {
            if let Some(signal) = signal
                && signal.recv().await.is_some()
            {
                return;
            }
            // A missing handler or a stopped signal driver never resolves.
            std::future::pending::<()>().await;
        }

        tokio::select! {
            () = next(self.interrupt.as_mut()) => {}
            () = next(self.terminate.as_mut()) => {}
        }
    }
}

#[cfg(not(unix))]
struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    fn install() -> Self {
        Self
    }

    async fn recv(&mut self) {
        if let Err(error) = signal::ctrl_c().await {
            error!(%error, "could not install Ctrl+C signal handler");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::{process::Command, time::Duration};

    use tokio::time::timeout;

    use super::{listen_for_signals, signal_count};

    // Blocking on purpose: an await point would let the listener task run first.
    #[expect(
        clippy::disallowed_methods,
        reason = "the test must not yield to the runtime"
    )]
    fn send_signal(name: &str) {
        let status = Command::new("kill")
            .args([format!("-{name}"), std::process::id().to_string()])
            .status()
            .expect("kill should run");
        assert!(status.success(), "kill -{name} should succeed");
    }

    #[tokio::test]
    async fn counts_signals_sent_before_the_listener_task_runs() {
        let signals = listen_for_signals();
        // The current-thread runtime has not polled the listener task yet, so these
        // signals would terminate the test process without installed handlers.
        send_signal("TERM");
        send_signal("INT");
        timeout(Duration::from_secs(5), signal_count(signals, 2))
            .await
            .expect("both signals should be counted");
    }
}
