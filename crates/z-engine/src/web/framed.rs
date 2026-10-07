//! Length-delimited compact-block blobs (u32 BE length + protobuf).

use crate::error::{EngineError, Result};
use prost::Message;
use rayon::prelude::*;
use zcash_client_backend::proto::compact_formats::CompactBlock;

/// ASCII magic for wallet snapshots (`zstk` + version byte in JSON).
pub const SNAPSHOT_MAGIC: &str = "zstk1";

/// Follow-on syncs stay on the calling worker; historic catch-up uses the pool.
pub(super) const SERIAL_SCAN_MAX_BLOCKS: usize = 32;

pub fn encode_one(block: &CompactBlock) -> Vec<u8> {
    let bytes = block.encode_to_vec();
    let n = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&bytes);
    out
}

pub fn encode_delimited(blocks: impl IntoIterator<Item = CompactBlock>) -> Vec<u8> {
    let mut out = Vec::new();
    for block in blocks {
        let bytes = block.encode_to_vec();
        let n = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    out
}

pub fn decode_delimited(bytes: &[u8]) -> Result<Vec<CompactBlock>> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut off = 0usize;
    while off < bytes.len() {
        if bytes.len() - off < 4 {
            return Err(EngineError::Message(
                "truncated compact-block length".into(),
            ));
        }
        let n = u32::from_be_bytes(bytes[off..off + 4].try_into().expect("4 bytes")) as usize;
        off += 4;
        if bytes.len() - off < n {
            return Err(EngineError::Message("truncated compact-block body".into()));
        }
        spans.push((off, n));
        off += n;
    }
    let decode_one = |s: usize, n: usize| {
        CompactBlock::decode(&bytes[s..s + n])
            .map_err(|e| EngineError::Message(format!("compact block: {e}")))
    };
    // Avoid waking the pool for ordinary new-block updates, including decoding.
    if spans.len() <= SERIAL_SCAN_MAX_BLOCKS || rayon::current_num_threads() <= 1 {
        return spans.into_iter().map(|(s, n)| decode_one(s, n)).collect();
    }
    spans
        .into_par_iter()
        .map(|(s, n)| decode_one(s, n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_decode_budgets_preserve_order_and_reject_invalid_frames() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        pool.install(|| {
            for n in [SERIAL_SCAN_MAX_BLOCKS, SERIAL_SCAN_MAX_BLOCKS + 1] {
                let blocks: Vec<_> = (0..n)
                    .map(|i| CompactBlock {
                        height: i as u64 + 1,
                        ..Default::default()
                    })
                    .collect();
                let blob = encode_delimited(blocks.clone());
                assert_eq!(decode_delimited(&blob).unwrap(), blocks);

                let mut truncated_length = blob.clone();
                truncated_length.push(0);
                assert!(decode_delimited(&truncated_length)
                    .unwrap_err()
                    .to_string()
                    .contains("truncated compact-block length"));
                let mut truncated_body = blob.clone();
                truncated_body.pop();
                assert!(decode_delimited(&truncated_body)
                    .unwrap_err()
                    .to_string()
                    .contains("truncated compact-block body"));
                let mut invalid_protobuf = blob;
                // Keep the frame count on the same side of the budget boundary.
                invalid_protobuf[4] = 0xff;
                assert!(decode_delimited(&invalid_protobuf)
                    .unwrap_err()
                    .to_string()
                    .contains("compact block:"));
            }
        });
    }
}
