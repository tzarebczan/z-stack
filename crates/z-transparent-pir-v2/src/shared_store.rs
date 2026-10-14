use crate::memory_store::MemoryStore;
use crate::store::*;
use std::{cell::RefCell, rc::Rc};
#[derive(Clone)]
pub(crate) struct SharedStore {
    pub inner: Rc<RefCell<MemoryStore>>,
    pub rewind: Rc<RefCell<Option<Anchor>>>,
}
impl WalletStore for SharedStore {
    fn set_identity(&self) -> Result<Option<SetIdentity>, StoreError> {
        self.inner.borrow().set_identity()
    }
    fn bind_set(&mut self, identity: &SetIdentity) -> Result<(), StoreError> {
        self.inner.borrow_mut().bind_set(identity)
    }
    fn anchor(&self) -> Result<Option<Anchor>, StoreError> {
        self.inner.borrow().anchor()
    }
    fn scripts(&self) -> Result<Vec<ScriptEntry>, StoreError> {
        self.inner.borrow().scripts()
    }
    fn add_scripts(&mut self, entries: &[ScriptEntry]) -> Result<usize, StoreError> {
        self.inner.borrow_mut().add_scripts(entries)
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
        self.inner.borrow_mut().commit_shard(commit)
    }
    fn commit_anchor(
        &mut self,
        anchor: &Anchor,
        settled_through: u64,
        covered_through: u64,
    ) -> Result<u64, StoreError> {
        self.inner
            .borrow_mut()
            .commit_anchor(anchor, settled_through, covered_through)
    }
    fn rollback_above(&mut self, anchor: &Anchor, reason: &str) -> Result<u64, StoreError> {
        let result = self.inner.borrow_mut().rollback_above(anchor, reason)?;
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
        self.inner.borrow_mut().put_setup(key, blob)
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
        self.inner
            .borrow_mut()
            .put_filter(revision_digest, filter_hash, sealed, bytes)
    }
    fn last_commit(&self) -> Result<u64, StoreError> {
        self.inner.borrow().last_commit()
    }
    fn entry_failures(&self, shard_id: u64, script: &[u8]) -> Result<u32, StoreError> {
        self.inner.borrow().entry_failures(shard_id, script)
    }
}
