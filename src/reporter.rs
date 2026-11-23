use std::{
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

use reqwest::Url;
use serde_json::json;
use tokio::{
    sync::oneshot,
    task::JoinHandle,
    time::{MissedTickBehavior, interval},
};
use tracing::{error, info};

use crate::{
    error::ErrorChain,
    http_client::http_client_builder,
    metrics::{Metrics, MetricsSnapshot},
};

const REPORT_INTERVAL: Duration = Duration::from_mins(5);

pub struct DiscordReporter {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl DiscordReporter {
    const fn disabled() -> Self {
        Self {
            shutdown: None,
            task: None,
        }
    }

    pub fn start(webhook: Option<Url>, metrics: Arc<Metrics>) -> Self {
        let Some(webhook) = webhook else {
            info!("DISCORD_WEBHOOK is not set; status reports are disabled");
            return Self::disabled();
        };

        let client = match http_client_builder(Duration::from_secs(15))
            .https_only(true)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                error!(error = %ErrorChain(&error), "could not create Discord HTTP client; status reports are disabled");
                return Self::disabled();
            }
        };

        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let initial_snapshot = metrics.snapshot();
        let reporter_started_at = Instant::now();
        let task = tokio::spawn(async move {
            let mut ticker = interval(REPORT_INTERVAL);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            ticker.tick().await;
            let mut last_successful_snapshot = initial_snapshot;
            let mut last_successful_report_at = reporter_started_at;

            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_receiver => break,
                    _ = ticker.tick() => {
                        let now = Instant::now();
                        let current_snapshot = metrics.snapshot();
                        let snapshot = current_snapshot.since(last_successful_snapshot);
                        let report_period = now.saturating_duration_since(last_successful_report_at);
                        let payload = match tokio::task::spawn_blocking(move || build_report(snapshot, report_period)).await {
                            Ok(payload) => payload,
                            Err(error) => {
                                error!(%error, "failed to build Discord status report");
                                continue;
                            }
                        };
                        // Shutdown must not wait for a slow Discord request.
                        let result = tokio::select! {
                            biased;
                            _ = &mut shutdown_receiver => break,
                            result = send_report(&client, &webhook, &payload) => result,
                        };
                        match result {
                            Ok(()) => {
                                last_successful_snapshot = current_snapshot;
                                last_successful_report_at = now;
                            }
                            Err(error) => {
                                // The webhook URL contains its secret token, so never log it.
                                let error = error.without_url();
                                error!(error = %ErrorChain(&error), "failed to send Discord status report");
                            }
                        }
                    },
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
            #[expect(
                clippy::let_underscore_must_use,
                reason = "The reporter may already have stopped and dropped its shutdown receiver."
            )]
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            error!(%error, "Discord status reporter stopped unexpectedly");
        }
    }
}

async fn send_report(
    client: &reqwest::Client,
    webhook: &Url,
    payload: &serde_json::Value,
) -> Result<(), reqwest::Error> {
    client
        .post(webhook.clone())
        .json(payload)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[expect(
    clippy::cast_precision_loss,
    reason = "report percentages and rates are rounded for display"
)]
fn build_report(snapshot: MetricsSnapshot, report_period: Duration) -> serde_json::Value {
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
    let requests_per_minute = if report_period.is_zero() {
        0.0
    } else {
        total_requests as f64 / report_period.as_secs_f64() * 60.0
    };
    let color = match snapshot.mojang_errors {
        0 => 0x002e_cc71,
        1..=10 => 0x00f3_9c12,
        _ => 0x00e7_4c3c,
    };
    let rss = process_rss_bytes().map_or_else(|| "Unavailable".to_owned(), format_bytes);
    let load_average = load_average().unwrap_or_else(|| "Unavailable".to_owned());
    let report_period = format_report_period(report_period);

    json!({
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
                    "name": format!("Requests ({report_period})"),
                    "value": format!(
                        "**Total:** {}\n**UUID Lookups:** {}\n**Skin Lookups:** {}\n**Req/min:** {requests_per_minute:.1}",
                        format_number(total_requests),
                        format_number(snapshot.uuid_requests),
                        format_number(snapshot.skin_requests)
                    ),
                    "inline": true
                },
                {
                    "name": format!("Cache ({report_period})"),
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
                    "name": format!("Batching ({report_period})"),
                    "value": format!(
                        "**Batches:** {}\n**Usernames:** {}\n**Avg Size:** {}",
                        format_number(snapshot.batches_processed),
                        format_number(snapshot.usernames_batched),
                        average_batch_size(snapshot)
                    ),
                    "inline": true
                },
                {
                    "name": format!("Mojang Backend ({report_period})"),
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
    })
}

#[expect(
    clippy::cast_precision_loss,
    reason = "report byte sizes are rounded to two decimal places for display"
)]
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

fn format_report_period(duration: Duration) -> String {
    let minutes = duration.as_secs().div_ceil(60);
    format!("{minutes}min")
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

#[expect(
    clippy::cast_precision_loss,
    reason = "the report rounds the average batch size to one decimal place"
)]
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
    #[expect(
        clippy::disallowed_methods,
        reason = "The reporter reads procfs on a Tokio blocking worker."
    )]
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
    #[expect(
        clippy::disallowed_methods,
        reason = "The reporter reads procfs on a Tokio blocking worker."
    )]
    let load_average = fs::read_to_string("/proc/loadavg").ok()?;
    let values = load_average.split_whitespace().take(3).collect::<Vec<_>>();
    (values.len() == 3).then(|| values.join(" / "))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{build_report, format_bytes, format_duration, format_number, format_report_period};
    use crate::metrics::MetricsSnapshot;

    fn field_value<'a>(report: &'a serde_json::Value, name: &str) -> &'a str {
        report["embeds"][0]["fields"]
            .as_array()
            .and_then(|fields| fields.iter().find(|field| field["name"] == name))
            .and_then(|field| field["value"].as_str())
            .unwrap_or_else(|| panic!("missing report field {name}"))
    }

    #[test]
    fn reports_window_counters_over_the_report_period() {
        let snapshot = MetricsSnapshot {
            uuid_requests: 1_200,
            skin_requests: 300,
            uuid_cache_hits: 3,
            uuid_cache_misses: 1,
            batches_processed: 4,
            usernames_batched: 10,
            mojang_errors: 2,
            ..MetricsSnapshot::default()
        };

        let report = build_report(snapshot, Duration::from_mins(10));

        assert_eq!(
            field_value(&report, "Requests (10min)"),
            "**Total:** 1,500\n**UUID Lookups:** 1,200\n**Skin Lookups:** 300\n**Req/min:** 150.0"
        );
        assert!(field_value(&report, "Cache (10min)").contains("**Hit Rate:** 75.0%"));
        assert!(field_value(&report, "Batching (10min)").ends_with("**Avg Size:** 2.5"));
        assert_eq!(report["embeds"][0]["color"], 0x00f3_9c12);
    }

    #[test]
    fn reports_empty_windows_without_rates() {
        let report = build_report(MetricsSnapshot::default(), Duration::from_mins(5));

        assert!(field_value(&report, "Cache (5min)").contains("**Hit Rate:** N/A"));
        assert!(field_value(&report, "Batching (5min)").ends_with("**Avg Size:** N/A"));
        assert_eq!(report["embeds"][0]["color"], 0x002e_cc71);
    }

    #[test]
    fn formats_report_values() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1_536), "1.50 KB");
        assert_eq!(format_bytes(1_572_864), "1.50 MB");
        assert_eq!(format_duration(Duration::from_secs(90_061)), "1d 1h 1m 1s");
        assert_eq!(format_report_period(Duration::from_secs(301)), "6min");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }
}
