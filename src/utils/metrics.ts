export const metrics = {
  uuidRequests: 0,
  skinRequests: 0,

  uuidCacheHits: 0,
  uuidCacheMisses: 0,
  skinCacheHits: 0,
  skinCacheMisses: 0,

  batchesProcessed: 0,
  usernamesBatched: 0,

  bytesSentToMojang: 0,
  bytesReceivedFromMojang: 0,

  mojangRequests: 0,
  mojangErrors: 0,

  startedAt: Date.now(),
  lastReportAt: Date.now(),

  reset() {
    this.uuidRequests = 0;
    this.skinRequests = 0;
    this.uuidCacheHits = 0;
    this.uuidCacheMisses = 0;
    this.skinCacheHits = 0;
    this.skinCacheMisses = 0;
    this.batchesProcessed = 0;
    this.usernamesBatched = 0;
    this.bytesSentToMojang = 0;
    this.bytesReceivedFromMojang = 0;
    this.mojangRequests = 0;
    this.mojangErrors = 0;
    this.lastReportAt = Date.now();
  },
};
