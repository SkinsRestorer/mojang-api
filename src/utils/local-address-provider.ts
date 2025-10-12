export function getRandomLocalAddressHost(): string {
  const IP_BASE = process.env.IP_BASE ?? "0.0.0.0";
  const IP_RANGE = parseInt(process.env.IP_RANGE ?? "0", 10);

  if (!IP_BASE || Number.isNaN(IP_RANGE)) {
    throw new Error("IP_BASE and IP_RANGE environment variables must be set.");
  }

  try {
    const base = parseIpToBytes(IP_BASE);
    const bits = base.length * 8;
    const bitSet = bytesToBitSet(base);

    // Override the last `IP_RANGE` bits with random bits
    for (let i = 0; i < IP_RANGE; i++) {
      const bitIndex = bits - i - 1;
      bitSet[bitIndex] = Math.random() >= 0.5;
    }

    const randomizedBytes = bitSetToBytes(bitSet, base.length);
    return bytesToIp(randomizedBytes);
  } catch (err) {
    console.error("Failed to get random local address", err);
    throw new Error("Failed to get random local address");
  }
}

function parseIpToBytes(ip: string): number[] {
  if (ip.includes(":")) {
    // IPv6
    const parts = ip.split(":").filter(Boolean);
    const buffer = new Uint8Array(16);
    let offset = 0;
    for (const part of parts) {
      const value = parseInt(part, 16);
      buffer[offset++] = (value >> 8) & 0xff;
      buffer[offset++] = value & 0xff;
    }
    return Array.from(buffer);
  } else {
    // IPv4
    return ip.split(".").map((v) => parseInt(v, 10));
  }
}

function bytesToIp(bytes: number[]): string {
  if (bytes.length === 4) {
    return bytes.join(".");
  } else if (bytes.length === 16) {
    const parts = [];
    for (let i = 0; i < 16; i += 2) {
      parts.push(((bytes[i] << 8) | bytes[i + 1]).toString(16));
    }
    return parts.join(":");
  } else {
    throw new Error(`Unsupported IP byte length: ${bytes.length}`);
  }
}

function bytesToBitSet(bytes: number[]): boolean[] {
  const bits: boolean[] = [];
  for (const byte of bytes) {
    for (let i = 7; i >= 0; i--) {
      bits.push((byte & (1 << i)) !== 0);
    }
  }
  return bits;
}

function bitSetToBytes(bits: boolean[], byteLength: number): number[] {
  const bytes = new Array(byteLength).fill(0);
  for (let i = 0; i < bits.length && i < byteLength * 8; i++) {
    if (bits[i]) {
      bytes[Math.floor(i / 8)] |= 1 << (7 - (i % 8));
    }
  }
  return bytes;
}
