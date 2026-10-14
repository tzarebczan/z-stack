//! Record and PIR constants shared by the current protocol and runtime.
pub use crate::protocol::{PROTOCOL_REVISION, SCHEMA_VERSION};
pub use crate::record::*;

pub const NETWORK: &str = "main";
pub const POOL: &str = "ironwood";
pub const ACTIVATION_HEIGHT: u64 = 3_428_143;
pub const RECORDS_PER_ROW: usize = 33;
pub const ROW_BYTES: usize = RECORD_BYTES * RECORDS_PER_ROW;
pub const SHARD_ROWS: usize = 8_192;
pub const ITEM_SIZE_BITS: u64 = (ROW_BYTES * 8) as u64;
pub const ENHANCE_SETUP_SEED: u64 = 0xa4d6_9bc2_317e_085f;

pub fn setup_seed_bytes() -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&ENHANCE_SETUP_SEED.to_le_bytes());
    bytes
}
