/**
 * Utility functions for validation
 */

/**
 * Checks if a Minecraft username is invalid
 * @param username The username to validate
 * @returns true if the username is invalid, false if valid
 */
export function invalidMinecraftUsername(username: string): boolean {
  // Check username length (max 16 characters)
  // Note: there are exceptions to players with under 3 characters, who bought the game early in its development.
  if (username.length > 16) {
    return true;
  }

  // Check for valid characters
  // Note: Players who bought the game early in its development can have "-" in usernames.
  const validCharsRegex = /^[a-zA-Z0-9_-]+$/;
  return !validCharsRegex.test(username);
}
