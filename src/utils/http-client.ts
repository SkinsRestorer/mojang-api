import fetch from 'node-fetch';
import {getRandomLocalAddressHost} from "./local-address-provider";

/**
 * Maximum request timeout in milliseconds
 */
const REQUEST_TIMEOUT_MS = 5000;

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
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);

    try {
      // Get a random local address for the outgoing connection
      const localAddress = getRandomLocalAddressHost();

      return await fetch(url, {
        method: 'GET',
        headers: {
          'Accept': 'application/json',
          'Accept-Language': 'en-US,en',
          'User-Agent': 'SRMojangAPI'
        },
        compress: true,
        signal: controller.signal,
        //localAddress: localAddress.address
      });
    } finally {
      clearTimeout(timeoutId);
    }
  }
};
