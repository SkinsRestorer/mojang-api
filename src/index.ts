import {Elysia, redirect} from "elysia";
import {cors} from "@elysiajs/cors";
import {swagger} from "@elysiajs/swagger";
import {mojangApiRouter} from "./routes/mojang-api";
import {healthRouter} from "./routes/health";
import {Generator, rateLimit} from "elysia-rate-limit";

// Get server port from environment variables or use default
const PORT = process.env.SERVER_PORT ? parseInt(process.env.SERVER_PORT) : 3000;

const cloudflareGenerator: Generator = (req, server) => {
  // We're always behind CF tunnels, so we can use CF-Connecting-IP header
  return req.headers.get('CF-Connecting-IP') ?? server?.requestIP(req)?.address ?? ''
}

// Create and configure the server
const app = new Elysia()
  .use(rateLimit({
    generator: cloudflareGenerator,
    max: 1000, // Maximum requests per IP
    duration: 60 * 1000, // Time window in milliseconds
  }))
  // Add CORS middleware
  .use(
    cors({
      origin: "*",
      methods: ["GET"],
      allowedHeaders: "*",
      credentials: true,
    })
  )
  // Add Swagger documentation
  .use(
    swagger({
      documentation: {
        info: {
          title: "Mojang API Proxy",
          version: "2.0.0",
          description: "A proxy service for Mojang API endpoints",
        },
        tags: [
          {name: "mojang", description: "Mojang API endpoints"},
          {name: "health", description: "Health check endpoint"},
        ]
      },
      exclude: ['/']
    })
  )
  // Add routers
  .use(mojangApiRouter)
  .use(healthRouter)
  // Global error handler
  .onError(({code, error, set}) => {
    console.error(`Error [${code}]:`, error);

    if (code === "NOT_FOUND") {
      set.status = 404;
      return {error: "Not Found"};
    }

    set.status = 500;
    return {error: "Internal Server Error"};
  })
  // Add a redirect from root to docs
  .get("/", redirect("/swagger"));

// Start the server
app.listen(PORT, () => {
  console.log(`🦊 Server started at http://localhost:${PORT}`);
  console.log(
    `📚 API Documentation available at http://localhost:${PORT}/swagger`
  );
});

// Handle graceful shutdown
process.on("SIGINT", () => {
  console.log("Server shutting down...");
  process.exit(0);
});
