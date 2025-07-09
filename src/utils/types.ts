/**
 * Response types for the Mojang API
 */

export interface ErrorResponse {
  error: ErrorType;
}

export enum ErrorType {
  INVALID_NAME = 'INVALID_NAME',
  INVALID_UUID = 'INVALID_UUID',
  INTERNAL_TIMEOUT = 'INTERNAL_TIMEOUT',
  INTERNAL_ERROR = 'INTERNAL_ERROR'
}

export interface UUIDResponse {
  exists: boolean;
  uuid: string | null;
}

export interface ProfileResponse {
  exists: boolean;
  skinProperty: SkinProperty | null;
}

export interface SkinProperty {
  value: string;
  signature: string;
}

// Mojang API response types
export interface MojangUUIDResponse {
  id: string | null;
  name?: string;
}

export interface MojangProfileResponse {
  id?: string;
  name?: string;
  properties?: MojangProperty[];
}

export interface MojangProperty {
  name: string;
  value: string;
  signature: string;
}

// Constants for the Mojang API
export const MOJANG_API = {
  // API endpoints
  UUID_URL: 'https://api.minecraftservices.com/minecraft/profile/lookup/name/%s',
  PROFILE_URL: 'https://sessionserver.mojang.com/session/minecraft/profile/%s?unsigned=false',

  // Cache duration in minutes
  CACHE_DURATION: 15,

  // Response headers for caching
  CACHE_HEADERS: {
    'Cache-Control': `public, max-age=${15 * 60}`,
    'Content-Type': 'application/json'
  }
};
