import { Elysia } from 'elysia';

/**
 * Router for health check endpoints
 */
export const healthRouter = new Elysia({ prefix: '/health' })
  .get('', () => {
    return { status: 'UP' };
  }, {
    detail: {
      tags: ['health'],
      description: 'Check if the service is up and running',
      responses: {
        200: {
          description: 'Service is healthy',
          content: {
            'application/json': {
              schema: {
                type: 'object',
                properties: {
                  status: { type: 'string' }
                }
              }
            }
          }
        }
      }
    }
  });
