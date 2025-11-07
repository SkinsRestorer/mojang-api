import { createRoute, OpenAPIHono, z } from "@hono/zod-openapi";
import { createCacheManager } from "../cache-manager";
import { batchProcessor } from "../utils/batch-processor";
import { httpClient } from "../utils/http-client";
import {
  ErrorType,
  MOJANG_API,
  type MojangProfileResponse,
} from "../utils/types";
import { convertToNoDashes, tryParseUUID } from "../utils/uuid-utils";
import { invalidMinecraftUsername } from "../utils/validation-utils";

/**
 * Router for Mojang API endpoints
 */
export const mojangApiRouter = new OpenAPIHono();

// Create cache manager
const cacheManager = createCacheManager();

const uuidResponseSchema = z.discriminatedUnion("exists", [
  z.object({
    exists: z.literal(true),
    uuid: z.string(),
  }),
  z.object({
    exists: z.literal(false),
    uuid: z.null(),
  }),
]);

const skinResponseSchema = z.discriminatedUnion("exists", [
  z.object({
    exists: z.literal(true),
    skinProperty: z.object({
      value: z.string(),
      signature: z.string(),
    }),
  }),
  z.object({
    exists: z.literal(false),
    skinProperty: z.null(),
  }),
]);

/**
 * Convert Minecraft username to UUID
 */
mojangApiRouter.openapi(
  createRoute({
    method: "get",
    path: "/uuid/{name}",
    request: {
      params: z.object({
        name: z.string().describe("Minecraft username to convert to UUID"),
      }),
    },
    tags: ["mojang"],
    description: "Convert a Minecraft username to UUID",
    responses: {
      200: {
        description: "Successful response",
        content: {
          "application/json": {
            schema: uuidResponseSchema,
          },
        },
      },
      400: {
        description: "Invalid username format",
        content: {
          "application/json": {
            schema: z.object({
              error: z.literal(ErrorType.INVALID_NAME).describe("Error type"),
            }),
          },
        },
      },
      500: {
        description: "Internal server error",
        content: {
          "application/json": {
            schema: z.object({
              error: z.literal(ErrorType.INTERNAL_ERROR).describe("Error type"),
            }),
          },
        },
      },
      503: {
        description: "Service unavailable due to timeout",
        content: {
          "application/json": {
            schema: z.object({
              error: z
                .literal(ErrorType.INTERNAL_TIMEOUT)
                .describe("Error type"),
            }),
          },
        },
      },
    },
  }),
  async (c) => {
    const { name } = c.req.valid("param");

    // Validate username
    if (invalidMinecraftUsername(name)) {
      return c.json({ error: ErrorType.INVALID_NAME } as const, 400);
    }

    try {
      // Use the batch processor to get the UUID
      const result = await batchProcessor.addRequest(name);

      return c.json(result, 200, MOJANG_API.CACHE_HEADERS);
    } catch (error: unknown) {
      console.error(`Error fetching UUID for name ${name}:`, error);

      // Check if it's a timeout error
      if (error instanceof Error && error.message === "Request timeout") {
        return c.json({ error: ErrorType.INTERNAL_TIMEOUT } as const, 503);
      }

      return c.json({ error: ErrorType.INTERNAL_ERROR } as const, 503);
    }
  },
);

/**
 * Get skin data for a Minecraft UUID
 */
mojangApiRouter.openapi(
  createRoute({
    method: "get",
    path: "/skin/{uuid}",
    request: {
      params: z.object({
        uuid: z.string().describe("Minecraft UUID to get skin data for"),
      }),
    },
    tags: ["mojang"],
    description: "Get skin data for a Minecraft UUID",
    responses: {
      200: {
        description: "Successful response",
        content: {
          "application/json": {
            schema: skinResponseSchema,
          },
        },
      },
      400: {
        description: "Invalid UUID format",
        content: {
          "application/json": {
            schema: z.object({
              error: z.literal(ErrorType.INVALID_UUID).describe("Error type"),
            }),
          },
        },
      },
      500: {
        description: "Internal server error",
        content: {
          "application/json": {
            schema: z.object({
              error: z.literal(ErrorType.INTERNAL_ERROR).describe("Error type"),
            }),
          },
        },
      },
      503: {
        description: "Service unavailable due to timeout",
        content: {
          "application/json": {
            schema: z.object({
              error: z
                .literal(ErrorType.INTERNAL_TIMEOUT)
                .describe("Error type"),
            }),
          },
        },
      },
    },
  }),
  async (c) => {
    const { uuid } = c.req.valid("param");
    const uuidParsed = tryParseUUID(uuid);

    if (!uuidParsed) {
      return c.json({ error: ErrorType.INVALID_UUID } as const, 400);
    }

    try {
      // Check cache first
      const cachedData = await cacheManager.getUUIDToSkin(uuidParsed);
      if (cachedData) {
        if (cachedData.value === null) {
          return c.json(
            {
              exists: false,
              skinProperty: null,
            } as const,
            200,
            MOJANG_API.CACHE_HEADERS,
          );
        } else {
          return c.json(
            {
              exists: true,
              skinProperty: cachedData.value,
            } as const,
            200,
            MOJANG_API.CACHE_HEADERS,
          );
        }
      }

      // If not in cache, call Mojang API
      const mojangUrl = MOJANG_API.PROFILE_URL.replace(
        "%s",
        convertToNoDashes(uuidParsed),
      );
      const response = await httpClient.get(mojangUrl);

      // Handle 204 No Content (profile doesn't exist)
      if (response.status === 204) {
        cacheManager.putUUIDToSkin(uuidParsed, null, new Date());

        return c.json(
          { exists: false, skinProperty: null } as const,
          200,
          MOJANG_API.CACHE_HEADERS,
        );
      }

      // Handle other non-success responses
      if (response.status < 200 || response.status >= 300) {
        console.error(
          `Error fetching skin for UUID ${uuidParsed}:`,
          response.status,
          response.statusText,
        );
        return c.json({ error: ErrorType.INTERNAL_ERROR } as const, 500);
      }

      const responseData = (await response.data) as MojangProfileResponse;

      // Find the textures property
      const property =
        responseData.properties?.find((p) => p.name === "textures") || null;

      // Cache the result
      cacheManager.putUUIDToSkin(
        uuidParsed,
        property
          ? {
              value: property.value,
              signature: property.signature,
            }
          : null,
        new Date(),
      );

      if (property === null) {
        return c.json(
          {
            exists: false,
            skinProperty: null,
          } as const,
          200,
          MOJANG_API.CACHE_HEADERS,
        );
      } else {
        // Return the skin property if it exists
        return c.json(
          {
            exists: true,
            skinProperty: {
              value: property.value,
              signature: property.signature,
            },
          } as const,
          200,
          MOJANG_API.CACHE_HEADERS,
        );
      }
    } catch (error: unknown) {
      console.error(`Error fetching skin for UUID ${uuidParsed}:`, error);

      // Check if it's a timeout error
      if (error instanceof Error && error.name === "AbortError") {
        return c.json({ error: ErrorType.INTERNAL_TIMEOUT } as const, 503);
      }

      return c.json({ error: ErrorType.INTERNAL_ERROR } as const, 503);
    }
  },
);
