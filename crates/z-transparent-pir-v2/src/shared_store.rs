use crate::memory_store::MemoryStore;
use crate::store::*;
use bincode::Options;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
pub(crate) const MAX_RETAINED_EVENTS: usize = 65_536;
pub(crate) const MAX_STORE_BYTES: usize = 63 * 1024 * 1024;
pub(crate) const MAX_TERMINAL_STORE_BYTES: usize = MAX_STORE_BYTES + 512 * 1024;
pub(crate) const MAX_CONTINUATION_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone)]
pub(crate) struct SharedStore {
    pub inner: Rc<RefCell<MemoryStore>>,
    pub rewind: Rc<RefCell<Option<Anchor>>>,
    pub policy_limit: Rc<Cell<bool>>,
}
impl SharedStore {
    // A refused growth mutation leaves the complete journal, including owed
    // cover work, unchanged. Only typed local caps set the terminal flag.
    fn update<T>(
        &mut self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.update_with_cover_reserve(false, f)
    }
    fn update_with_cover_reserve<T>(
        &mut self,
        cover_failure: bool,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut candidate = self.inner.borrow().clone();
        let result = f(&mut candidate).and_then(|value| {
            let bytes = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .serialized_size(&candidate)
                .map_err(|_| StoreError::Corrupt("store encoding".into()))?;
            if bytes > MAX_STORE_BYTES as u64 {
                if !cover_failure || bytes > MAX_TERMINAL_STORE_BYTES as u64 {
                    return Err(StoreError::HistoryMetadataLimit);
                }
                // The failed reply has already created this obligation. Preserve
                // it in reserved space, then stop before returning another IO.
                self.policy_limit.set(true);
            }
            *self.inner.borrow_mut() = candidate;
            Ok(value)
        });
        if matches!(result, Err(StoreError::HistoryMetadataLimit)) {
            self.policy_limit.set(true);
        }
        result
    }
    fn cover_failure(&self, commit: &ShardCommit) -> Result<bool, StoreError> {
        let [pending] = commit.pending_upsert.as_slice() else {
            return Ok(false);
        };
        if commit.failed_entries.as_slice() != [pending.script.clone()]
            || pending.script.len() > 40
            || pending.dummy_debt.is_none()
            || !commit.events.is_empty()
            || !commit.covered_scripts.is_empty()
            || !commit.pending_complete.is_empty()
            || !commit.cached_runs.is_empty()
            || pending.shard_id != commit.shard_id
            || pending.revision_digest != commit.revision_digest
        {
            return Ok(false);
        }
        let Some(mut prior) = self
            .inner
            .borrow()
            .pending()?
            .into_iter()
            .find(|held| held.id.is_some() && held.id == pending.id)
        else {
            return Ok(false);
        };
        prior.dummy_debt = pending.dummy_debt;
        Ok(prior == *pending)
    }
}
impl WalletStore for SharedStore {
    fn set_identity(&self) -> Result<Option<SetIdentity>, StoreError> {
        self.inner.borrow().set_identity()
    }
    fn bind_set(&mut self, identity: &SetIdentity) -> Result<(), StoreError> {
        self.update(|state| state.bind_set(identity))
    }
    fn anchor(&self) -> Result<Option<Anchor>, StoreError> {
        self.inner.borrow().anchor()
    }
    fn scripts(&self) -> Result<Vec<ScriptEntry>, StoreError> {
        self.inner.borrow().scripts()
    }
    fn add_scripts(&mut self, entries: &[ScriptEntry]) -> Result<usize, StoreError> {
        self.update(|state| state.add_scripts(entries))
    }
    fn coverage(&self, script: &[u8]) -> Result<Vec<CoverageRange>, StoreError> {
        self.inner.borrow().coverage(script)
    }
    fn provisional(&self) -> Result<Vec<CoverageRange>, StoreError> {
        self.inner.borrow().provisional()
    }
    fn events(&self) -> Result<Vec<StoredEvent>, StoreError> {
        self.inner.borrow().events()
    }
    fn commit_shard(&mut self, commit: ShardCommit) -> Result<u64, StoreError> {
        let cover_failure = self.cover_failure(&commit)?;
        self.update_with_cover_reserve(cover_failure, |state| state.commit_shard(commit))
    }
    fn commit_anchor(
        &mut self,
        anchor: &Anchor,
        settled_through: u64,
        covered_through: u64,
    ) -> Result<u64, StoreError> {
        self.update(|state| state.commit_anchor(anchor, settled_through, covered_through))
    }
    fn rollback_above(&mut self, anchor: &Anchor, reason: &str) -> Result<u64, StoreError> {
        let result = self.update(|state| state.rollback_above(anchor, reason))?;
        let mut rewind = self.rewind.borrow_mut();
        if rewind
            .as_ref()
            .is_none_or(|held| anchor.height < held.height)
        {
            *rewind = Some(anchor.clone());
        }
        Ok(result)
    }
    fn promote_provisional(
        &mut self,
        shard_id: u64,
        revision_digest: &str,
    ) -> Result<(), StoreError> {
        self.inner
            .borrow_mut()
            .promote_provisional(shard_id, revision_digest)
    }
    fn pending(&self) -> Result<Vec<PendingPages>, StoreError> {
        self.inner.borrow().pending()
    }
    fn pending_limit(&self) -> usize {
        self.inner.borrow().pending_limit()
    }
    fn cached_run(
        &self,
        child_digest: &str,
        script: &[u8],
    ) -> Result<Option<CachedRun>, StoreError> {
        self.inner.borrow().cached_run(child_digest, script)
    }
    fn setup(&self, key: &SetupKey) -> Result<Option<SetupBlob>, StoreError> {
        self.inner.borrow().setup(key)
    }
    fn put_setup(&mut self, key: &SetupKey, blob: &SetupBlob) -> Result<(), StoreError> {
        self.update(|state| state.put_setup(key, blob))
    }
    fn drop_setup(&mut self, key: &SetupKey) -> Result<(), StoreError> {
        self.inner.borrow_mut().drop_setup(key)
    }
    fn filter(
        &self,
        revision_digest: &str,
        filter_hash: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.borrow().filter(revision_digest, filter_hash)
    }
    fn put_filter(
        &mut self,
        revision_digest: &str,
        filter_hash: &str,
        sealed: bool,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        self.update(|state| state.put_filter(revision_digest, filter_hash, sealed, bytes))
    }
    fn last_commit(&self) -> Result<u64, StoreError> {
        self.inner.borrow().last_commit()
    }
    fn entry_failures(&self, shard_id: u64, script: &[u8]) -> Result<u32, StoreError> {
        self.inner.borrow().entry_failures(shard_id, script)
    }
}
