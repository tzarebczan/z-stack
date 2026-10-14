// Public synthetic protocol fixture. No wallet, keys, fullnode or live network.
use base64::{engine::general_purpose::STANDARD, Engine};
use enhance_pir::protocol::*;
use inspiring::{QueryPackPreprocessed, TopKeyImages};
use ipir_sp::{
    serialize::{deserialize_packing_keys, serialized_packing_keys_len},
    server::{build_pack_preprocessed_blocks, published_c1_rows, YServer},
    IPIRClient, ProductionSimplePirParams, SimplePirProfile,
};
use sha2::{Digest, Sha256};

pub struct Fixture<'a> {
    profile: &'a ProductionSimplePirParams,
    server: YServer<u16>,
    pre: Vec<QueryPackPreprocessed<'a>>,
    top: TopKeyImages<'a>,
    pub manifest: Vec<u8>,
    pub material: Vec<u8>,
}
pub fn profile() -> ProductionSimplePirParams {
    ProductionSimplePirParams::new(4096, enhance_pir::ITEM_SIZE_BITS, SimplePirProfile::P16Q48)
        .unwrap()
}
pub fn record(position: u64) -> [u8; 653] {
    let mut record = [0; 653];
    for (i, byte) in record[..640].iter_mut().enumerate() {
        *byte = (i as u64 + position * 13) as u8;
    }
    record
}
impl<'a> Fixture<'a> {
    pub fn new(profile: &'a ProductionSimplePirParams) -> Self {
        Self::with_records(
            profile,
            &(0..67).map(record).collect::<Vec<_>>(),
            [0xef; 32],
        )
    }
    pub fn with_records(
        profile: &'a ProductionSimplePirParams,
        records: &[[u8; 653]],
        anchor_hash: [u8; 32],
    ) -> Self {
        let params = profile.ypir();
        let columns = params.db_cols;
        let mut rows = vec![0u16; records.len().div_ceil(33) * columns];
        for (position, record) in records.iter().enumerate() {
            let (row, slot) = (position / 33, position % 33);
            let mut bytes = vec![0u8; columns * 2];
            // Read previous slots from their complete u16 words before adding
            // the next odd-width653B record, so records can cross word boundaries.
            for (value, pair) in rows[row * columns..(row + 1) * columns]
                .iter()
                .zip(bytes.chunks_exact_mut(2))
            {
                pair.copy_from_slice(&value.to_le_bytes());
            }
            bytes[slot * 653..(slot + 1) * 653].copy_from_slice(record);
            for (value, pair) in rows[row * columns..(row + 1) * columns]
                .iter_mut()
                .zip(bytes.chunks_exact(2))
            {
                *value = u16::from_le_bytes([pair[0], pair[1]]);
            }
        }
        let server = YServer::from_profile(
            profile,
            rows.into_iter()
                .chain(std::iter::repeat(0))
                .take(params.db_rows * columns),
            false,
            true,
        );
        let client =
            IPIRClient::from_profile(4096, enhance_pir::ITEM_SIZE_BITS, SimplePirProfile::P16Q48)
                .unwrap();
        let setup = client.generate_public_query_setup_simplepir_from_seed(setup_seed(0));
        let offline =
            server.perform_offline_precomputation_simplepir(profile.rlwe(), setup.polys());
        let pre = build_pack_preprocessed_blocks(profile.rlwe(), &offline.crs_blocks).unwrap();
        let public = published_c1_rows(&pre, profile.rlwe().q);
        let coverage = Lifecycle::default()
            .coverage(records.len() as u64, Geometry::default())
            .unwrap();
        let unit = coverage.shards[0].units[0].clone();
        let manifest = Manifest {
            recovery_epoch: 0,
            placement_revision: 1,
            domain_recovery_epochs: [(0, "0".into())].into(),
            schema_version: SCHEMA_VERSION,
            protocol_revision: PROTOCOL_REVISION.into(),
            network: "main".into(),
            pool: "ironwood".into(),
            generation: 1,
            anchor_height: 3_500_000,
            anchor_block_hash: hex::encode(anchor_hash),
            geometry: Geometry::default(),
            coverage,
            sessions: vec![SessionRef {
                shard_id: 0,
                public_params_sha256: hex::encode(Sha256::digest(&public)),
                parameter_id: parameter_id(4096).unwrap(),
            }],
            unit_identities: [(
                0,
                vec![UnitIdentity {
                    recovery_epoch: 0,
                    table: "enhance".into(),
                    shard_id: 0,
                    local_row_start: unit.local_row_start,
                    allocated_rows: unit.allocated_rows,
                    setup_sha256: hex::encode(Sha256::digest(setup_seed(0))),
                    parameter_id: unit_parameter_id(unit.allocated_rows).unwrap(),
                    content_sha256: "cd".repeat(32),
                }],
            )]
            .into(),
        };
        manifest.validate().unwrap();
        let material = ShardSession {
            session_id: hex::encode(manifest.session_id(0).unwrap()),
            generation: 1,
            shard_id: 0,
            params: parameters(4096).unwrap(),
            public_params_base64: STANDARD.encode(public),
        };
        Self {
            profile,
            server,
            pre,
            top: TopKeyImages::build(profile.rlwe()),
            manifest: serde_json::to_vec(&manifest).unwrap(),
            material: serde_json::to_vec(&material).unwrap(),
        }
    }
    pub fn answer(&self, body: &[u8]) -> Vec<u8> {
        let binding = QueryBinding::decode(body).unwrap();
        let body = &body[HEADER_BYTES..];
        let keys_len = serialized_packing_keys_len(self.profile.rlwe());
        let keys = deserialize_packing_keys(self.profile.rlwe(), &body[..keys_len]).unwrap();
        let (answer, _) = self
            .server
            .perform_full_online_computation_simplepir_measured(
                self.profile.rlwe(),
                &body[keys_len..],
                &keys,
                &self.top,
                &self.pre,
            )
            .unwrap();
        [binding.encode(), answer].concat()
    }
}
