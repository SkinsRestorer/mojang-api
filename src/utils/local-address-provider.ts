/**
 * Utility to provide random local IP addresses for outgoing connections
 * This is a JavaScript implementation of the Java LocalAddressProvider
 */

/**
 * Returns a random local IP address within the configured range
 * @returns A random IP address object that can be used for binding
 */
export function getRandomLocalAddress(): { address: string, port: number } {
  try {
    const ipBase = process.env.IP_BASE || '127.0.0.1';
    const ipRange = process.env.IP_RANGE ? parseInt(process.env.IP_RANGE) : 0;

    // If no range is configured, return the base IP
    if (ipRange <= 0) {
      return {
        address: ipBase,
        port: 0
      };
    }

    // Parse the IP address into its components
    const ipParts = ipBase.split('.').map(Number);

    // Validate IP format
    if (ipParts.length !== 4 || ipParts.some(part => isNaN(part) || part < 0 || part > 255)) {
      console.error('Invalid IP_BASE format, using default IP');
      return {
        address: '127.0.0.1',
        port: 0
      };
    }

    // Generate a random IP within the specified range
    // For simplicity, we'll only modify the last octet within the range
    const lastOctetMax = Math.min(255, ipParts[3] + ipRange);
    const randomLastOctet = ipParts[3] + Math.floor(Math.random() * (lastOctetMax - ipParts[3] + 1));

    const randomIp = `${ipParts[0]}.${ipParts[1]}.${ipParts[2]}.${randomLastOctet}`;

    return {
      address: randomIp,
      port: 0
    };
  } catch (error) {
    console.error('Failed to get random local address', error);
    return {
      address: '127.0.0.1',
      port: 0
    };
  }
}
