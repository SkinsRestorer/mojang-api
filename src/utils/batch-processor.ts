import { createCacheManager } from "../cache-manager";
import { httpClient } from "./http-client";
import { MOJANG_API, type MojangBatchUUIDResponse } from "./types";
import { tryParseUUID } from "./uuid-utils";

interface PendingRequest {
  name: string;
  resolve: (
    result:
      | { exists: true; uuid: string }
      | {
          exists: false;
          uuid: null;
        },
  ) => void;
  reject: (error: Error) => void;
}

/**
 * Batch processor for Mojang username to UUID requests
 * Collects requests and processes them in batches every 3 seconds
 */
export class BatchProcessor {
  private pendingRequests: PendingRequest[] = [];
  private processingInterval: NodeJS.Timeout | null = null;
  private cacheManager = createCacheManager();

  constructor() {
    // Start the batch processing interval
    this.processingInterval = setInterval(() => {
      this.processBatch();
    }, MOJANG_API.BATCH_INTERVAL_MS);
  }

  /**
   * Add a username request to the batch queue
   * @param name The username to lookup
   * @returns Promise that resolves with the UUID result
   */
  async addRequest(name: string): Promise<
    | { exists: true; uuid: string }
    | {
        exists: false;
        uuid: null;
      }
  > {
    // First check cache
    const cachedData = await this.cacheManager.getNameToUUID(name);
    if (cachedData) {
      if (cachedData.value !== null) {
        return {
          exists: true,
          uuid: cachedData.value,
        };
      } else {
        return {
          exists: false,
          uuid: null,
        };
      }
    }

    // If not in cache, add to batch queue
    return new Promise<
      | { exists: true; uuid: string }
      | {
          exists: false;
          uuid: null;
        }
    >((resolve, reject) => {
      this.pendingRequests.push({
        name,
        resolve,
        reject,
      });

      // If we have enough requests for a full batch, process immediately
      if (this.pendingRequests.length >= MOJANG_API.BATCH_SIZE) {
        this.processBatch();
      }
    });
  }

  /**
   * Shutdown the batch processor
   */
  shutdown(): void {
    if (this.processingInterval) {
      clearInterval(this.processingInterval);
      this.processingInterval = null;
    }

    // Process any remaining requests
    if (this.pendingRequests.length > 0) {
      this.processBatch();
    }

    this.cacheManager.close();
  }

  /**
   * Process the current batch of pending requests
   */
  private async processBatch(): Promise<void> {
    if (this.pendingRequests.length === 0) {
      return;
    }

    // Take up to BATCH_SIZE requests from the queue
    const requestsToProcess = this.pendingRequests.splice(
      0,
      MOJANG_API.BATCH_SIZE,
    );
    const usernames = requestsToProcess.map((req) => req.name);

    try {
      console.log(
        `Processing batch of ${usernames.length} usernames: ${usernames.join(", ")}`,
      );

      const batchUuidUrl =
        MOJANG_API.BATCH_UUID_URLS[
          Math.floor(Math.random() * MOJANG_API.BATCH_UUID_URLS.length)
        ];

      // Make the batch request to Mojang API
      const response = await httpClient.post(batchUuidUrl, usernames);

      if (response.status === 400) {
        // Handle validation errors
        const errorData = response.data;
        console.error("Batch request validation error:", errorData);

        // Reject all requests with validation error
        requestsToProcess.forEach((req) => {
          req.reject(new Error("Validation error in batch request"));
        });
        return;
      }

      if (response.status < 200 || response.status >= 300) {
        console.error(
          "Batch request failed:",
          response.status,
          response.statusText,
        );

        // Reject all requests with server error
        requestsToProcess.forEach((req) => {
          req.reject(new Error(`Server error: ${response.status}`));
        });
        return;
      }

      const batchResults: MojangBatchUUIDResponse[] = response.data || [];
      const currentTime = new Date();

      // Create a map of results by username (case-insensitive)
      const resultMap = new Map<string, MojangBatchUUIDResponse>();
      batchResults.forEach((result) => {
        resultMap.set(result.name.toLowerCase(), result);
      });

      // Process each request
      requestsToProcess.forEach((req) => {
        const result = resultMap.get(req.name.toLowerCase());

        if (result) {
          // Player exists
          const uuid = tryParseUUID(result.id);
          if (!uuid) {
            // Invalid UUID format from Mojang
            req.reject(new Error("Received invalid UUID format from Mojang"));
            return;
          }

          // Cache the result
          this.cacheManager.putNameToUUID(req.name, uuid, currentTime);

          req.resolve({
            exists: true,
            uuid,
          });
        } else {
          // Player doesn't exist
          // Cache the negative result
          this.cacheManager.putNameToUUID(req.name, null, currentTime);

          req.resolve({
            exists: false,
            uuid: null,
          });
        }
      });
    } catch (error: unknown) {
      console.error("Error processing batch:", error);

      // Reject all requests with the error
      requestsToProcess.forEach((req) => {
        if (error instanceof Error && error.name === "AbortError") {
          req.reject(new Error("Request timeout"));
        } else {
          req.reject(new Error("Internal server error"));
        }
      });
    }
  }
}

// Create a singleton instance
export const batchProcessor = new BatchProcessor();
