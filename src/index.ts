import "dotenv-flow/config";

import { serve } from "@hono/node-server";
import { swaggerUI } from "@hono/swagger-ui";
import { OpenAPIHono } from "@hono/zod-openapi";
import { cors } from "hono/cors";
import { logger } from "hono/logger";
import { rateLimiter } from "hono-rate-limiter";
import { healthRouter } from "./routes/health";
import { mojangApiRouter } from "./routes/mojang-api";
import { batchProcessor } from "./utils/batch-processor";

// Get server port from environment variables or use default
const PORT = process.env.SERVER_PORT
  ? parseInt(process.env.SERVER_PORT, 10)
  : 3000;

// Create and configure the server
const app = new OpenAPIHono();

// Add logger middleware
app.use("*", logger());

// Add CORS middleware
app.use(
  "*",
  cors({
    origin: "*",
    allowMethods: ["GET"],
    allowHeaders: ["*"],
    credentials: true,
  }),
);

app.use(
  "*",
  rateLimiter({
    windowMs: 60 * 1000, // 1 minute
    limit: 1000, // Maximum requests per IP
    keyGenerator: (c) => {
      // We're always behind CF tunnels, so we can use CF-Connecting-IP header
      return c.req.header("CF-Connecting-IP") ?? "";
    },
  }),
);

// Add Swagger documentation
app.get(
  "/swagger",
  swaggerUI({
    url: "/openapi",
  }),
);

app.doc("/openapi", {
  openapi: "3.0.0",
  info: {
    title: "Mojang API Proxy",
    version: "2.0.0",
    description: "A proxy service for Mojang API endpoints",
  },
  tags: [
    { name: "mojang", description: "Mojang API endpoints" },
    { name: "health", description: "Health check endpoint" },
  ],
  servers: [
    { url: `https://eclipse.skinsrestorer.net`, description: "Main Server" },
    { url: `http://localhost:${PORT}`, description: "Local Server" },
  ],
});

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
serve(
  {
    fetch: app.fetch,
    port: PORT,
  },
  (info) => {
    console.log(`🦊 Server started at http://localhost:${info.port}`);
    console.log(
      `📚 API Documentation available at http://localhost:${info.port}/swagger`,
    );
  },
);

// Handle graceful shutdown
process.on("SIGINT", () => {
  console.log("Server shutting down...");
  batchProcessor.shutdown();
  process.exit(0);
});

process.on("SIGTERM", () => {
  console.log("Server shutting down...");
  batchProcessor.shutdown();
  process.exit(0);
});
