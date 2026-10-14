//! Client and protocol types for privately enhancing Ironwood compact actions.
pub mod client;
pub mod protocol;
mod record;
/// Experimental status protocol; not enabled in wallet production flows.
#[cfg(feature = "http-client")]
pub mod status;
pub mod types;

pub use record::{EnhanceTransactionMetadata, InvalidEnhanceRecord};
pub use types::{
    EnhanceRecord, EnhanceRecordParts, ACTIVATION_HEIGHT, ENHANCE_SETUP_SEED,
    FLAG_HAS_TRANSPARENT_INPUTS, FLAG_HAS_TRANSPARENT_OUTPUTS, ITEM_SIZE_BITS, NETWORK, POOL,
    PROTOCOL_REVISION, RECORDS_PER_ROW, RECORD_BYTES, ROW_BYTES, SCHEMA_VERSION, SHARD_ROWS,
};

#[cfg(feature = "native-reinspiring")]
pub mod native;
