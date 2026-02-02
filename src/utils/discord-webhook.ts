import * as os from "node:os";
import axios from "axios";
import { metrics } from "./metrics";

const REPORT_INTERVAL_MS = 5 * 60 * 1000;

let reportInterval: NodeJS.Timeout | null = null;

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(2)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

function formatUptime(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs = seconds % 60;

  const parts: string[] = [];
  if (days > 0) parts.push(`${days}d`);
  if (hours > 0) parts.push(`${hours}h`);
  if (minutes > 0) parts.push(`${minutes}m`);
  parts.push(`${secs}s`);
  return parts.join(" ");
}

function formatNumber(n: number): string {
  return n.toLocaleString("en-US");
}

async function sendReport(): Promise<void> {
  const webhookUrl = process.env.DISCORD_WEBHOOK;
  if (!webhookUrl) return;

  const uptime = Date.now() - metrics.startedAt;
  const reportPeriodMs = Date.now() - metrics.lastReportAt;

  const totalRequests = metrics.uuidRequests + metrics.skinRequests;
  const totalCacheHits = metrics.uuidCacheHits + metrics.skinCacheHits;
  const totalCacheMisses = metrics.uuidCacheMisses + metrics.skinCacheMisses;
  const totalCacheLookups = totalCacheHits + totalCacheMisses;
  const cacheHitRate =
    totalCacheLookups > 0
      ? ((totalCacheHits / totalCacheLookups) * 100).toFixed(1)
      : "N/A";

  const memUsage = process.memoryUsage();
  const loadAvg = os.loadavg();

  const color =
    metrics.mojangErrors === 0
      ? 0x2ecc71
      : metrics.mojangErrors > 10
        ? 0xe74c3c
        : 0xf39c12;

  const embed = {
    title: "Mojang API Proxy \u2014 Status Report",
    color,
    fields: [
      {
        name: "\ud83d\udda5\ufe0f Server",
        value: [
          `**Uptime:** ${formatUptime(uptime)}`,
          `**Memory:** ${formatBytes(memUsage.heapUsed)} / ${formatBytes(memUsage.heapTotal)}`,
          `**RSS:** ${formatBytes(memUsage.rss)}`,
          `**Load Avg:** ${loadAvg.map((l) => l.toFixed(2)).join(" / ")}`,
        ].join("\n"),
        inline: false,
      },
      {
        name: "\ud83d\udcca Requests (5min)",
        value: [
          `**Total:** ${formatNumber(totalRequests)}`,
          `**UUID Lookups:** ${formatNumber(metrics.uuidRequests)}`,
          `**Skin Lookups:** ${formatNumber(metrics.skinRequests)}`,
          `**Req/min:** ${(totalRequests / (reportPeriodMs / 60000)).toFixed(1)}`,
        ].join("\n"),
        inline: true,
      },
      {
        name: "\ud83d\udcbe Cache (5min)",
        value: [
          `**Hits:** ${formatNumber(totalCacheHits)}`,
          `**Misses:** ${formatNumber(totalCacheMisses)}`,
          `**Hit Rate:** ${cacheHitRate}%`,
          `**UUID:** ${formatNumber(metrics.uuidCacheHits)} hit / ${formatNumber(metrics.uuidCacheMisses)} miss`,
          `**Skin:** ${formatNumber(metrics.skinCacheHits)} hit / ${formatNumber(metrics.skinCacheMisses)} miss`,
        ].join("\n"),
        inline: true,
      },
      {
        name: "\ud83d\udce6 Batching (5min)",
        value: [
          `**Batches:** ${formatNumber(metrics.batchesProcessed)}`,
          `**Usernames:** ${formatNumber(metrics.usernamesBatched)}`,
          `**Avg Size:** ${metrics.batchesProcessed > 0 ? (metrics.usernamesBatched / metrics.batchesProcessed).toFixed(1) : "N/A"}`,
        ].join("\n"),
        inline: true,
      },
      {
        name: "\ud83c\udf10 Mojang Backend (5min)",
        value: [
          `**Requests:** ${formatNumber(metrics.mojangRequests)}`,
          `**Errors:** ${formatNumber(metrics.mojangErrors)}`,
          `**Sent:** ${formatBytes(metrics.bytesSentToMojang)}`,
          `**Received:** ${formatBytes(metrics.bytesReceivedFromMojang)}`,
        ].join("\n"),
        inline: true,
      },
    ],
    timestamp: new Date().toISOString(),
    footer: {
      text: "SRMojangAPI v2.0.0",
    },
  };

  try {
    await axios.post(webhookUrl, { embeds: [embed] });
  } catch (error) {
    console.error("Failed to send Discord webhook report:", error);
  }

  metrics.reset();
}

export function startDiscordReporter(): void {
  if (!process.env.DISCORD_WEBHOOK) {
    console.log("DISCORD_WEBHOOK not set, skipping Discord reporter");
    return;
  }

  console.log("Discord webhook reporter started (every 5 minutes)");
  reportInterval = setInterval(sendReport, REPORT_INTERVAL_MS);
}

export function stopDiscordReporter(): void {
  if (reportInterval) {
    clearInterval(reportInterval);
    reportInterval = null;
  }
}
