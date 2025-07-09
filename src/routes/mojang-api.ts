import {httpClient} from '../utils/http-client';
import {invalidMinecraftUsername} from '../utils/validation-utils';
import {convertToNoDashes, tryParseUUID} from '../utils/uuid-utils';
import {ErrorType, MOJANG_API, MojangProfileResponse, MojangUUIDResponse,} from '../utils/types';
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
    path: '/uuid/{name}',
    request: {
      params: z.object({
        name: z.string()
          .describe('Minecraft username to convert to UUID')
          .openapi({
            param: {
              name: 'name',
              in: 'path',
            },
            example: 'Pistonmaster',
          }),
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
              exists: z.literal(true),
              uuid: z.string()
            }).or(z.object({
              exists: z.literal(false),
              uuid: z.null()
            })),
          },
        },
      },
      400: {
        description: 'Invalid username format',
        content: {
          'application/json': {
            schema: z.object({
              error: z.literal(ErrorType.INVALID_NAME).describe('Error type')
            })
          },
        },
      },
      500: {
        description: 'Internal server error',
        content: {
          'application/json': {
            schema: z.object({
              error: z.literal(ErrorType.INTERNAL_ERROR).describe('Error type')
            })
          },
        },
      },
      503: {
        description: 'Service unavailable due to timeout',
        content: {
          'application/json': {
            schema: z.object({
              error: z.literal(ErrorType.INTERNAL_TIMEOUT).describe('Error type')
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
      return c.json({error: ErrorType.INVALID_NAME} as const, 400);
    }

    try {
      // Check cache first
      const cachedData = await cacheManager.getNameToUUID(name);
      if (cachedData) {
        if (cachedData.value === null) {
          return c.json({
            exists: false,
            uuid: null
          } as const, 200, MOJANG_API.CACHE_HEADERS);
        } else {
          return c.json({
            exists: true,
            uuid: cachedData.value
          } as const, 200, MOJANG_API.CACHE_HEADERS);
        }
      }

      // If not in cache, call Mojang API
      const mojangUrl = MOJANG_API.UUID_URL.replace('%s', name);
      const response = await httpClient.get(mojangUrl);

      const isNotFound = response.status === 404;
      const isSuccess = response.status >= 200 && response.status < 300;
      if (!isNotFound && !isSuccess) {
        return c.json({error: ErrorType.INTERNAL_ERROR} as const, 500);
      }

      const responseData = response.data as MojangUUIDResponse;
      const uuid = isNotFound || !responseData.id ? null : tryParseUUID(responseData.id);

      // Cache the result
      cacheManager.putNameToUUID(name, uuid, new Date());

      if (uuid === null) {
        return c.json({
          exists: false,
          uuid: null
        } as const, 200, MOJANG_API.CACHE_HEADERS);
      } else {
        return c.json({
          exists: true,
          uuid
        } as const, 200, MOJANG_API.CACHE_HEADERS);
      }
    } catch (error: unknown) {
      console.error(`Error fetching UUID for name ${name}:`, error);

      // Check if it's a timeout error
      if (error instanceof Error && error.name === 'AbortError') {
        return c.json({error: ErrorType.INTERNAL_TIMEOUT} as const, 503);
      }

      return c.json({error: ErrorType.INTERNAL_ERROR} as const, 500);
    }
  });

/**
 * Get skin data for a Minecraft UUID
 */
mojangApiRouter.openapi(
  createRoute({
    method: 'get',
    path: '/skin/{uuid}',
    request: {
      params: z.object({
        uuid: z.string()
          .describe('Minecraft UUID to get skin data for')
          .openapi({
            param: {
              name: 'uuid',
              in: 'path',
            },
            example: 'b1ae0778-4817-436c-96a3-a72c67cda060',
          }),
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
              exists: z.literal(true),
              skinProperty: z.object({
                value: z.string(),
                signature: z.string()
              })
            }).or(z.object({
              exists: z.literal(false),
              skinProperty: z.null()
            }))
          },
        },
      },
      400: {
        description: 'Invalid UUID format',
        content: {
          'application/json': {
            schema: z.object({
              error: z.literal(ErrorType.INVALID_UUID).describe('Error type')
            })
          },
        },
      },
      500: {
        description: 'Internal server error',
        content: {
          'application/json': {
            schema: z.object({
              error: z.literal(ErrorType.INTERNAL_ERROR).describe('Error type')
            })
          },
        },
      },
      503: {
        description: 'Service unavailable due to timeout',
        content: {
          'application/json': {
            schema: z.object({
              error: z.literal(ErrorType.INTERNAL_TIMEOUT).describe('Error type')
            })
          },
        },
      }
    },
  }),
  async (c) => {
    const {uuid} = c.req.valid('param');
    const uuidParsed = tryParseUUID(uuid);

    if (!uuidParsed) {
      return c.json({error: ErrorType.INVALID_UUID} as const, 400);
    }

    try {
      // Check cache first
      const cachedData = await cacheManager.getUUIDToSkin(uuidParsed);
      if (cachedData) {
        if (cachedData.value === null) {
          return c.json({
            exists: false,
            skinProperty: null
          } as const, 200, MOJANG_API.CACHE_HEADERS);
        } else {
          return c.json({
            exists: true,
            skinProperty: cachedData.value
          } as const, 200, MOJANG_API.CACHE_HEADERS);
        }
      }

      // If not in cache, call Mojang API
      const mojangUrl = MOJANG_API.PROFILE_URL.replace('%s', convertToNoDashes(uuidParsed));
      const response = await httpClient.get(mojangUrl);

      // Handle 204 No Content (profile doesn't exist)
      if (response.status === 204) {
        cacheManager.putUUIDToSkin(uuidParsed, null, new Date());

        return c.json({exists: false, skinProperty: null} as const, 200, MOJANG_API.CACHE_HEADERS);
      }

      // Handle other non-success responses
      if (response.status < 200 || response.status >= 300) {
        return c.json({error: ErrorType.INTERNAL_ERROR} as const, 500);
      }

      const responseData = await response.data as MojangProfileResponse;

      // Find the textures property
      const property = responseData.properties?.find(p => p.name === 'textures') || null;

      // Cache the result
      cacheManager.putUUIDToSkin(uuidParsed, property ? {
        value: property.value,
        signature: property.signature
      } : null, new Date());

      if (property === null) {
        return c.json({
          exists: false,
          skinProperty: null
        } as const, 200, MOJANG_API.CACHE_HEADERS);
      } else {
        // Return the skin property if it exists
        return c.json({
          exists: true,
          skinProperty: {
            value: property.value,
            signature: property.signature
          }
        } as const, 200, MOJANG_API.CACHE_HEADERS);
      }
    } catch (error: unknown) {
      console.error(`Error fetching skin for UUID ${uuidParsed}:`, error);

      // Check if it's a timeout error
      if (error instanceof Error && error.name === 'AbortError') {
        return c.json({error: ErrorType.INTERNAL_TIMEOUT} as const, 503);
      }

      return c.json({error: ErrorType.INTERNAL_ERROR} as const, 500);
    }
  });
