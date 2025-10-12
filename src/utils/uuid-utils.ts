/**
 * Utility functions for UUID handling
 */

/**
 * Tries to parse a UUID string
 * @param str The string to parse as UUID
 * @returns The UUID string if valid, null if not
 */
export function tryParseUUID(str: string): string | null {
  // Check if it's already a valid dashed UUID format
  if (
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(str)
  ) {
    return str.toLowerCase();
  }

  // If we have a non-dashed UUID, convert it to dashed
  if (/^[0-9a-f]{32}$/i.test(str)) {
    try {
      return convertToDashed(str).toLowerCase();
    } catch (_e) {
      return null;
    }
  }

  return null;
}

/**
 * Converts a UUID without dashes to one with dashes
 * @param noDashes UUID string without dashes
 * @returns UUID string with dashes
 */
export function convertToDashed(noDashes: string): string {
  if (!/^[0-9a-f]{32}$/i.test(noDashes)) {
    throw new Error("Invalid UUID format");
  }

  return [
    noDashes.substring(0, 8),
    noDashes.substring(8, 12),
    noDashes.substring(12, 16),
    noDashes.substring(16, 20),
    noDashes.substring(20),
  ].join("-");
}

/**
 * Converts a UUID with dashes to one without dashes
 * @param uuid UUID string with dashes
 * @returns UUID string without dashes
 */
export function convertToNoDashes(uuid: string): string {
  return uuid.replace(/-/g, "");
}
