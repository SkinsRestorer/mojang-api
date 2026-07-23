use std::{env, num::ParseIntError, path::PathBuf, time::Duration};

use reqwest::Url;
use thiserror::Error;

const DEFAULT_SERVER_PORT: u16 = 3000;
const DEFAULT_BATCH_SIZE: usize = 10;
const DEFAULT_BATCH_INTERVAL: Duration = Duration::from_secs(3);
const DEFAULT_QUEUE_CAPACITY: usize = 1_024;
const DEFAULT_MAX_IN_FLIGHT_BATCHES: usize = 16;

#[derive(Debug, Clone)]
pub struct MojangEndpoints {
    pub batch_urls: Vec<Url>,
    pub profile_base_url: Url,
}

impl MojangEndpoints {
    fn production() -> Result<Self, ConfigError> {
        Ok(Self {
            batch_urls: [
                "https://api.mojang.com/profiles/minecraft",
                "https://api.minecraftservices.com/minecraft/profile/lookup/bulk/byname",
            ]
            .into_iter()
            .map(Url::parse)
            .collect::<Result<_, _>>()?,
            profile_base_url: Url::parse(
                "https://sessionserver.mojang.com/session/minecraft/profile/",
            )?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server_port: u16,
    pub proxy_list_file: Option<PathBuf>,
    pub discord_webhook: Option<Url>,
    pub endpoints: MojangEndpoints,
    pub request_timeout: Duration,
    pub batch_size: usize,
    pub batch_interval: Duration,
    pub queue_capacity: usize,
    pub max_in_flight_batches: usize,
}

impl Config {
    /// Loads application settings from environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured port, `DISCORD_WEBHOOK`, or a built-in endpoint is
    /// invalid.
    pub fn from_env() -> Result<Self, ConfigError> {
        let server_port = env::var("SERVER_PORT")
            .ok()
            .or_else(|| env::var("PORT").ok())
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(DEFAULT_SERVER_PORT);
        let proxy_list_file = env::var_os("PROXY_LIST_FILE").map(PathBuf::from);
        let discord_webhook = env::var("DISCORD_WEBHOOK")
            .ok()
            .map(|value| Url::parse(&value))
            .transpose()?;

        Ok(Self {
            server_port,
            proxy_list_file,
            discord_webhook,
            endpoints: MojangEndpoints::production()?,
            request_timeout: Duration::from_secs(15),
            batch_size: DEFAULT_BATCH_SIZE,
            batch_interval: DEFAULT_BATCH_INTERVAL,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_in_flight_batches: DEFAULT_MAX_IN_FLIGHT_BATCHES,
        })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("SERVER_PORT or PORT must be a valid TCP port")]
    InvalidPort(#[from] ParseIntError),
    #[error("an application URL is invalid")]
    InvalidUrl(#[from] url::ParseError),
}
