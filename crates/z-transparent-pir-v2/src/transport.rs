use crate::client::Table;
use crate::store::WalletStore;
pub use transparent_wallet::transport::{
    BoxError, ByteCharges, Overloaded, ShardReply, ShardRequest, StaleRevision,
};
pub(crate) fn describe_error(e: &(dyn std::error::Error + 'static)) -> String {
    e.to_string()
}
pub trait FilterSource {
    fn uses_parents(&self) -> bool {
        false
    }
    async fn shard_map(&mut self) -> Result<(Vec<u8>, u64), BoxError>;
    async fn filter(&mut self, id: u64) -> Result<(Vec<u8>, u64), BoxError>;
    fn prefetch(&mut self, _ids: &[u64]) {}
    async fn prepare_parents(
        &mut self,
        _map: &transparent_filter::ShardMap,
        _ids: &[u64],
        _s: &mut dyn WalletStore,
    ) -> Result<u64, BoxError> {
        Ok(0)
    }
    async fn parent_negative(
        &mut self,
        _map: &transparent_filter::ShardMap,
        _id: u64,
        _scripts: &[Vec<u8>],
        _s: &mut dyn WalletStore,
    ) -> Result<(bool, u64), BoxError> {
        Ok((false, 0))
    }
}
pub trait ShardTransport {
    async fn init(&mut self) -> Result<(Vec<u8>, u64), BoxError>;
    async fn manifest(&mut self, id: u64, rev: &str) -> Result<(Vec<u8>, u64), BoxError>;
    async fn setup(
        &mut self,
        id: u64,
        rev: &str,
        table: Table,
        segment: u32,
    ) -> Result<(Vec<u8>, u64), BoxError>;
    async fn query(
        &mut self,
        id: u64,
        rev: &str,
        table: Table,
        body: &[u8],
    ) -> Result<Vec<u8>, BoxError>;
    fn concurrency(&self) -> usize {
        1
    }
    async fn batch(&mut self, requests: &[ShardRequest<'_>]) -> Vec<Option<ShardReply>> {
        let mut out = Vec::new();
        let mut failed = false;
        for r in requests {
            if failed {
                out.push(None);
                continue;
            }
            let result = match *r {
                ShardRequest::Manifest { shard_id, revision } => {
                    self.manifest(shard_id, revision).await
                }
                ShardRequest::Setup {
                    shard_id,
                    revision,
                    table,
                    segment,
                } => self.setup(shard_id, revision, table, segment).await,
                ShardRequest::Query {
                    shard_id,
                    revision,
                    table,
                    body,
                } => self.query(shard_id, revision, table, body).await.map(|b| {
                    let n = b.len() as u64;
                    (b, n)
                }),
            };
            failed = result.is_err();
            out.push(Some(result));
        }
        out
    }
}
