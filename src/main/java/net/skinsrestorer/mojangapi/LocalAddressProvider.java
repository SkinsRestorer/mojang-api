package net.skinsrestorer.mojangapi;

import lombok.extern.slf4j.Slf4j;

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.util.BitSet;
import java.util.concurrent.ThreadLocalRandom;

@Slf4j
public class LocalAddressProvider {
  private static final String IP_BASE = System.getenv("IP_BASE");
  private static final int IP_RANGE = Integer.parseInt(System.getenv("IP_RANGE"));

  public static InetSocketAddress getRandomLocalAddress() {
    return new InetSocketAddress(getRandomLocalAddressHost(), 0);
  }

  private static InetAddress getRandomLocalAddressHost() {
    try {
      var ipBase = InetAddress.getByName(IP_BASE).getAddress();
      var bitSet = BitSet.valueOf(ipBase);
      var bits = ipBase.length * 8;

      for (int i = 0; i < IP_RANGE; i++) {
        bitSet.set(bits - i - 1, ThreadLocalRandom.current().nextBoolean());
      }

      return InetAddress.getByAddress(bitSet.toByteArray());
    } catch (Exception e) {
      log.error("Failed to get random local address", e);
      throw new RuntimeException("Failed to get random local address", e);
    }
  }
}
