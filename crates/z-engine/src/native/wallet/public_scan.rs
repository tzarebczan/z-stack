//! Bounded offline regtest scanner for already authenticated shared publications.
use super::*;
use prost::Message;
use zcash_client_backend::{
    data_api::chain::{error::Error as ChainError, scan_cached_blocks, BlockSource, ChainState},
    proto::compact_formats::CompactBlock,
};
use zcash_primitives::block::BlockHash;

struct Blocks(Vec<CompactBlock>);
impl BlockSource for Blocks {
    type Error = Infallible;
    fn with_blocks<F, E>(
        &self,
        from: Option<BlockHeight>,
        limit: Option<usize>,
        mut visit: F,
    ) -> std::result::Result<(), ChainError<E, Self::Error>>
    where
        F: FnMut(CompactBlock) -> std::result::Result<(), ChainError<E, Self::Error>>,
    {
        let height = u64::from(u32::from(from.unwrap_or(0.into())));
        for block in self
            .0
            .iter()
            .filter(|b| b.height >= height)
            .take(limit.unwrap_or(usize::MAX))
        {
            visit(block.clone())?;
        }
        Ok(())
    }
}
fn invalid() -> EngineError {
    EngineError::Message("public_scan_invalid".into())
}

impl NativeWallet {
    /// Scan a complete, contiguous genesis-based regtest publication, using the
    /// standard native scanner and one wallet transaction. No network requests,
    /// memo enhancement, transparent-address lookup, rebroadcast or fallback.
    ///
    /// `bytes` uses protobuf varint-delimited CompactBlock messages. The caller
    /// verifies publication signatures, freshness, digests and anti-rollback first.
    /// This is shielded-wallet integration research, not production chain trust,
    /// transparent history, or a scalable incremental scanning API.
    pub fn scan_public_regtest(&self, mut bytes: &[u8]) -> Result<u32> {
        if self.network != ZNetwork::Regtest
            || self.birthday_height() != 1
            || bytes.is_empty()
            || bytes.len() > 128 * 1024 * 1024
        {
            return Err(invalid());
        }
        let mut genesis_bytes = crate::web::from_hex(
            "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
        )
        .map_err(|_| invalid())?;
        genesis_bytes.reverse();
        let genesis = BlockHash::from_slice(&genesis_bytes);
        let mut previous = genesis.0.to_vec();
        let mut blocks = Vec::new();
        while !bytes.is_empty() {
            if blocks.len() >= 320 {
                return Err(invalid());
            }
            let block = CompactBlock::decode_length_delimited(&mut bytes).map_err(|_| invalid())?;
            if block.height != (blocks.len() + 1) as u64
                || block.hash.len() != 32
                || block.prev_hash != previous
            {
                return Err(invalid());
            }
            previous = block.hash.clone();
            blocks.push(block);
        }
        let tip = blocks.len() as u32;
        let mut db = self.open_db()?;
        // Cancellation can leave scanning behind an already recorded chain tip.
        // Neither committed height may be rolled back by an older publication.
        if self.scanned_height()? > tip
            || db
                .chain_height()
                .map_err(|error| EngineError::WalletDb(error.to_string()))?
                .is_some_and(|height| u32::from(height) > tip)
        {
            return Err(invalid());
        }
        for b in &blocks {
            if let Some(hash) = db
                .get_block_hash((b.height as u32).into())
                .map_err(|_| invalid())?
            {
                if hash.0.as_slice() != b.hash {
                    return Err(invalid());
                }
            }
        }
        db.transactionally(|wdb| -> anyhow::Result<()> {
            // Recheck under the write transaction if another client advanced it.
            if wdb
                .chain_height()?
                .is_some_and(|height| u32::from(height) > tip)
            {
                anyhow::bail!("public_scan_invalid");
            }
            wdb.update_chain_tip(tip.into())?;
            scan_cached_blocks(
                &self.network,
                &Blocks(blocks),
                wdb,
                1.into(),
                &ChainState::empty(0.into(), genesis),
                tip as usize,
            )
            .map_err(|_| anyhow::anyhow!("public_scan_failed"))?;
            Ok(())
        })
        .map_err(EngineError::from)?;
        Ok(tip)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture_account, fixture_wallet};
    use super::*;

    #[test]
    fn publication_cannot_roll_back_an_unscanned_chain_tip() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        wallet
            .open_db()
            .unwrap()
            .update_chain_tip(300.into())
            .unwrap();
        let scanned = wallet.scanned_height().unwrap();
        assert!(scanned < 1);
        let mut genesis = crate::web::from_hex(
            "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
        )
        .unwrap();
        genesis.reverse();
        let publication = CompactBlock {
            height: 1,
            hash: vec![1; 32],
            prev_hash: genesis,
            ..Default::default()
        }
        .encode_length_delimited_to_vec();

        assert_eq!(
            wallet
                .scan_public_regtest(&publication)
                .unwrap_err()
                .to_string(),
            "public_scan_invalid"
        );
        assert_eq!(wallet.scanned_height().unwrap(), scanned);
        assert_eq!(
            wallet.open_db().unwrap().chain_height().unwrap(),
            Some(300.into())
        );
    }

    #[test]
    fn invalid_publication_framing_and_chain_leave_wallet_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        let before = wallet.scanned_height().unwrap();
        for bytes in [
            vec![],
            vec![0xff],
            vec![0xff; 10],
            CompactBlock {
                height: 1,
                hash: vec![1; 32],
                prev_hash: vec![0; 32],
                ..Default::default()
            }
            .encode_length_delimited_to_vec(),
            CompactBlock {
                height: 2,
                hash: vec![1; 32],
                prev_hash: vec![0; 32],
                ..Default::default()
            }
            .encode_length_delimited_to_vec(),
        ] {
            assert_eq!(
                wallet.scan_public_regtest(&bytes).unwrap_err().to_string(),
                "public_scan_invalid"
            );
            assert_eq!(wallet.scanned_height().unwrap(), before);
        }
        let mut wrong_profile = wallet;
        wrong_profile.meta.birthday_height = 2;
        assert!(wrong_profile.scan_public_regtest(&[0]).is_err());
        wrong_profile.meta.birthday_height = 1;
        wrong_profile.network = ZNetwork::Mainnet;
        assert!(wrong_profile.scan_public_regtest(&[0]).is_err());
    }
}
