// Internal resource policy shared by streaming transports and scan prefetch.
// The working reservation covers frame/network storage plus contiguous conversion.
export const COMPACT_RANGE_MAX_BYTES = 32 * 1024 * 1024;
export const COMPACT_RANGE_WORKING_BYTES = 2 * COMPACT_RANGE_MAX_BYTES;
export const COMPACT_PREFETCH_MAX_BYTES = 256 * 1024 * 1024;
// Eight Private Cloud batches. Each reserves one working range, and each batch
// is two 500-block posts, which fills the gateway's 16 block permits.
export const COMPACT_PREFETCH_PRIVATE_CLOUD_MAX_BYTES = 8 * COMPACT_RANGE_WORKING_BYTES;
