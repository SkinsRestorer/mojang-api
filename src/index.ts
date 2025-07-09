import 'dotenv-flow/config'

import { Hono } from "hono";
import { cors } from "hono/cors";
import { swaggerUI } from "@hono/swagger-ui";
import { serve } from "@hono/node-server";
import { mojangApiRouter } from "./routes/mojang-api";
import { healthRouter } from "./routes/health";
import { rateLimiter } from "hono-rate-limiter";
import { logger } from "hono/logger";
import { openAPISpecs } from "hono-openapi";

// Get server port from environment variables or use default
const PORT = process.env.SERVER_PORT ? parseInt(process.env.SERVER_PORT) : 3000;

// Create and configure the server
const app = new Hono();

// Add logger middleware
app.use("*", logger());

// Add CORS middleware
app.use("*", cors({
  origin: "*",
  allowMethods: ["GET"],
  allowHeaders: ["*"],
  credentials: true,
}));

app.use("*", rateLimiter({
  windowMs: 60 * 1000, // 1 minute
  limit: 1000, // Maximum requests per IP
  keyGenerator: (c) => {
    // We're always behind CF tunnels, so we can use CF-Connecting-IP header
    return c.req.header('CF-Connecting-IP') ?? '';
  }
}));

// Add Swagger documentation
app.get("/swagger", swaggerUI({
  url: "/openapi",
}));

app.get(
  '/openapi',
  openAPISpecs(app, {
    documentation: {
      info: {
        title: "Mojang API Proxy",
        version: "2.0.0",
        description: "A proxy service for Mojang API endpoints",
      },
      tags: [
        {name: "mojang", description: "Mojang API endpoints"},
        {name: "health", description: "Health check endpoint"},
      ],
      servers: [
        { url: 'http://localhost:3000', description: 'Local Server' },
      ],
    },
  })
)

// Add a redirect from root to docs
app.get("/", (c) => {
  return c.redirect("/swagger");
});

// Add routers
app.route("/mojang", mojangApiRouter);
app.route("/health", healthRouter);

// Global error handler
app.notFound((c) => {
  return c.json({ error: "Not Found" }, 404);
});

app.onError((err, c) => {
  console.error("Error:", err);
  return c.json({ error: "Internal Server Error" }, 500);
});

// Start the server
serve({
  fetch: app.fetch,
  port: PORT,
}, (info) => {
  console.log(`🦊 Server started at http://localhost:${info.port}`);
  console.log(`📚 API Documentation available at http://localhost:${info.port}/swagger`);
});

// Handle graceful shutdown
process.on("SIGINT", () => {
  console.log("Server shutting down...");
  process.exit(0);
});
