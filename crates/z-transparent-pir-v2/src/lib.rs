//! Rust-owned, yielding transparent v12 acquisition. No wallet balance authority.
#![allow(async_fn_in_trait)]
mod types;
pub use types::*;
mod adapters;
mod client;
mod memory_store;
mod pass;
mod serialization;
mod store;
mod sync;
mod transport;
pub use pass::{AcceptedRewind, Error, Pass};
mod ledger {
    pub use transparent_wallet::ledger::*;
}

mod shared_store;
