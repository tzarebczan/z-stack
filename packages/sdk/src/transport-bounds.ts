// Internal resource policy shared by streaming transports and scan prefetch.
// The working reservation covers frame/network storage plus contiguous conversion.
export const COMPACT_RANGE_MAX_BYTES = 32 * 1024 * 1024;
export const COMPACT_RANGE_WORKING_BYTES = 2 * COMPACT_RANGE_MAX_BYTES;
export const COMPACT_PREFETCH_MAX_BYTES = 256 * 1024 * 1024;
