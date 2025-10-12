import { createRoute, OpenAPIHono, z } from "@hono/zod-openapi";

/**
 * Router for health check endpoints
 */
export const healthRouter = new OpenAPIHono();

// Health check endpoint
healthRouter.openapi(
  createRoute({
    method: "get",
    path: "/",
    tags: ["health"],
    description: "Health check endpoint",
    responses: {
      200: {
        description: "Successful response",
        content: {
          "application/json": {
            schema: z.object({
              status: z.string().describe("Health status of the service"),
            }),
          },
        },
      },
    },
  }),
  (c) => {
    return c.json({ status: "UP" });
  },
);
