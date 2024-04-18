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

      // Add padding to the end of the byte array, as BitSet does not persist the length of the original byte array
      var exported = bitSet.toByteArray();
      var byteArray = new byte[ipBase.length];
      System.arraycopy(exported, 0, byteArray, 0, exported.length);

      return InetAddress.getByAddress(byteArray);
    } catch (Exception e) {
      log.error("Failed to get random local address", e);
      throw new RuntimeException("Failed to get random local address", e);
    }
  }
}
