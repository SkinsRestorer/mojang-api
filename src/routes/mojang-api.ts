import { Elysia, t } from 'elysia';
import { httpClient } from '../utils/http-client';
import { invalidMinecraftUsername } from '../utils/validation-utils';
import { tryParseUUID, convertToNoDashes } from '../utils/uuid-utils';
import {
  ErrorType,
  MOJANG_API,
  MojangProfileResponse,
  MojangUUIDResponse,
  ProfileResponse,
  UUIDResponse
} from '../utils/types';
import {createCacheManager} from '../cache-manager';

/**
 * Router for Mojang API endpoints
 */
export const mojangApiRouter = new Elysia({ prefix: '/mojang' })
  // Setup context with cache manager
  .decorate("cacheManager", createCacheManager())
  /**
   * Convert Minecraft username to UUID
   */
  .get('/uuid/:name', async ({ params, set, cacheManager }) => {
    const { name } = params;

    // Validate username
    if (invalidMinecraftUsername(name)) {
      set.status = 400;
      return { error: ErrorType.INVALID_NAME };
    }

    try {
      // Check cache first
      const cachedData = await cacheManager.getNameToUUID(name);
      if (cachedData) {
        set.headers = MOJANG_API.CACHE_HEADERS;
        return { exists: cachedData.value !== null, uuid: cachedData.value } satisfies UUIDResponse;
      }

      // If not in cache, call Mojang API
      const mojangUrl = MOJANG_API.UUID_URL.replace('%s', name);
      const response = await httpClient.get(mojangUrl);

      const isNotFound = response.status === 404;
      const isSuccess = response.status >= 200 && response.status < 300;
      if (!isNotFound && !isSuccess) {
        set.status = 500;
        return { error: ErrorType.INTERNAL_ERROR };
      }

      const responseData = response.data as MojangUUIDResponse;
      const uuid = isNotFound || !responseData.id ? null : tryParseUUID(responseData.id);

      // Cache the result
      cacheManager.putNameToUUID(name, uuid, new Date());

      // Return response
      set.headers = MOJANG_API.CACHE_HEADERS;
      return { exists: uuid !== null, uuid } satisfies UUIDResponse;
    } catch (error: unknown) {
      console.error(`Error fetching UUID for name ${name}:`, error);

      // Check if it's a timeout error
      if (error instanceof Error && error.name === 'AbortError') {
        set.status = 503;
        return { error: ErrorType.INTERNAL_TIMEOUT };
      }

      set.status = 500;
      return { error: ErrorType.INTERNAL_ERROR };
    }
  }, {
    params: t.Object({
      name: t.String()
    }),
    detail: {
      tags: ['mojang'],
      summary: 'Convert a Minecraft username to UUID',
      description: 'Returns the UUID for a given Minecraft username',
      responses: {
        200: {
          description: 'UUID found or not found',
        },
        400: {
          description: 'Invalid username format'
        },
        500: {
          description: 'Internal server error'
        },
        503: {
          description: 'Request timed out'
        }
      }
    }
  })

  /**
   * Get skin data for a Minecraft UUID
   */
  .get('/skin/:uuid', async ({ params, set, cacheManager }) => {
    const uuidParam = params.uuid;
    const uuid = tryParseUUID(uuidParam);

    if (!uuid) {
      set.status = 400;
      return { error: ErrorType.INVALID_UUID };
    }

    try {
      // Check cache first
      const cachedData = await cacheManager.getUUIDToSkin(uuid);
      if (cachedData) {
        set.headers = MOJANG_API.CACHE_HEADERS;
        return {
          exists: cachedData.value !== null,
          skinProperty: cachedData.value
        } satisfies ProfileResponse;
      }

      // If not in cache, call Mojang API
      const mojangUrl = MOJANG_API.PROFILE_URL.replace('%s', convertToNoDashes(uuid));
      const response = await httpClient.get(mojangUrl);

      // Handle 204 No Content (profile doesn't exist)
      if (response.status === 204) {
        cacheManager.putUUIDToSkin(uuid, null, new Date());
        set.headers = MOJANG_API.CACHE_HEADERS;
        return { exists: false, skinProperty: null };
      }

      // Handle other non-success responses
      if (response.status < 200 || response.status >= 300) {
        set.status = 500;
        return { error: ErrorType.INTERNAL_ERROR };
      }

      const responseData = await response.data as MojangProfileResponse;

      // Find the textures property
      const property = responseData.properties?.find(p => p.name === 'textures') || null;

      // Cache the result
      cacheManager.putUUIDToSkin(uuid, property ? {
        value: property.value,
        signature: property.signature
      } : null, new Date());

      // Return response
      set.headers = MOJANG_API.CACHE_HEADERS;
      return {
        exists: property !== null,
        skinProperty: property ? {
          value: property.value,
          signature: property.signature
        } : null
      } satisfies ProfileResponse;
    } catch (error: unknown) {
      console.error(`Error fetching skin for UUID ${uuid}:`, error);

      // Check if it's a timeout error
      if (error instanceof Error && error.name === 'AbortError') {
        set.status = 503;
        return { error: ErrorType.INTERNAL_TIMEOUT };
      }

      set.status = 500;
      return { error: ErrorType.INTERNAL_ERROR };
    }
  }, {
    params: t.Object({
      uuid: t.String()
    }),
    detail: {
      tags: ['mojang'],
      summary: 'Get skin data for a Minecraft UUID',
      description: 'Returns skin property data for a given Minecraft UUID',
      responses: {
        200: {
          description: 'Skin data found or not found',
        },
        400: {
          description: 'Invalid UUID format'
        },
        500: {
          description: 'Internal server error'
        },
        503: {
          description: 'Request timed out'
        }
      }
    }
  });
