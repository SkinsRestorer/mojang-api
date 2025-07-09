import {httpClient} from '../utils/http-client';
import {invalidMinecraftUsername} from '../utils/validation-utils';
import {tryParseUUID, convertToNoDashes} from '../utils/uuid-utils';
import {
  ErrorType,
  MOJANG_API,
  MojangProfileResponse,
  MojangUUIDResponse,
  ProfileResponse,
  UUIDResponse
} from '../utils/types';
import {createCacheManager} from '../cache-manager';
import {createRoute, OpenAPIHono, z} from "@hono/zod-openapi";

/**
 * Router for Mojang API endpoints
 */
export const mojangApiRouter = new OpenAPIHono();

// Create cache manager
const cacheManager = createCacheManager();

/**
 * Convert Minecraft username to UUID
 */
mojangApiRouter.openapi(
  createRoute({
    method: 'get',
    path: '/uuid/:name',
    request: {
      params: z.object({
        name: z.string().describe('Minecraft username to convert to UUID')
      }),
    },
    tags: ['mojang'],
    description: 'Convert a Minecraft username to UUID',
    responses: {
      200: {
        description: 'Successful response',
        content: {
          'application/json': {
            schema: z.object({
              exists: z.boolean(),
              uuid: z.string().nullable()
            })
          },
        },
      },
    },
  }),
  async (c) => {
    const {name} = c.req.valid('param');

    // Validate username
    if (invalidMinecraftUsername(name)) {
      return c.json({error: ErrorType.INVALID_NAME}, 400);
    }

    try {
      // Check cache first
      const cachedData = await cacheManager.getNameToUUID(name);
      if (cachedData) {
        // Set cache headers
        Object.entries(MOJANG_API.CACHE_HEADERS).forEach(([key, value]) => {
          c.header(key, value);
        });

        return c.json({
          exists: cachedData.value !== null,
          uuid: cachedData.value
        } satisfies UUIDResponse);
      }

      // If not in cache, call Mojang API
      const mojangUrl = MOJANG_API.UUID_URL.replace('%s', name);
      const response = await httpClient.get(mojangUrl);

      const isNotFound = response.status === 404;
      const isSuccess = response.status >= 200 && response.status < 300;
      if (!isNotFound && !isSuccess) {
        return c.json({error: ErrorType.INTERNAL_ERROR}, 500);
      }

      const responseData = response.data as MojangUUIDResponse;
      const uuid = isNotFound || !responseData.id ? null : tryParseUUID(responseData.id);

      // Cache the result
      cacheManager.putNameToUUID(name, uuid, new Date());

      // Set cache headers
      Object.entries(MOJANG_API.CACHE_HEADERS).forEach(([key, value]) => {
        c.header(key, value);
      });

      return c.json({
        exists: uuid !== null,
        uuid
      } satisfies UUIDResponse);
    } catch (error: unknown) {
      console.error(`Error fetching UUID for name ${name}:`, error);

      // Check if it's a timeout error
      if (error instanceof Error && error.name === 'AbortError') {
        return c.json({error: ErrorType.INTERNAL_TIMEOUT}, 503);
      }

      return c.json({error: ErrorType.INTERNAL_ERROR}, 500);
    }
  });

/**
 * Get skin data for a Minecraft UUID
 */
mojangApiRouter.openapi(
  createRoute({
    method: 'get',
    path: '/skin/:uuid',
    request: {
      params: z.object({
        uuid: z.string().describe('Minecraft UUID to get skin data for')
      }),
    },
    tags: ['mojang'],
    description: 'Get skin data for a Minecraft UUID',
    responses: {
      200: {
        description: 'Successful response',
        content: {
          'application/json': {
            schema: z.object({
              exists: z.boolean(),
              skinProperty: z.object({
                value: z.string(),
                signature: z.string()
              }).nullable()
            })
          },
        },
      },
    },
  }),
  async (c) => {
    const {uuid} = c.req.valid('param');
    const uuidParsed = tryParseUUID(uuid);

    if (!uuidParsed) {
      return c.json({error: ErrorType.INVALID_UUID}, 400);
    }

    try {
      // Check cache first
      const cachedData = await cacheManager.getUUIDToSkin(uuidParsed);
      if (cachedData) {
        // Set cache headers
        Object.entries(MOJANG_API.CACHE_HEADERS).forEach(([key, value]) => {
          c.header(key, value);
        });

        return c.json({
          exists: cachedData.value !== null,
          skinProperty: cachedData.value
        } satisfies ProfileResponse);
      }

      // If not in cache, call Mojang API
      const mojangUrl = MOJANG_API.PROFILE_URL.replace('%s', convertToNoDashes(uuidParsed));
      const response = await httpClient.get(mojangUrl);

      // Handle 204 No Content (profile doesn't exist)
      if (response.status === 204) {
        cacheManager.putUUIDToSkin(uuidParsed, null, new Date());

        // Set cache headers
        Object.entries(MOJANG_API.CACHE_HEADERS).forEach(([key, value]) => {
          c.header(key, value);
        });

        return c.json({exists: false, skinProperty: null});
      }

      // Handle other non-success responses
      if (response.status < 200 || response.status >= 300) {
        return c.json({error: ErrorType.INTERNAL_ERROR}, 500);
      }

      const responseData = await response.data as MojangProfileResponse;

      // Find the textures property
      const property = responseData.properties?.find(p => p.name === 'textures') || null;

      // Cache the result
      cacheManager.putUUIDToSkin(uuidParsed, property ? {
        value: property.value,
        signature: property.signature
      } : null, new Date());

      // Set cache headers
      Object.entries(MOJANG_API.CACHE_HEADERS).forEach(([key, value]) => {
        c.header(key, value);
      });

      return c.json({
        exists: property !== null,
        skinProperty: property ? {
          value: property.value,
          signature: property.signature
        } : null
      } satisfies ProfileResponse);
    } catch (error: unknown) {
      console.error(`Error fetching skin for UUID ${uuidParsed}:`, error);

      // Check if it's a timeout error
      if (error instanceof Error && error.name === 'AbortError') {
        return c.json({error: ErrorType.INTERNAL_TIMEOUT}, 503);
      }

      return c.json({error: ErrorType.INTERNAL_ERROR}, 500);
    }
  });
