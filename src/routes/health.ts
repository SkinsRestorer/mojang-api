import {Hono} from 'hono';
import {describeRoute} from "hono-openapi";
import {resolver} from "hono-openapi/zod";
import {z} from "zod";

/**
 * Router for health check endpoints
 */
export const healthRouter = new Hono();

// Health check endpoint
healthRouter.get('/',
  describeRoute({
    tags: ['health'],
    description: 'Health check endpoint',
    responses: {
      200: {
        description: 'Successful response',
        content: {
          'application/json': {
            schema: resolver(
              z.object({
                status: z.string().describe('Health status of the service'),
              })
            )
          },
        },
      },
    },
  }),
  (c) => {
    return c.json({status: 'UP'});
  });
