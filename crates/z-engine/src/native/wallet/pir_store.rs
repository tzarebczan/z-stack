//! Bounded qualification journal. The pinned upstream store defines semantics;
//! each accepted operation commits through the wallet's extension transaction.
use super::super::RecoveryCancellation;
use super::pir::{
    native_scope_in_transaction, NativeScanSnapshot, NativeScope, PirDiscoveryReport,
};
use super::*;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use transparent_events::TransparentEvent;
use transparent_wallet::{client::Table, *};

const MAX_ENTRIES: u64 = 16_384;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OPERATION_BYTES: usize = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Event {
    script: Vec<u8>,
    bytes: Vec<u8>,
    shard_id: u64,
    revision: String,
}
impl From<StoredEvent> for Event {
    fn from(e: StoredEvent) -> Self {
        Self {
            script: e.script,
            bytes: e.event.to_bytes(),
            shard_id: e.shard_id,
            revision: e.revision_digest,
        }
    }
}
impl Event {
    fn decode(self) -> std::result::Result<StoredEvent, StoreError> {
        Ok(StoredEvent {
            script: self.script,
            event: TransparentEvent::from_bytes(&self.bytes).map_err(|_| corrupt())?,
            shard_id: self.shard_id,
            revision_digest: self.revision,
        })
    }
}
#[derive(Serialize, Deserialize)]
struct Pending {
    id: Option<u64>,
    shard_id: u64,
    revision: String,
    script: Vec<u8>,
    first_page: u32,
    page_count: u32,
    inline: Vec<Vec<u8>>,
    next: u32,
    attempts: u32,
    validated: u32,
    boundary: Option<(u32, Vec<u8>)>,
    target: Option<Anchor>,
}
impl From<PendingPages> for Pending {
    fn from(p: PendingPages) -> Self {
        Self {
            id: p.id,
            shard_id: p.shard_id,
            revision: p.revision_digest,
            script: p.script,
            first_page: p.first_page,
            page_count: p.page_count,
            inline: p.inline.into_iter().map(|e| e.to_bytes()).collect(),
            next: p.next_ordinal,
            attempts: p.attempts,
            validated: p.validated_events,
            boundary: p.boundary.map(|b| (b.event_bytes, b.last_event.to_bytes())),
            target: p.target_anchor,
        }
    }
}
impl Pending {
    fn decode(self) -> std::result::Result<PendingPages, StoreError> {
        let decode = |b: Vec<u8>| TransparentEvent::from_bytes(&b).map_err(|_| corrupt());
        Ok(PendingPages {
            id: self.id,
            shard_id: self.shard_id,
            revision_digest: self.revision,
            script: self.script,
            first_page: self.first_page,
            page_count: self.page_count,
            inline: self
                .inline
                .into_iter()
                .map(decode)
                .collect::<std::result::Result<_, _>>()?,
            next_ordinal: self.next,
            attempts: self.attempts,
            validated_events: self.validated,
            boundary: self
                .boundary
                .map(|(event_bytes, b)| {
                    decode(b).map(|last_event| transparent_wallet::store::PageBoundary {
                        event_bytes,
                        last_event,
                    })
                })
                .transpose()?,
            target_anchor: self.target,
        })
    }
}
#[derive(Serialize, Deserialize)]
struct Shard {
    source: Option<Anchor>,
    id: u64,
    revision: String,
    sealed: bool,
    start: u64,
    end: u64,
    hash: String,
    events: Vec<Event>,
    scripts: Vec<Vec<u8>>,
    pending: Vec<Pending>,
    complete: Vec<u64>,
}
impl From<ShardCommit> for Shard {
    fn from(c: ShardCommit) -> Self {
        Self {
            source: c.source_anchor,
            id: c.shard_id,
            revision: c.revision_digest,
            sealed: c.sealed,
            start: c.start_height,
            end: c.end_height,
            hash: c.terminal_block_hash,
            events: c.events.into_iter().map(Event::from).collect(),
            scripts: c.covered_scripts,
            pending: c.pending_upsert.into_iter().map(Pending::from).collect(),
            complete: c.pending_complete,
        }
    }
}
impl Shard {
    fn decode(self) -> std::result::Result<ShardCommit, StoreError> {
        Ok(ShardCommit {
            source_anchor: self.source,
            shard_id: self.id,
            revision_digest: self.revision,
            sealed: self.sealed,
            start_height: self.start,
            end_height: self.end,
            terminal_block_hash: self.hash,
            events: self
                .events
                .into_iter()
                .map(Event::decode)
                .collect::<std::result::Result<_, _>>()?,
            covered_scripts: self.scripts,
            pending_upsert: self
                .pending
                .into_iter()
                .map(Pending::decode)
                .collect::<std::result::Result<_, _>>()?,
            pending_complete: self.complete,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Operation {
    Bind(SetIdentity),
    Scripts(Vec<ScriptEntry>),
    Shard(Shard),
    Anchor(Anchor, u64, u64),
    Rollback(Anchor, String),
    NativeRewind(Anchor),
    Promote(u64, String),
    Setup(String, String, bool, u32, SetupBlob),
    Filter(String, String, bool, Vec<u8>),
    Report(PirDiscoveryReport, NativeScope),
    Enroll(NativeScope),
}
#[derive(Default)]
struct Replay {
    memory: MemoryStore,
    report: Option<(PirDiscoveryReport, NativeScope)>,
    enrolled: Option<NativeScope>,
}
impl Replay {
    fn apply(&mut self, op: Operation) -> std::result::Result<u64, StoreError> {
        let result = match op {
            Operation::Bind(i) => {
                self.memory.bind_set(&i)?;
                0
            }
            Operation::Scripts(s) => self.memory.add_scripts(&s)? as u64,
            Operation::Shard(s) => self.memory.commit_shard(s.decode()?)?,
            Operation::Anchor(a, s, c) => self.memory.commit_anchor(&a, s, c)?,
            Operation::Rollback(a, r) => self.memory.rollback_above(&a, &r)?,
            Operation::NativeRewind(a) => self
                .memory
                .rollback_above(&a, "native accepted-chain rewind")?,
            Operation::Promote(i, r) => {
                self.memory.promote_provisional(i, &r)?;
                0
            }
            Operation::Setup(set_digest, revision_digest, directory, segment, b) => {
                self.memory.put_setup(
                    &SetupKey {
                        set_digest,
                        revision_digest,
                        table: if directory {
                            Table::Directory
                        } else {
                            Table::Pages
                        },
                        segment,
                    },
                    &b,
                )?;
                0
            }
            Operation::Filter(r, h, s, b) => {
                self.memory.put_filter(&r, &h, s, &b)?;
                0
            }
            Operation::Enroll(scope) => {
                self.enrolled = Some(scope);
                0
            }
            Operation::Report(report, scope) => {
                self.report = Some((report, scope));
                return Ok(0);
            }
        };
        self.report = None;
        Ok(result)
    }
}
fn journal_digest(prior: &str, payload: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(prior.as_bytes());
    h.update(payload);
    format!("{:x}", h.finalize())
}
fn corrupt() -> StoreError {
    StoreError::Corrupt("native_pir_journal_invalid".into())
}
fn io(_: rusqlite::Error) -> StoreError {
    StoreError::Io("native_pir_database_failed".into())
}
impl From<rusqlite::Error> for JournalError {
    fn from(_: rusqlite::Error) -> Self {
        Self(io(rusqlite::Error::InvalidQuery))
    }
}
struct JournalError(StoreError);

pub(super) struct NativePirStore<'a> {
    db: SyncDb,
    state: Replay,
    generation: u64,
    bytes: u64,
    digest: String,
    cancel: &'a AtomicBool,
    recovery_cancel: Option<&'a RecoveryCancellation>,
    scope: Option<NativeScope>,
    publication_start: u64,
    // Fields drop in declaration order: close SQLite before releasing its lease.
    _database_lease: std::fs::File,
}
impl<'a> NativePirStore<'a> {
    pub(super) fn open(
        wallet: &NativeWallet,
        cancel: &'a AtomicBool,
    ) -> std::result::Result<Self, StoreError> {
        Self::open_with_commit_gate(wallet, cancel, None)
    }
    pub(super) fn open_recovery(
        wallet: &NativeWallet,
        cancel: &'a RecoveryCancellation,
    ) -> std::result::Result<Self, StoreError> {
        Self::open_with_commit_gate(wallet, cancel.flag(), Some(cancel))
    }
    pub(super) fn open_with_commit_gate(
        wallet: &NativeWallet,
        cancel: &'a AtomicBool,
        recovery_cancel: Option<&'a RecoveryCancellation>,
    ) -> std::result::Result<Self, StoreError> {
        let _gate = recovery_cancel
            .map(|token| token.commit_guard())
            .transpose()
            .map_err(|_| StoreError::Io("native_pir_cancelled".into()))?;
        let database_lease = database_lease::shared(&wallet.paths.data_db)
            .map_err(|_| StoreError::Io("native_pir_database_busy".into()))?;
        // Initialize/migrate the native wallet before introducing extension schema.
        drop(
            wallet
                .open_db()
                .map_err(|_| io(rusqlite::Error::InvalidQuery))?,
        );
        let mut conn = Connection::open(&wallet.paths.data_db).map_err(io)?;
        rusqlite::vtab::array::load_module(&conn).map_err(io)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(io)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(io)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(io)?;
        conn.pragma_update(None, "foreign_keys", true).map_err(io)?;
        let tx = conn.transaction().map_err(io)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS ext_coffer_pir_schema (version INTEGER PRIMARY KEY CHECK(version=1)); CREATE TABLE IF NOT EXISTS ext_coffer_pir_journal (id INTEGER PRIMARY KEY, payload BLOB NOT NULL); CREATE TABLE IF NOT EXISTS ext_coffer_pir_head (id INTEGER PRIMARY KEY CHECK(id=1), generation INTEGER NOT NULL, bytes INTEGER NOT NULL, digest TEXT NOT NULL);").map_err(io)?;
        let version: Option<u32> = tx
            .query_row("SELECT version FROM ext_coffer_pir_schema", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(io)?;
        if version.is_some_and(|v| v != 1) {
            return Err(corrupt());
        }
        if version.is_none() {
            let existing: u64 = tx
                .query_row("SELECT COUNT(*) FROM ext_coffer_pir_journal", [], |r| {
                    r.get(0)
                })
                .map_err(io)?;
            if existing != 0 {
                return Err(corrupt());
            }
            tx.execute("INSERT INTO ext_coffer_pir_schema(version) VALUES(1)", [])
                .map_err(io)?;
            tx.execute(
                "INSERT INTO ext_coffer_pir_head(id,generation,bytes,digest) VALUES(1,0,0,'')",
                [],
            )
            .map_err(io)?;
        }
        tx.commit().map_err(io)?;
        let read = conn.transaction().map_err(io)?;
        let mut state = Replay::default();
        let mut generation = 0;
        let mut bytes = 0;
        let mut digest = String::new();
        {
            let mut stmt = read
                .prepare(
                    "SELECT id,length(payload),payload FROM ext_coffer_pir_journal ORDER BY id",
                )
                .map_err(io)?;
            let mut rows = stmt.query([]).map_err(io)?;
            while let Some(row) = rows.next().map_err(io)? {
                let id: u64 = row.get(0).map_err(io)?;
                let size: u64 = row.get(1).map_err(io)?;
                if size > MAX_OPERATION_BYTES as u64 {
                    return Err(corrupt());
                }
                let payload: Vec<u8> = row.get(2).map_err(io)?;
                bytes += payload.len() as u64;
                if id != generation + 1
                    || id > MAX_ENTRIES
                    || bytes > MAX_BYTES
                    || payload.len() > MAX_OPERATION_BYTES
                {
                    return Err(corrupt());
                }
                let op: Operation = serde_json::from_slice(&payload).map_err(|_| corrupt())?;
                state.apply(op).map_err(|_| corrupt())?;
                generation = id;
                digest = journal_digest(&digest, &payload);
            }
        }
        let recorded: (u64, u64, String) = read
            .query_row(
                "SELECT generation,bytes,digest FROM ext_coffer_pir_head WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|_| corrupt())?;
        if recorded != (generation, bytes, digest.clone()) {
            return Err(corrupt());
        }
        read.commit().map_err(io)?;
        let db = WalletDb::from_connection(conn, wallet.network, SystemClock, new_rng());
        Ok(Self {
            db,
            state,
            generation,
            bytes,
            digest,
            cancel,
            recovery_cancel,
            scope: None,
            publication_start: 0,
            _database_lease: database_lease,
        })
    }
    pub(super) fn enroll(
        &mut self,
        scope: NativeScope,
        start: u64,
    ) -> std::result::Result<(), StoreError> {
        if self
            .state
            .enrolled
            .as_ref()
            .is_some_and(|old| !scope.continues(old))
        {
            return Err(StoreError::Io("native_pir_scope_changed".into()));
        }
        self.scope = Some(scope.clone());
        self.publication_start = start;
        self.write(Operation::Enroll(scope)).map(|_| ())
    }
    pub(super) fn report(&self) -> Option<&(PirDiscoveryReport, NativeScope)> {
        self.state.report.as_ref()
    }
    pub(super) fn persist_report(
        &mut self,
        report: PirDiscoveryReport,
        expected: &NativeScanSnapshot,
    ) -> std::result::Result<(), StoreError> {
        if report.native_scanner_identity != Some(expected.identity()?)
            || !report
                .accepted_anchor
                .as_ref()
                .is_some_and(|anchor| expected.matches_anchor(anchor))
        {
            return Err(StoreError::Io("native_pir_reconciliation_required".into()));
        }
        self.write_at(
            Operation::Report(report, self.scope.clone().ok_or_else(corrupt)?),
            Some(expected),
        )
        .map(|_| ())
    }
    pub(super) fn rewind_native(
        &mut self,
        anchor: Anchor,
        expected: &NativeScanSnapshot,
    ) -> std::result::Result<u32, StoreError> {
        self.write_at(Operation::NativeRewind(anchor.clone()), Some(expected))?;
        Ok(anchor.height as u32)
    }
    fn write(&mut self, op: Operation) -> std::result::Result<u64, StoreError> {
        self.write_at(op, None)
    }
    fn write_at(
        &mut self,
        op: Operation,
        expected_snapshot: Option<&NativeScanSnapshot>,
    ) -> std::result::Result<u64, StoreError> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(StoreError::Io("native_pir_cancelled".into()));
        }
        let native_rewind = match &op {
            Operation::NativeRewind(anchor) => Some(anchor.clone()),
            _ => None,
        };
        let payload = serde_json::to_vec(&op).map_err(|_| corrupt())?;
        if self.generation >= MAX_ENTRIES
            || self.bytes + payload.len() as u64 > MAX_BYTES
            || payload.len() > MAX_OPERATION_BYTES
        {
            return Err(StoreError::Io("native_pir_qualification_limit".into()));
        }
        let mut next = Replay {
            memory: self.state.memory.clone(),
            report: self.state.report.clone(),
            enrolled: self.state.enrolled.clone(),
        };
        let result = next.apply(op)?;
        let generation = self.generation;
        let bytes = self.bytes + payload.len() as u64;
        let digest = journal_digest(&self.digest, &payload);
        let prior_digest = &self.digest;
        let scope = &self.scope;
        let cancel = self.cancel;
        let start = self.publication_start;
        let _gate = self
            .recovery_cancel
            .map(|token| token.commit_guard())
            .transpose()
            .map_err(|_| StoreError::Io("native_pir_cancelled".into()))?;
        self.db
            .transactionally_with_extension(|wdb, ext| -> std::result::Result<(), JournalError> {
                let current: (u64, String) = ext.query_row(
                    "SELECT generation,digest FROM ext_coffer_pir_head WHERE id=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if current != (generation, prior_digest.clone()) {
                    return Err(JournalError(StoreError::Io(
                        "native_pir_generation_changed".into(),
                    )));
                }
                if cancel.load(Ordering::Acquire) {
                    return Err(JournalError(StoreError::Io("native_pir_cancelled".into())));
                }
                if let Some(expected) = scope {
                    let actual = native_scope_in_transaction(wdb, ext, start).map_err(|_| {
                        JournalError(StoreError::Io("native_pir_scope_invalid".into()))
                    })?;
                    if &actual != expected {
                        return Err(JournalError(StoreError::Io(
                            "native_pir_scope_changed".into(),
                        )));
                    }
                }
                if let Some(expected) = expected_snapshot {
                    if NativeScanSnapshot::capture(wdb, expected.anchor_height)
                        .map_err(JournalError)?
                        != *expected
                    {
                        return Err(JournalError(StoreError::Io(
                            "native_pir_reconciliation_required".into(),
                        )));
                    }
                }
                if let Some(anchor)=&native_rewind {
                    let actual=wdb.truncate_to_height((anchor.height as u32).into()).map_err(|_|JournalError(StoreError::Io("native_recovery_rewind_failed".into())))?;
                    if u64::from(u32::from(actual))!=anchor.height {return Err(JournalError(StoreError::Io("native_recovery_rewind_checkpoint_required".into())));}
                    ext.execute("INSERT INTO ext_coffer_pir_recovery_v1 VALUES(1,1,1,NULL) ON CONFLICT(id) DO UPDATE SET blocked=1,generation=generation+1,receipt=NULL",[])?;
                }
                ext.execute(
                    "INSERT INTO ext_coffer_pir_journal(id,payload) VALUES(?1,?2)",
                    (generation + 1, &payload),
                )?;
                ext.execute(
                    "UPDATE ext_coffer_pir_head SET generation=?1,bytes=?2,digest=?3 WHERE id=1",
                    (generation + 1, bytes, &digest),
                )?;
                if cancel.load(Ordering::Acquire) {
                    return Err(JournalError(StoreError::Io("native_pir_cancelled".into())));
                }
                Ok(())
            })
            .map_err(|e| e.0)?;
        self.state = next;
        self.generation += 1;
        self.bytes = bytes;
        self.digest = digest;
        Ok(result)
    }
}
impl WalletStore for NativePirStore<'_> {
    fn set_identity(&self) -> std::result::Result<Option<SetIdentity>, StoreError> {
        self.state.memory.set_identity()
    }
    fn bind_set(&mut self, i: &SetIdentity) -> std::result::Result<(), StoreError> {
        self.write(Operation::Bind(i.clone())).map(|_| ())
    }
    fn anchor(&self) -> std::result::Result<Option<Anchor>, StoreError> {
        self.state.memory.anchor()
    }
    fn scripts(&self) -> std::result::Result<Vec<ScriptEntry>, StoreError> {
        self.state.memory.scripts()
    }
    fn add_scripts(&mut self, s: &[ScriptEntry]) -> std::result::Result<usize, StoreError> {
        self.write(Operation::Scripts(s.to_vec()))
            .map(|n| n as usize)
    }
    fn coverage(&self, s: &[u8]) -> std::result::Result<Vec<CoverageRange>, StoreError> {
        self.state.memory.coverage(s)
    }
    fn provisional(&self) -> std::result::Result<Vec<CoverageRange>, StoreError> {
        self.state.memory.provisional()
    }
    fn events(&self) -> std::result::Result<Vec<StoredEvent>, StoreError> {
        self.state.memory.events()
    }
    fn commit_shard(&mut self, c: ShardCommit) -> std::result::Result<u64, StoreError> {
        self.write(Operation::Shard(c.into()))
    }
    fn commit_anchor(
        &mut self,
        a: &Anchor,
        s: u64,
        c: u64,
    ) -> std::result::Result<u64, StoreError> {
        self.write(Operation::Anchor(a.clone(), s, c))
    }
    fn rollback_above(&mut self, a: &Anchor, r: &str) -> std::result::Result<u64, StoreError> {
        self.write(Operation::Rollback(a.clone(), r.into()))
    }
    fn promote_provisional(&mut self, i: u64, r: &str) -> std::result::Result<(), StoreError> {
        self.write(Operation::Promote(i, r.into())).map(|_| ())
    }
    fn pending(&self) -> std::result::Result<Vec<PendingPages>, StoreError> {
        self.state.memory.pending()
    }
    fn pending_limit(&self) -> usize {
        self.state.memory.pending_limit()
    }
    fn setup(&self, k: &SetupKey) -> std::result::Result<Option<SetupBlob>, StoreError> {
        self.state.memory.setup(k)
    }
    fn put_setup(&mut self, k: &SetupKey, b: &SetupBlob) -> std::result::Result<(), StoreError> {
        self.write(Operation::Setup(
            k.set_digest.clone(),
            k.revision_digest.clone(),
            k.table == Table::Directory,
            k.segment,
            b.clone(),
        ))
        .map(|_| ())
    }
    fn filter(&self, r: &str, h: &str) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        self.state.memory.filter(r, h)
    }
    fn put_filter(
        &mut self,
        r: &str,
        h: &str,
        s: bool,
        b: &[u8],
    ) -> std::result::Result<(), StoreError> {
        self.write(Operation::Filter(r.into(), h.into(), s, b.to_vec()))
            .map(|_| ())
    }
    fn last_commit(&self) -> std::result::Result<u64, StoreError> {
        self.state.memory.last_commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::wallet::tests::{fixture_account, fixture_wallet};
    use transparent_wallet::testing;
    static LIVE: AtomicBool = AtomicBool::new(false);
    fn fixture() -> (tempfile::TempDir, NativeWallet) {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (key, birthday) = fixture_account();
        wallet.replace_scan_db(&key, &birthday).unwrap();
        (dir, wallet)
    }
    fn commit() -> ShardCommit {
        ShardCommit {
            shard_id: 0,
            revision_digest: "r0".into(),
            sealed: true,
            start_height: 1,
            end_height: 100,
            terminal_block_hash: "11".repeat(32),
            events: vec![testing::receive(1, 0, 10_000, 10)],
            covered_scripts: vec![testing::script(1)],
            ..Default::default()
        }
    }
    #[test]
    fn upstream_contract() {
        let keep = std::cell::RefCell::new(Vec::new());
        testing::suite(|| {
            let (dir, wallet) = fixture();
            let store = NativePirStore::open(&wallet, &LIVE).unwrap();
            keep.borrow_mut().push(dir);
            store
        });
    }
    #[test]
    fn durable_reopen_rollback_and_contradiction_preserve_native_balance() {
        let (_dir, wallet) = fixture();
        let before = wallet
            .balance()
            .map(|b| b.transparent_available)
            .map_err(|e| e.to_string());
        let mut store = NativePirStore::open(&wallet, &LIVE).unwrap();
        store.commit_shard(commit()).unwrap();
        let event_bytes = store.events().unwrap();
        let generation = store.generation;
        let mut different = commit();
        different.events[0] = testing::receive(1, 0, 11_000, 10);
        assert!(store.commit_shard(different).is_err());
        assert_eq!(store.events().unwrap(), event_bytes);
        assert_eq!(store.generation, generation);
        drop(store);
        let mut store = NativePirStore::open(&wallet, &LIVE).unwrap();
        assert_eq!(store.ledger().unwrap().confirmed_balance(), 10_000);
        store
            .rollback_above(
                &Anchor {
                    height: 5,
                    hash: "22".repeat(32),
                },
                "local reorg",
            )
            .unwrap();
        drop(store);
        let store = NativePirStore::open(&wallet, &LIVE).unwrap();
        assert!(store.events().unwrap().is_empty());
        assert_eq!(
            wallet
                .balance()
                .map(|b| b.transparent_available)
                .map_err(|e| e.to_string()),
            before
        );
        assert!(wallet.history(100).unwrap().is_empty());
    }
    #[test]
    fn failed_sql_commit_keeps_memory_and_disk_unchanged() {
        let (_dir, wallet) = fixture();
        let mut store = NativePirStore::open(&wallet, &LIVE).unwrap();
        let conn = Connection::open(&wallet.paths.data_db).unwrap();
        conn.execute_batch("CREATE TRIGGER ext_coffer_pir_fail BEFORE INSERT ON ext_coffer_pir_journal BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(store.commit_shard(commit()).is_err());
        assert_eq!(store.generation, 0);
        assert!(store.events().unwrap().is_empty());
        conn.execute_batch("DROP TRIGGER ext_coffer_pir_fail")
            .unwrap();
        store.commit_shard(commit()).unwrap();
        drop(store);
        assert_eq!(
            NativePirStore::open(&wallet, &LIVE)
                .unwrap()
                .ledger()
                .unwrap()
                .confirmed_balance(),
            10_000
        );
    }
    #[test]
    fn recovery_journal_writes_wait_for_the_cancellation_commit_gate() {
        let (_dir, wallet) = fixture();
        let token = RecoveryCancellation::new();
        let mut store = NativePirStore::open_recovery(&wallet, &token).unwrap();
        let gate = token.commit_guard().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (written_tx, written_rx) = std::sync::mpsc::channel();
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        std::thread::scope(|threads| {
            threads.spawn(move || {
                started_tx.send(()).unwrap();
                written_tx
                    .send(store.commit_shard(commit()).is_err())
                    .unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(
                written_rx.recv_timeout(Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            threads.spawn(|| {
                token.cancel();
                cancelled_tx.send(()).unwrap();
            });
            while !token.is_cancelled() {
                std::thread::yield_now();
            }
            assert!(matches!(
                cancelled_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            drop(gate);
            cancelled_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(written_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        });
        let conn = Connection::open(&wallet.paths.data_db).unwrap();
        let generation: u64 = conn
            .query_row(
                "SELECT generation FROM ext_coffer_pir_head WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(generation, 0);
    }
    #[test]
    fn cancelled_recovery_never_initializes_a_discovery_journal() {
        let (_dir, wallet) = fixture();
        let token = RecoveryCancellation::new();
        token.cancel();
        assert!(NativePirStore::open_recovery(&wallet, &token).is_err());
        let conn = Connection::open(&wallet.paths.data_db).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='ext_coffer_pir_head')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
    }
    #[test]
    fn stale_writer_and_cancel_do_not_advance_coverage() {
        let (_dir, wallet) = fixture();
        let mut first = NativePirStore::open(&wallet, &LIVE).unwrap();
        let mut second = NativePirStore::open(&wallet, &LIVE).unwrap();
        first.commit_shard(commit()).unwrap();
        assert!(second.commit_shard(commit()).is_err());
        assert!(second.events().unwrap().is_empty());
        let cancel = AtomicBool::new(true);
        let mut cancelled = NativePirStore::open(&wallet, &cancel).unwrap();
        let generation = cancelled.generation;
        assert!(cancelled.commit_shard(commit()).is_err());
        assert_eq!(cancelled.generation, generation);
    }
    #[test]
    fn concurrent_reopen_observes_one_committed_journal_snapshot() {
        let (_dir, wallet) = fixture();
        NativePirStore::open(&wallet, &LIVE).unwrap();
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let mut store = NativePirStore::open(&wallet, &LIVE).unwrap();
                for _ in 0..200 {
                    store.bind_set(&testing::identity()).unwrap();
                }
            });
            for _ in 0..40 {
                NativePirStore::open(&wallet, &LIVE).unwrap();
            }
        });
        assert_eq!(
            NativePirStore::open(&wallet, &LIVE).unwrap().generation,
            200
        );
    }
    #[test]
    fn corrupt_truncated_and_unknown_version_journals_fail_closed() {
        for sql in [
            "UPDATE ext_coffer_pir_journal SET payload=X'ff' WHERE id=1",
            "DELETE FROM ext_coffer_pir_journal WHERE id=1",
            "DELETE FROM ext_coffer_pir_head",
            "PRAGMA ignore_check_constraints=ON; UPDATE ext_coffer_pir_schema SET version=99",
        ] {
            let (_dir, wallet) = fixture();
            let mut store = NativePirStore::open(&wallet, &LIVE).unwrap();
            store.commit_shard(commit()).unwrap();
            drop(store);
            let conn = Connection::open(&wallet.paths.data_db).unwrap();
            conn.execute_batch(sql).unwrap();
            assert!(NativePirStore::open(&wallet, &LIVE).is_err());
        }
    }
}
