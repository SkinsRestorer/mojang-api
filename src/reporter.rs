use std::{fs, sync::Arc, time::Duration};

use reqwest::Url;
use serde_json::json;
use tokio::{
    sync::oneshot,
    task::JoinHandle,
    time::{MissedTickBehavior, interval},
};
use tracing::{error, info};

use crate::metrics::{Metrics, MetricsSnapshot};

const REPORT_INTERVAL: Duration = Duration::from_mins(5);

pub struct DiscordReporter {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl DiscordReporter {
    pub fn start(webhook: Option<Url>, metrics: Arc<Metrics>) -> Self {
        let Some(webhook) = webhook else {
            info!("DISCORD_WEBHOOK is not set; status reports are disabled");
            return Self {
                shutdown: None,
                task: None,
            };
        };

        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .https_only(true)
                .build()
            {
                Ok(client) => client,
                Err(error) => {
                    error!(%error, "could not create Discord HTTP client");
                    return;
                }
            };
            let mut ticker = interval(REPORT_INTERVAL);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            ticker.tick().await;

            loop {
                tokio::select! {
                    _ = ticker.tick() => send_report(&client, &webhook, &metrics).await,
                    _ = &mut shutdown_receiver => break,
                }
            }
        });
        info!("Discord status reporter started");
        Self {
            shutdown: Some(shutdown),
            task: Some(task),
        }
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            error!(%error, "Discord status reporter stopped unexpectedly");
        }
    }
}

#[allow(clippy::cast_precision_loss)]
async fn send_report(client: &reqwest::Client, webhook: &Url, metrics: &Metrics) {
    let snapshot = metrics.snapshot_and_reset();
    let total_requests = snapshot
        .uuid_requests
        .saturating_add(snapshot.skin_requests);
    let total_cache_hits = snapshot
        .uuid_cache_hits
        .saturating_add(snapshot.skin_cache_hits);
    let total_cache_misses = snapshot
        .uuid_cache_misses
        .saturating_add(snapshot.skin_cache_misses);
    let total_cache_lookups = total_cache_hits.saturating_add(total_cache_misses);
    let cache_hit_rate = if total_cache_lookups == 0 {
        "N/A".to_owned()
    } else {
        format!(
            "{:.1}%",
            total_cache_hits as f64 / total_cache_lookups as f64 * 100.0
        )
    };
    let requests_per_minute = if snapshot.report_period.is_zero() {
        0.0
    } else {
        total_requests as f64 / snapshot.report_period.as_secs_f64() * 60.0
    };
    let color = match snapshot.mojang_errors {
        0 => 0x002e_cc71,
        1..=10 => 0x00f3_9c12,
        _ => 0x00e7_4c3c,
    };
    let rss = process_rss_bytes().map_or_else(|| "Unavailable".to_owned(), format_bytes);
    let load_average = load_average().unwrap_or_else(|| "Unavailable".to_owned());

    let payload = json!({
        "embeds": [{
            "title": "Mojang API Proxy - Status Report",
            "color": color,
            "fields": [
                {
                    "name": "Server",
                    "value": format!(
                        "**Uptime:** {}\n**RSS:** {rss}\n**Load Avg:** {load_average}",
                        format_duration(snapshot.uptime)
                    ),
                    "inline": false
                },
                {
                    "name": "Requests (5min)",
                    "value": format!(
                        "**Total:** {}\n**UUID Lookups:** {}\n**Skin Lookups:** {}\n**Req/min:** {requests_per_minute:.1}",
                        format_number(total_requests),
                        format_number(snapshot.uuid_requests),
                        format_number(snapshot.skin_requests)
                    ),
                    "inline": true
                },
                {
                    "name": "Cache (5min)",
                    "value": format!(
                        "**Hits:** {}\n**Misses:** {}\n**Hit Rate:** {cache_hit_rate}\n**UUID:** {} hit / {} miss\n**Skin:** {} hit / {} miss",
                        format_number(total_cache_hits),
                        format_number(total_cache_misses),
                        format_number(snapshot.uuid_cache_hits),
                        format_number(snapshot.uuid_cache_misses),
                        format_number(snapshot.skin_cache_hits),
                        format_number(snapshot.skin_cache_misses)
                    ),
                    "inline": true
                },
                {
                    "name": "Batching (5min)",
                    "value": format!(
                        "**Batches:** {}\n**Usernames:** {}\n**Avg Size:** {}",
                        format_number(snapshot.batches_processed),
                        format_number(snapshot.usernames_batched),
                        average_batch_size(snapshot)
                    ),
                    "inline": true
                },
                {
                    "name": "Mojang Backend (5min)",
                    "value": format!(
                        "**Requests:** {}\n**Errors:** {}\n**Sent:** {}\n**Received:** {}",
                        format_number(snapshot.mojang_requests),
                        format_number(snapshot.mojang_errors),
                        format_bytes(snapshot.bytes_sent_to_mojang),
                        format_bytes(snapshot.bytes_received_from_mojang)
                    ),
                    "inline": true
                }
            ],
            "timestamp": jiff::Timestamp::now().to_string(),
            "footer": { "text": format!("SRMojangAPI v{}", env!("CARGO_PKG_VERSION")) }
        }]
    });

    if let Err(error) = client.post(webhook.clone()).json(&payload).send().await {
        error!(%error, "failed to send Discord status report");
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.2} KB", bytes as f64 / KIB),
        _ => format!("{:.2} MB", bytes as f64 / MIB),
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    parts.push(format!("{seconds}s"));
    parts.join(" ")
}

fn format_number(number: u64) -> String {
    let digits = number.to_string();
    let mut formatted = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && digits.len().saturating_sub(index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

#[allow(clippy::cast_precision_loss)]
fn average_batch_size(snapshot: MetricsSnapshot) -> String {
    if snapshot.batches_processed == 0 {
        "N/A".to_owned()
    } else {
        format!(
            "{:.1}",
            snapshot.usernames_batched as f64 / snapshot.batches_processed as f64
        )
    }
}

fn process_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let kibibytes = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    kibibytes.checked_mul(1024)
}

fn load_average() -> Option<String> {
    let load_average = fs::read_to_string("/proc/loadavg").ok()?;
    let values = load_average.split_whitespace().take(3).collect::<Vec<_>>();
    (values.len() == 3).then(|| values.join(" / "))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{format_bytes, format_duration, format_number};

    #[test]
    fn formats_report_values() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1_536), "1.50 KB");
        assert_eq!(format_bytes(1_572_864), "1.50 MB");
        assert_eq!(format_duration(Duration::from_secs(90_061)), "1d 1h 1m 1s");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }
}
