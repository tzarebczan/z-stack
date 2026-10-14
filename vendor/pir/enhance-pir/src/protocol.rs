//! Schema-11 query-domain geometry. The wire identifiers remain fixed for wallet compatibility.
use crate::types::{RECORDS_PER_ROW, RECORD_BYTES, ROW_BYTES};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Frozen schema-11 wallet limit, independent of worker placement density.
pub const MAX_QUERY_SHARDS: u64 = 24;
pub const SCHEMA_VERSION: u16 = 11;
#[cfg(not(feature = "native-reinspiring"))]
pub const PROTOCOL_REVISION: &str = "ironwood-enhance-pir-v7";
#[cfg(feature = "native-reinspiring")]
pub const PROTOCOL_REVISION: &str = "ironwood-enhance-pir-v9-native-two-mask-m29";
pub const RETAINED_GENERATIONS: usize = 5;
pub const HEADER_BYTES: usize = 116;
/// A query's selection covers whole blocks of this many rows: the native ring
/// degree, which is also the smallest mutable unit.
pub const QUERY_ROW_QUANTUM: u64 = 2048;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub max_shard_rows: u64,
    pub min_shard_rows: u64,
    pub max_mutable_unit_rows: u64,
    pub min_mutable_unit_rows: u64,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            max_shard_rows: 32768,
            min_shard_rows: 4096,
            max_mutable_unit_rows: 8192,
            min_mutable_unit_rows: 2048,
        }
    }
}

impl Geometry {
    /// Only the geometry family exercised by the qualification suite is accepted.
    pub fn validate(self) -> Result<(), String> {
        if self.max_shard_rows != 32768
            || self.min_shard_rows != 4096
            || self.max_mutable_unit_rows != 8192
            || self.min_mutable_unit_rows != 2048
        {
            return Err("unqualified geometry".into());
        }
        Ok(())
    }

    pub fn logical_rows(self, records: u64) -> Result<u64, String> {
        self.validate()?;
        if records == 0 || records > self.max_shard_rows * RECORDS_PER_ROW as u64 {
            return Err("empty or oversized query shard".into());
        }
        Ok(records
            .div_ceil(RECORDS_PER_ROW as u64)
            .max(self.min_shard_rows)
            .next_power_of_two())
    }

    pub fn units(self, records: u64) -> Result<Vec<MutableUnit>, String> {
        self.logical_rows(records)?;
        let used = records.div_ceil(RECORDS_PER_ROW as u64);
        let mut units = Vec::new();
        let mut start = 0;
        while start < used {
            let rows = (used - start).min(self.max_mutable_unit_rows);
            let allocated = rows.max(self.min_mutable_unit_rows).next_power_of_two();
            units.push(MutableUnit {
                local_row_start: start,
                used_rows: rows,
                allocated_rows: allocated,
            });
            start += rows;
        }
        Ok(units)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MutableUnit {
    pub local_row_start: u64,
    pub used_rows: u64,
    pub allocated_rows: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ShardState {
    Growing,
    Provisional,
    Sealed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryShard {
    pub id: u64,
    pub global_row_start: u64,
    pub records: u64,
    pub logical_rows: u64,
    pub state: ShardState,
    pub units: Vec<MutableUnit>,
}

impl QueryShard {
    pub fn locate(&self, position: u64) -> Option<(usize, usize)> {
        let first = self.global_row_start.checked_mul(RECORDS_PER_ROW as u64)?;
        let relative = position.checked_sub(first)?;
        (relative < self.records).then_some((
            (relative / RECORDS_PER_ROW as u64) as usize,
            (relative % RECORDS_PER_ROW as u64) as usize,
        ))
    }
}

/// Persisted fixed-range identities; confirmation is decided from canonical blocks.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lifecycle {
    pub identities: Vec<u64>,
    pub next_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub global_start: u64,
    pub global_end: u64,
    pub domain_id: u64,
    pub local_start: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub records: u64,
    pub shards: Vec<QueryShard>,
    pub routes: Vec<Route>,
}

impl QueryShard {
    pub fn composed(&self) -> bool {
        self.id > 0
            && self.records.div_ceil(RECORDS_PER_ROW as u64) < Geometry::default().min_shard_rows
    }

    pub fn expected_units(&self, geometry: Geometry) -> Result<Vec<MutableUnit>, String> {
        let mut units = geometry.units(self.records)?;
        if self.composed() {
            units.push(MutableUnit {
                local_row_start: geometry.min_shard_rows,
                used_rows: geometry.min_shard_rows,
                allocated_rows: geometry.min_shard_rows,
            });
        }
        Ok(units)
    }

    pub fn expected_logical_rows(&self, geometry: Geometry) -> Result<u64, String> {
        if self.composed() {
            Ok(2 * geometry.min_shard_rows)
        } else {
            geometry.logical_rows(self.records)
        }
    }

    /// Rows a query selects over: the end of the last row any unit can hold,
    /// rounded up to whole [`QUERY_ROW_QUANTUM`] blocks. Every later row is
    /// prescribed zero padding, so its selection coefficients cannot change
    /// the answer and a native query may omit them. Full and composed domains
    /// select over every logical row.
    ///
    /// A function of `id` and `records` alone, both of which the session ID
    /// binds, and never of the lifecycle state, which it does not. Every query
    /// to a session therefore has the same length whatever row it selects.
    pub fn query_rows(&self, geometry: Geometry) -> Result<u64, String> {
        let end = self
            .expected_units(geometry)?
            .iter()
            .map(|u| u.local_row_start + u.used_rows.next_multiple_of(QUERY_ROW_QUANTUM))
            .max()
            .ok_or("empty query shard")?;
        Ok(end.min(self.expected_logical_rows(geometry)?))
    }
}

impl Lifecycle {
    /// Build one complete fixed-range view. Full ranges are provisional until
    /// their completing block is confirmed by the canonical controller.
    pub fn coverage(&mut self, records: u64, geometry: Geometry) -> Result<Coverage, String> {
        geometry.validate()?;
        let span = geometry.max_shard_rows * RECORDS_PER_ROW as u64;
        let count = records.div_ceil(span);
        if count == 0 || count > MAX_QUERY_SHARDS {
            return Err("unsupported coverage".into());
        }
        if self.next_id > MAX_QUERY_SHARDS {
            return Err("invalid persisted identity ceiling".into());
        }
        self.next_id = self.next_id.max(count);
        self.identities = (0..self.next_id).collect();
        let mut shards = Vec::new();
        let mut routes = Vec::new();
        for id in 0..count {
            let n = (records - id * span).min(span);
            let mut shard = QueryShard {
                id,
                global_row_start: id * geometry.max_shard_rows,
                records: n,
                logical_rows: 0,
                state: if n == span {
                    ShardState::Provisional
                } else {
                    ShardState::Growing
                },
                units: Vec::new(),
            };
            shard.logical_rows = shard.expected_logical_rows(geometry)?;
            shard.units = shard.expected_units(geometry)?;
            if shard.composed() {
                let previous: &mut Route = routes.last_mut().ok_or("missing predecessor")?;
                previous.global_end -= geometry.min_shard_rows;
                routes.push(Route {
                    global_start: shard.global_row_start - geometry.min_shard_rows,
                    global_end: shard.global_row_start,
                    domain_id: id,
                    local_start: geometry.min_shard_rows,
                });
            }
            routes.push(Route {
                global_start: shard.global_row_start,
                global_end: shard.global_row_start + n.div_ceil(RECORDS_PER_ROW as u64),
                domain_id: id,
                local_start: 0,
            });
            shards.push(shard);
        }
        Ok(Coverage {
            records,
            shards,
            routes,
        })
    }
}

impl Coverage {
    pub fn validate(&self, geometry: Geometry) -> Result<(), String> {
        let expected = Lifecycle::default().coverage(self.records, geometry)?;
        if self.routes != expected.routes || self.shards.len() != expected.shards.len() {
            return Err("noncanonical routing coverage".into());
        }
        for (actual, mut expected) in self.shards.iter().zip(expected.shards) {
            if actual.state == ShardState::Sealed && expected.state == ShardState::Provisional {
                expected.state = ShardState::Sealed;
            }
            if actual != &expected {
                return Err("invalid fixed domain geometry".into());
            }
        }
        Ok(())
    }

    pub fn locate(&self, position: u64) -> Option<(&QueryShard, usize, usize)> {
        if position >= self.records {
            return None;
        }
        let row = position / RECORDS_PER_ROW as u64;
        let route = self
            .routes
            .iter()
            .find(|r| r.global_start <= row && row < r.global_end)?;
        let shard = self.shards.iter().find(|s| s.id == route.domain_id)?;
        Some((
            shard,
            (route.local_start + row - route.global_start) as usize,
            (position % RECORDS_PER_ROW as u64) as usize,
        ))
    }
}

/// Canonical JSON u64 encoding avoids loss through JavaScript numbers.
pub mod decimal_u64 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(n: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&n.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let text = String::deserialize(d)?;
        let n: u64 = text.parse().map_err(serde::de::Error::custom)?;
        if n.to_string() != text {
            return Err(serde::de::Error::custom("noncanonical u64"));
        }
        Ok(n)
    }
}

/// All fields participate in cache identity, including the shard-local setup slice.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UnitIdentity {
    #[serde(with = "decimal_u64")]
    pub recovery_epoch: u64,
    pub table: String,
    pub shard_id: u64,
    pub local_row_start: u64,
    pub allocated_rows: u64,
    pub setup_sha256: String,
    pub parameter_id: String,
    pub content_sha256: String,
}

impl UnitIdentity {
    pub fn digest(&self) -> String {
        digest(self)
    }
}

pub fn digest(value: &impl Serialize) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("serializable protocol value"),
    ))
}

/// Deterministic public setup domain separation; this is public material, not a key.
pub fn setup_seed(shard_id: u64) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"ironwood-enhance-pir-v4/main/ironwood/setup\0");
    hash.update(crate::types::setup_seed_bytes());
    hash.update(shard_id.to_le_bytes());
    hash.finalize().into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryBinding {
    /// The routing revision, not the lifetime of the session material.
    pub generation: u64,
    pub shard_id: u64,
    pub epoch: [u8; 8],
    pub recovery_epoch: u64,
    pub session_id: [u8; 32],
    pub request_id: [u8; 16],
    pub anchor_hash: [u8; 32],
}

impl QueryBinding {
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = b"EPQ7".to_vec();
        bytes.extend(self.generation.to_le_bytes());
        bytes.extend(self.shard_id.to_le_bytes());
        bytes.extend(self.epoch);
        bytes.extend(self.recovery_epoch.to_le_bytes());
        bytes.extend(self.session_id);
        bytes.extend(self.request_id);
        bytes.extend(self.anchor_hash);
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < HEADER_BYTES || &bytes[..4] != b"EPQ7" {
            return Err("invalid v7 query framing".into());
        }
        Ok(Self {
            generation: u64::from_le_bytes(bytes[4..12].try_into().unwrap()),
            shard_id: u64::from_le_bytes(bytes[12..20].try_into().unwrap()),
            epoch: bytes[20..28].try_into().unwrap(),
            recovery_epoch: u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
            session_id: bytes[36..68].try_into().unwrap(),
            request_id: bytes[68..84].try_into().unwrap(),
            anchor_hash: bytes[84..116].try_into().unwrap(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionRef {
    pub shard_id: u64,
    pub public_params_sha256: String,
    pub parameter_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(with = "decimal_u64")]
    pub recovery_epoch: u64,
    pub placement_revision: u64,
    pub domain_recovery_epochs: std::collections::BTreeMap<u64, String>,
    pub schema_version: u16,
    pub protocol_revision: String,
    pub network: String,
    pub pool: String,
    pub generation: u64,
    pub anchor_height: u64,
    pub anchor_block_hash: String,
    pub geometry: Geometry,
    pub coverage: Coverage,
    pub sessions: Vec<SessionRef>,
    pub unit_identities: std::collections::BTreeMap<u64, Vec<UnitIdentity>>,
}

impl Manifest {
    /// Hash ordered padded unit commitments, geometry and packing material.
    /// Unit hashes commit to every stored byte; gaps are prescribed zero padding.
    pub fn session_id(&self, id: u64) -> Result<[u8; 32], String> {
        let shard = self
            .coverage
            .shards
            .iter()
            .find(|s| s.id == id)
            .ok_or("unknown domain")?;
        let reference = self
            .sessions
            .iter()
            .find(|s| s.shard_id == id)
            .ok_or("missing session")?;
        let epoch = self
            .domain_recovery_epochs
            .get(&id)
            .ok_or("missing domain epoch")?;
        let number: u64 = epoch.parse().map_err(|_| "invalid domain epoch")?;
        if number.to_string() != *epoch || number > self.recovery_epoch {
            return Err("invalid domain epoch".into());
        }
        let units = self.unit_identities.get(&id).ok_or("missing units")?;
        let mut hash = Sha256::new();
        hash.update(b"enhance-pir/v7/session\0");
        hash.update((PROTOCOL_REVISION.len() as u64).to_le_bytes());
        hash.update(PROTOCOL_REVISION.as_bytes());
        for n in [
            id,
            number,
            shard.logical_rows,
            shard.records,
            units.len() as u64,
        ] {
            hash.update(n.to_le_bytes());
        }
        for unit in units {
            hash.update(unit.recovery_epoch.to_le_bytes());
            hash.update(unit.local_row_start.to_le_bytes());
            hash.update(unit.allocated_rows.to_le_bytes());
            for value in [&unit.content_sha256, &unit.setup_sha256, &unit.parameter_id] {
                hash.update((value.len() as u64).to_le_bytes());
                hash.update(value.as_bytes());
            }
        }
        for value in [&reference.parameter_id, &reference.public_params_sha256] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
        Ok(hash.finalize().into())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION
            || self.protocol_revision != PROTOCOL_REVISION
            || self.network != "main"
            || self.pool != "ironwood"
            || self.generation == 0
            || self.anchor_block_hash.len() != 64
            || !canonical_hash(&self.anchor_block_hash)
        {
            return Err("incompatible manifest".into());
        }
        self.coverage.validate(self.geometry)?;
        if self.domain_recovery_epochs.len() != self.coverage.shards.len()
            || self.sessions.len() != self.coverage.shards.len()
            || self.unit_identities.len() != self.coverage.shards.len()
        {
            return Err("incomplete sessions".into());
        }
        for (shard, session) in self.coverage.shards.iter().zip(&self.sessions) {
            self.session_id(shard.id)?;
            let units = self
                .unit_identities
                .get(&shard.id)
                .ok_or("missing unit identities")?;
            if units.len() != shard.units.len() {
                return Err("incomplete unit identities".into());
            }
            for (identity, unit) in units.iter().zip(&shard.units) {
                if identity.recovery_epoch.to_string() != self.domain_recovery_epochs[&shard.id]
                    || identity.shard_id != shard.id
                    || identity.table != "enhance"
                    || identity.local_row_start != unit.local_row_start
                    || identity.allocated_rows != unit.allocated_rows
                    || identity.parameter_id != unit_parameter_id(unit.allocated_rows)?
                    || identity.content_sha256.len() != 64
                    || !canonical_hash(&identity.content_sha256)
                    || identity.setup_sha256 != hex::encode(Sha256::digest(setup_seed(shard.id)))
                {
                    return Err("invalid unit identity".into());
                }
            }
            if session.shard_id != shard.id
                || session.parameter_id != parameter_id(shard.logical_rows)?
                || session.public_params_sha256.len() != 64
                || !canonical_hash(&session.public_params_sha256)
            {
                return Err("invalid shard session reference".into());
            }
        }
        Ok(())
    }
}

pub fn canonical_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn parameters(logical_rows: u64) -> Result<ipir_sp::YpirSchemeParams, String> {
    if !matches!(logical_rows, 4096 | 8192 | 16384 | 32768) {
        return Err("unqualified logical rows".into());
    }
    ipir_sp::params_for_simplepir_profile(
        logical_rows,
        (RECORD_BYTES * RECORDS_PER_ROW * 8) as u64,
        ipir_sp::SimplePirProfile::P16Q48,
    )
    .map(|(_, p)| {
        #[cfg(feature = "native-reinspiring")]
        {
            let mut p = p;
            p.query_bits = 49;
            p.q_prime_1 = 1 << 22;
            p
        }
        #[cfg(not(feature = "native-reinspiring"))]
        {
            p
        }
    })
    .map_err(|e| e.to_string())
}

/// Native identity binds the q48 transport profile to every native packing
/// parameter, so a change to any of them changes the published identity.
#[cfg(feature = "native-reinspiring")]
#[derive(Serialize)]
struct NativeIdentity<'a> {
    q48: &'a ipir_sp::YpirSchemeParams,
    native_encoding: Vec<u8>,
    mask_bits: usize,
    query_bits: usize,
    response_bits: usize,
    cols: usize,
}

#[cfg(feature = "native-reinspiring")]
fn native_identity(q48: &ipir_sp::YpirSchemeParams, mask_bits: usize) -> String {
    use crate::native::{params, COLS, QUERY_BITS, RESPONSE_BITS};
    digest(&NativeIdentity {
        q48,
        native_encoding: params().encoding(),
        mask_bits,
        query_bits: QUERY_BITS,
        response_bits: RESPONSE_BITS,
        cols: COLS,
    })
}

fn parameter_identity(params: &ipir_sp::YpirSchemeParams) -> String {
    #[cfg(feature = "native-reinspiring")]
    {
        native_identity(params, crate::native::MASK_BITS)
    }
    #[cfg(not(feature = "native-reinspiring"))]
    {
        digest(params)
    }
}

pub fn parameter_id(logical_rows: u64) -> Result<String, String> {
    Ok(format!(
        "{PROTOCOL_REVISION}/{}",
        parameter_identity(&parameters(logical_rows)?)
    ))
}

pub fn unit_parameter_id(rows: u64) -> Result<String, String> {
    if !matches!(rows, 2048 | 4096 | 8192) {
        return Err("unqualified unit size".into());
    }
    let (_, params) = ipir_sp::params_for_simplepir_profile(
        rows,
        crate::ITEM_SIZE_BITS,
        ipir_sp::SimplePirProfile::P16Q48,
    )
    .map_err(|e| e.to_string())?;
    Ok(format!(
        "{PROTOCOL_REVISION}/unit/{}",
        parameter_identity(&params)
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardSession {
    pub session_id: String,
    pub generation: u64,
    pub shard_id: u64,
    pub params: ipir_sp::YpirSchemeParams,
    pub public_params_base64: String,
}

pub const ROW_PAYLOAD_BYTES: usize = ROW_BYTES;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_boundary_and_composition_transition_preserve_every_record() {
        let g = Geometry::default();
        let span = 32768 * RECORDS_PER_ROW as u64;
        let successor_records = 4096 * RECORDS_PER_ROW as u64;
        let mut state = Lifecycle::default();
        for n in [
            1,
            span - 1,
            span,
            span + 1,
            span + successor_records - 1,
            span + successor_records,
            span * 2 + successor_records + 17,
        ] {
            let c = state.coverage(n, g).unwrap();
            c.validate(g).unwrap();
            for p in [0, n - 1, n / 2] {
                assert!(c.locate(p).is_some());
            }
            assert!(c.locate(n).is_none());
        }
        let before = state.coverage(span - 1, g).unwrap();
        let during = state.coverage(span + 1, g).unwrap();
        let after = state.coverage(span + successor_records, g).unwrap();
        assert_eq!(
            before.locate(span - 2).unwrap().0.id,
            after.locate(span - 2).unwrap().0.id
        );
        assert_ne!(
            before.locate(span - 2).unwrap().0.id,
            during.locate(span - 2).unwrap().0.id
        );
        assert_eq!(during.shards[0].units.last().unwrap().allocated_rows, 8192);
        assert_eq!(after.shards[1].global_row_start, 32768);
        assert!(!after.shards.iter().any(|s| s.composed()));
    }

    #[test]
    fn repeated_cycles_and_reorgs_keep_identity() {
        let mut l = Lifecycle::default();
        let g = Geometry::default();
        for full_rows in (1..500_000u64).step_by(1023) {
            let c = l.coverage(full_rows * 33 + 7, g).unwrap();
            c.validate(g).unwrap();
            assert!(c.shards.iter().all(|s| s.logical_rows <= 32768));
        }
        let ids = l.identities.clone();
        l.coverage(10, g).unwrap();
        l.coverage(400_000 * 33, g).unwrap();
        assert_eq!(l.identities, ids);
        assert!(l.coverage(0, g).is_err());
        assert!(l.coverage(u64::MAX, g).is_err());
    }

    #[test]
    #[cfg(not(feature = "native-reinspiring"))]
    fn v7_parameter_identities_are_frozen() {
        assert_eq!(
            parameter_id(32768).unwrap(),
            "ironwood-enhance-pir-v7/b731a410f932c354abd6a01a05b32865d327b287e021a8d9769190d28c08290c"
        );
        assert_eq!(
            unit_parameter_id(8192).unwrap(),
            "ironwood-enhance-pir-v7/unit/943a13f970b8cd1fe917c1442bdb487aebafdedd0c79fa14554d346b85746d57"
        );
    }

    #[test]
    #[cfg(feature = "native-reinspiring")]
    fn native_parameter_identity_binds_every_native_parameter() {
        let id = parameter_id(32768).unwrap();
        assert!(id.starts_with("ironwood-enhance-pir-v9-native-two-mask-m29/"));
        let params = parameters(32768).unwrap();
        let mask_bits = crate::native::MASK_BITS;
        assert_eq!(
            id,
            format!(
                "{PROTOCOL_REVISION}/{}",
                native_identity(&params, mask_bits)
            )
        );
        // The identity is no longer the bare q48 digest, and a different mask
        // width yields a different digest.
        assert_ne!(id, format!("{PROTOCOL_REVISION}/{}", digest(&params)));
        assert_ne!(
            native_identity(&params, mask_bits),
            native_identity(&params, mask_bits + 1)
        );
        // Frozen wire identities; the independent wallet library
        // (zakura-pir-enhance, feature native-reinspiring) pins the same values.
        assert_eq!(
            id,
            "ironwood-enhance-pir-v9-native-two-mask-m29/04612c53c821f0c235e8dd44036ac43cbf605bebd8012742a3d71bda18ec13f6"
        );
        assert_eq!(
            unit_parameter_id(8192).unwrap(),
            "ironwood-enhance-pir-v9-native-two-mask-m29/unit/0ce2b7a68ec0ea3bc9cbee600eb8a300bb2f06d121a2cc9caca7f6673537e070"
        );
        assert_eq!(
            unit_parameter_id(4096).unwrap(),
            "ironwood-enhance-pir-v9-native-two-mask-m29/unit/ac16d8451fb2cb1c523ec4b6a8ad17b20ba67c58b367b2ae40ba1783ddfc29f1"
        );
        assert_eq!(
            unit_parameter_id(2048).unwrap(),
            "ironwood-enhance-pir-v9-native-two-mask-m29/unit/2e03e17451928e9498fa614db9077f27709feda2d5a45c40ef2c4f0643dc478a"
        );
    }

    #[test]
    fn adaptive_units_and_binding_reject_malformed_input() {
        let g = Geometry::default();
        for (rows, allocated) in [(1, 2048), (2048, 2048), (2049, 4096), (4097, 8192)] {
            assert_eq!(g.units(rows * 33).unwrap()[0].allocated_rows, allocated);
        }
        assert_eq!(g.units(8192 * 33 + 1).unwrap()[1].allocated_rows, 2048);
        let b = QueryBinding {
            recovery_epoch: 0,
            session_id: [7; 32],
            request_id: [8; 16],
            anchor_hash: [9; 32],
            generation: 1,
            shard_id: 7,
            epoch: [3; 8],
        };
        assert_eq!(QueryBinding::decode(&b.encode()).unwrap(), b);
        for n in 0..HEADER_BYTES {
            assert!(QueryBinding::decode(&b.encode()[..n]).is_err());
        }
        assert!(QueryBinding::decode(&[0; HEADER_BYTES]).is_err());
    }

    /// Every row a real query can select lies below its domain's query rows,
    /// every unit's data does too, and full or composed domains select over
    /// every logical row. Exhaustive over the growing domain's used rows, as
    /// shard 0 and as a successor that composes with its full predecessor.
    #[test]
    fn query_rows_cover_every_routed_row_and_unit() {
        let g = Geometry::default();
        let span = 32768 * RECORDS_PER_ROW as u64;
        for used in 1..=32768u64 {
            for records in [
                used * RECORDS_PER_ROW as u64,
                span + used * RECORDS_PER_ROW as u64,
            ] {
                let c = Lifecycle::default().coverage(records, g).unwrap();
                for shard in &c.shards {
                    let rows = shard.query_rows(g).unwrap();
                    assert_eq!(rows % QUERY_ROW_QUANTUM, 0);
                    assert!(rows <= shard.logical_rows);
                    if shard.composed() || shard.records == span {
                        assert_eq!(rows, shard.logical_rows);
                    }
                    for u in &shard.units {
                        assert!(u.local_row_start + u.used_rows <= rows);
                    }
                }
                for r in &c.routes {
                    let shard = c.shards.iter().find(|s| s.id == r.domain_id).unwrap();
                    let end = r.local_start + (r.global_end - r.global_start);
                    assert!(end <= shard.query_rows(g).unwrap());
                }
            }
        }
    }

    /// The shared vector the wallet library checks against its own copy.
    #[test]
    fn query_rows_match_the_frozen_vector() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/query-rows.json")).unwrap();
        assert_eq!(vector["quantum"], QUERY_ROW_QUANTUM);
        let g = Geometry::default();
        for case in vector["cases"].as_array().unwrap() {
            let id = case["shard_id"].as_u64().unwrap();
            let shard = QueryShard {
                id,
                global_row_start: id * g.max_shard_rows,
                records: case["records"].as_u64().unwrap(),
                logical_rows: 0,
                state: ShardState::Growing,
                units: Vec::new(),
            };
            assert_eq!(
                shard.expected_logical_rows(g).unwrap(),
                case["logical_rows"],
                "{case}"
            );
            assert_eq!(shard.query_rows(g).unwrap(), case["query_rows"], "{case}");
        }
    }
}
