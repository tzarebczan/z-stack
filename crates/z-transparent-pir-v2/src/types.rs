use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Table {
    Directory,
    Pages { child: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Init,
    Map,
    Filter {
        shard_id: u64,
    },
    Manifest {
        shard_id: u64,
        revision: String,
    },
    Setup {
        shard_id: u64,
        revision: String,
        table: Table,
        segment: u32,
    },
    Query {
        shard_id: u64,
        revision: String,
        table: Table,
        body: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Data { body: String },
    Stale { map_digest_hint: Option<String> },
    Overloaded { retry_after: Option<u64> },
    Misrouted,
    Unavailable,
    Refused,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub height: u64,
    pub hash: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub script: String,
    pub required_from: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub source_id: String,
    pub generation: u64,
    pub scope_id: String,
    pub anchor: Anchor,
    pub scripts: Vec<Script>,
    pub accepted_headers: Vec<Anchor>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub script: String,
    pub event: String,
    pub shard_id: u64,
    pub revision: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub script: String,
    pub required_from: u64,
    pub through: u64,
    pub revision: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub source_id: String,
    pub generation: u64,
    pub scope_id: String,
    pub checkpoint_id: String,
    pub previous_checkpoint_id: Option<String>,
    pub anchor: Anchor,
    pub covered_through: u64,
    pub complete: bool,
    pub pending_pages: u64,
    pub unresolved_spends: u64,
    pub records: Vec<Record>,
    pub coverage: Vec<Coverage>,
    pub rewind_to: Option<Anchor>,
    pub continuation: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Independently retrieved historical chain points per pass, including
    /// prior-anchor revalidation. Reaching this limit commits resumable progress.
    #[serde(default = "default_chain_reads")]
    pub max_chain_reads: u64,
    pub max_exchanges: u64,
    pub max_download_bytes: u64,
    pub max_upload_bytes: u64,
    pub max_queries: u64,
    pub max_private_bytes: u64,
    pub max_scripts: usize,
}
fn default_chain_reads() -> u64 {
    32
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_chain_reads: default_chain_reads(),
            max_exchanges: 64,
            max_download_bytes: 256 * 1024 * 1024,
            max_upload_bytes: 32 * 1024 * 1024,
            max_queries: 64,
            max_private_bytes: 96 * 1024 * 1024,
            max_scripts: 4096,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Status {
    Complete,
    Incomplete { reason: String },
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Exchange {
        request_id: u64,
        operation: Operation,
    },
    NeedChain {
        height: u64,
        hash: String,
    },
    Checkpoint {
        checkpoint: Checkpoint,
    },
    Stopped {
        status: Status,
    },
}
