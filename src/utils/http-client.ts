import fetch from 'node-fetch';
import {getRandomLocalAddressHost} from "./local-address-provider";
import axios from "axios";
import * as http from "node:http";
import * as https from "node:https";

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
    // Get a random local address for the outgoing connection
    const localAddress = getRandomLocalAddressHost();

    const httpAgent = new http.Agent({localAddress});
    const httpsAgent = new https.Agent({localAddress});

    return await axios.get(url, {
      method: 'GET',
      headers: {
        'Accept': 'application/json',
        'Accept-Language': 'en-US,en',
        'User-Agent': 'SRMojangAPI'
      },
      httpAgent,
      httpsAgent,
      timeout: 30_000,
      validateStatus: () => true, // Accept all status codes; never throw
    });
  }
};
