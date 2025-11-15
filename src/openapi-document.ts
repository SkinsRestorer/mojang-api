import { ErrorType } from "./utils/types";

type HttpStatus = 200 | 400 | 500 | 503;

type SchemaObject = Record<string, unknown>;

type Responses = Record<HttpStatus, SchemaObject>;

const errorSchema = (error: ErrorType): SchemaObject => ({
  description: "Error response",
  content: {
    "application/json": {
      schema: {
        type: "object",
        required: ["error"],
        properties: {
          error: {
            type: "string",
            enum: [error],
            description: "Error type",
          },
        },
      },
    },
  },
});

const uuidSuccessResponse: SchemaObject = {
  description: "Successful response",
  content: {
    "application/json": {
      schema: {
        $ref: "#/components/schemas/UuidLookupResponse",
      },
    },
  },
};

const skinSuccessResponse: SchemaObject = {
  description: "Successful response",
  content: {
    "application/json": {
      schema: {
        $ref: "#/components/schemas/SkinLookupResponse",
      },
    },
  },
};

const healthSuccessResponse: SchemaObject = {
  description: "Successful response",
  content: {
    "application/json": {
      schema: {
        type: "object",
        required: ["status"],
        properties: {
          status: {
            type: "string",
            example: "UP",
            description: "Health status of the service",
          },
        },
      },
    },
  },
};

const uuidResponses: Responses = {
  200: uuidSuccessResponse,
  400: errorSchema(ErrorType.INVALID_NAME),
  500: errorSchema(ErrorType.INTERNAL_ERROR),
  503: errorSchema(ErrorType.INTERNAL_TIMEOUT),
};

const skinResponses: Responses = {
  200: skinSuccessResponse,
  400: errorSchema(ErrorType.INVALID_UUID),
  500: errorSchema(ErrorType.INTERNAL_ERROR),
  503: errorSchema(ErrorType.INTERNAL_TIMEOUT),
};

export const buildOpenApiDocument = (localPort: number) => ({
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
    { url: "https://eclipse.skinsrestorer.net", description: "Main Server" },
    { url: `http://localhost:${localPort}`, description: "Local Server" },
  ],
  paths: {
    "/mojang/uuid/{name}": {
      get: {
        tags: ["mojang"],
        summary: "Convert a Minecraft username to UUID",
        description: "Convert a Minecraft username to UUID",
        parameters: [
          {
            name: "name",
            in: "path",
            required: true,
            description: "Minecraft username to convert to UUID",
            schema: { type: "string" },
          },
        ],
        responses: uuidResponses,
      },
    },
    "/mojang/skin/{uuid}": {
      get: {
        tags: ["mojang"],
        summary: "Get skin data for a Minecraft UUID",
        description: "Get skin data for a Minecraft UUID",
        parameters: [
          {
            name: "uuid",
            in: "path",
            required: true,
            description: "Minecraft UUID to get skin data for",
            schema: { type: "string" },
          },
        ],
        responses: skinResponses,
      },
    },
    "/health": {
      get: {
        tags: ["health"],
        summary: "Health check endpoint",
        description: "Health check endpoint",
        responses: {
          200: healthSuccessResponse,
        },
      },
    },
  },
  components: {
    schemas: {
      UuidLookupResponse: {
        type: "object",
        required: ["exists", "uuid"],
        properties: {
          exists: {
            type: "boolean",
            description: "Indicates whether the requested player exists",
          },
          uuid: {
            type: "string",
            nullable: true,
            description: "The player's UUID when found, otherwise null",
          },
        },
      },
      SkinProperty: {
        type: "object",
        required: ["value", "signature"],
        properties: {
          value: {
            type: "string",
            description: "Base64 encoded skin data",
          },
          signature: {
            type: "string",
            description: "Skin data signature",
          },
        },
      },
      SkinLookupResponse: {
        type: "object",
        required: ["exists", "skinProperty"],
        properties: {
          exists: {
            type: "boolean",
            description:
              "Indicates whether a skin property exists for the UUID",
          },
          skinProperty: {
            $ref: "#/components/schemas/SkinProperty",
            nullable: true,
            description: "Skin property payload when available",
          },
        },
      },
    },
  },
});
