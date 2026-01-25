import {existsSync, readFileSync} from "node:fs";
import axios from "axios";
import {HttpsProxyAgent} from "https-proxy-agent";

/**
 * Maximum request timeout in milliseconds
 */
const REQUEST_TIMEOUT_MS = 15_000;

const proxyUrls: string[] = [];

/**
 * Loads proxy list from file specified in PROXY_LIST_FILE env var.
 * File format: ip:port:user:pass (one per line, user:pass optional)
 */
function loadProxyList(): void {
  const proxyListFile = process.env.PROXY_LIST_FILE;
  if (!proxyListFile) return;

  if (!existsSync(proxyListFile)) {
    console.warn(`Proxy list file not found: ${proxyListFile}`);
    return;
  }

  const content = readFileSync(proxyListFile, "utf-8");
  const lines = content.split("\n").filter((line) => line.trim());

  for (const line of lines) {
    const parts = line.trim().split(":");
    if (parts.length >= 2) {
      const host = parts[0];
      const port = parts[1];
      if (parts.length >= 4) {
        const username = parts[2];
        const password = parts.slice(3).join(":");
        proxyUrls.push(`http://${username}:${password}@${host}:${port}`);
      } else {
        proxyUrls.push(`http://${host}:${port}`);
      }
    }
  }

  console.log(`Loaded ${proxyUrls.length} proxies from ${proxyListFile}`);
}

/**
 * Returns an HttpsProxyAgent for a random proxy, or undefined if no proxies available
 */
function getRandomProxyAgent(): HttpsProxyAgent<string> | undefined {
  if (proxyUrls.length === 0) return undefined;
  const url = proxyUrls[Math.floor(Math.random() * proxyUrls.length)];
  return new HttpsProxyAgent(url);
}

// Load proxies on module initialization
loadProxyList();

/**
 * HTTP client for making requests to external APIs
 */
export const httpClient = {
  /**
   * Makes a GET request to the specified URL
   * @param url The URL to request
   * @returns A promise that resolves to the response object
   */
  async get(url: string) {
    return await axios.get(url, {
      method: "GET",
      headers: {
        Accept: "application/json",
        "Accept-Language": "en-US,en",
        "User-Agent": "SRMojangAPI",
      },
      httpsAgent: getRandomProxyAgent(),
      proxy: false,
      timeout: REQUEST_TIMEOUT_MS,
      validateStatus: () => true, // Accept all status codes; never throw
    });
  },

  /**
   * Makes a POST request to the specified URL
   * @param url The URL to request
   * @param data The data to send in the request body
   * @returns A promise that resolves to the response object
   */
  async post(url: string, data: unknown) {
    return await axios.post(url, data, {
      headers: {
        Accept: "application/json",
        "Accept-Language": "en-US,en",
        "Content-Type": "application/json",
        "User-Agent": "SRMojangAPI",
      },
      httpsAgent: getRandomProxyAgent(),
      proxy: false,
      timeout: REQUEST_TIMEOUT_MS,
      validateStatus: () => true, // Accept all status codes; never throw
    });
  },
};
