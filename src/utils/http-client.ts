import axios from "axios";

/**
 * Maximum request timeout in milliseconds
 */
const REQUEST_TIMEOUT_MS = 15_000;

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
      proxy: process.env.HTTP_PROXY
        ? JSON.parse(process.env.HTTP_PROXY)
        : false,
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
      proxy: process.env.HTTP_PROXY
        ? JSON.parse(process.env.HTTP_PROXY)
        : false,
      timeout: REQUEST_TIMEOUT_MS,
      validateStatus: () => true, // Accept all status codes; never throw
    });
  },
};
