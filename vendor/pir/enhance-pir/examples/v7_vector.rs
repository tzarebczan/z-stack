use enhance_pir::protocol::*;
use sha2::{Digest, Sha256};

fn manifest(records: u64) -> Manifest {
    let coverage = Lifecycle::default()
        .coverage(records, Geometry::default())
        .unwrap();
    let sessions = coverage
        .shards
        .iter()
        .map(|s| SessionRef {
            shard_id: s.id,
            public_params_sha256: "ab".repeat(32),
            parameter_id: parameter_id(s.logical_rows).unwrap(),
        })
        .collect();
    let unit_identities = coverage
        .shards
        .iter()
        .map(|s| {
            (
                s.id,
                s.units
                    .iter()
                    .map(|u| UnitIdentity {
                        recovery_epoch: 0,
                        table: "enhance".into(),
                        shard_id: s.id,
                        local_row_start: u.local_row_start,
                        allocated_rows: u.allocated_rows,
                        setup_sha256: hex::encode(Sha256::digest(setup_seed(s.id))),
                        parameter_id: unit_parameter_id(u.allocated_rows).unwrap(),
                        content_sha256: "cd".repeat(32),
                    })
                    .collect(),
            )
        })
        .collect();
    Manifest {
        recovery_epoch: 0,
        placement_revision: 1,
        domain_recovery_epochs: coverage.shards.iter().map(|s| (s.id, "0".into())).collect(),
        schema_version: SCHEMA_VERSION,
        protocol_revision: PROTOCOL_REVISION.into(),
        network: "main".into(),
        pool: "ironwood".into(),
        generation: 1,
        anchor_height: 1000,
        anchor_block_hash: "ef".repeat(32),
        geometry: Geometry::default(),
        coverage,
        sessions,
        unit_identities,
    }
}

fn main() {
    let m = manifest(67);
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"session_id":hex::encode(m.session_id(0).unwrap()),"manifest":m})
        )
        .unwrap()
    );
}
