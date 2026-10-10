//! Reviewed native payments. Proposal, signed bytes and receipt share the wallet DB.
//! Callers serialize wallet operations and authenticate each signing operation.
use super::*;
use prost::Message;
use rusqlite::{params, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};
use zcash_client_backend::proto::proposal::Proposal as ProposalProto;

/// No input references, seed material, or raw transaction bytes leave this boundary.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PaymentReceipt {
    pub id: String,
    /// prepared, signed, unknown, accepted, or mined. Accepted is not settlement.
    pub phase: String,
    pub txids: Vec<String>,
    pub fee_zat: u64,
    pub expires_at: u64,
}

fn failed(code: &str) -> EngineError {
    EngineError::Message(code.into())
}
fn clock() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| failed("payment_clock_invalid"))
}

// Replacing the scan DB must not discard the only saved copy of a signed
// payment or its reconciliation receipt. A future rescan migration can preserve
// these together; until then fail closed, including for acknowledged payments.
pub(super) fn require_rescan_safe(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| failed("payment_storage_failed"))?;
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ext_native_payments_v1')", [], |r| r.get(0))
        .map_err(|_| failed("payment_storage_failed"))?;
    if exists {
        let signed: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM ext_native_payments_v1 WHERE phase != 'prepared')",
                [],
                |r| r.get(0),
            )
            .map_err(|_| failed("payment_storage_failed"))?;
        if signed {
            return Err(failed("payment_rescan_would_discard_receipts"));
        }
    }
    Ok(())
}
fn connection(wallet: &NativeWallet) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(&wallet.paths.data_db)
        .map_err(|_| failed("payment_storage_failed"))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|_| failed("payment_storage_failed"))?;
    rusqlite::vtab::array::load_module(&conn).map_err(|_| failed("payment_storage_failed"))?;
    // Native scanning normally uses NORMAL. Signed payments and their receipts
    // need FULL on this connection, including when the WAL is already enabled.
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|_| failed("payment_storage_failed"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ext_native_payments_v1 (
        id TEXT PRIMARY KEY, proposal BLOB NOT NULL, fee INTEGER NOT NULL,
        created INTEGER NOT NULL, expires INTEGER NOT NULL, height INTEGER NOT NULL,
        block_hash BLOB NOT NULL, phase TEXT NOT NULL, txids TEXT NOT NULL
    )",
    )
    .map_err(|_| failed("payment_storage_failed"))?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture_account, fixture_wallet};
    use super::*;
    const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn prepare(conn: &rusqlite::Connection) {
        conn.execute("INSERT INTO ext_native_payments_v1 VALUES (?1,x'00',10000,1,121,1,zeroblob(32),'prepared','[]')", [ID]).unwrap();
    }
    #[test]
    fn receipts_are_wallet_bound_and_corruption_is_not_a_signing_or_submission_state() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(first.path());
        let other = fixture_wallet(second.path());
        let conn = connection(&wallet).unwrap();
        prepare(&conn);
        assert_eq!(wallet.payment_receipt(ID).unwrap().phase, "prepared");
        assert_eq!(
            other.payment_receipt(ID).unwrap_err().to_string(),
            "payment_missing"
        );
        assert!(wallet.payment_receipt("../../wallet").is_err());
        for (phase, ids) in [
            ("signed", "[]"),
            ("prepared", "[\"bad\"]"),
            ("other", "[]"),
            ("accepted", "[\"bad\"]"),
        ] {
            conn.execute(
                "UPDATE ext_native_payments_v1 SET phase=?1, txids=?2",
                params![phase, ids],
            )
            .unwrap();
            assert_eq!(
                wallet.payment_receipt(ID).unwrap_err().to_string(),
                "payment_storage_failed"
            );
        }
    }
    #[test]
    fn wallet_state_and_receipt_roll_back_together_and_reopen_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        let conn = connection(&wallet).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA synchronous", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            2
        );
        prepare(&conn);
        let mut db = WalletDb::from_connection(conn, wallet.network, SystemClock, new_rng());
        let before = db.chain_height().unwrap();
        let result = db.transactionally_with_extension(|wdb, ext| -> anyhow::Result<()> {
            wdb.update_chain_tip(20.into())?;
            ext.execute(
                "UPDATE ext_native_payments_v1 SET phase='signed', txids=?1",
                [serde_json::to_string(&vec![ID]).unwrap()],
            )?;
            anyhow::bail!("interrupted before commit")
        });
        assert!(result.is_err());
        drop(db);
        let reopened = NativeWallet::open(dir.path()).unwrap();
        assert_eq!(reopened.open_db().unwrap().chain_height().unwrap(), before);
        assert_eq!(reopened.payment_receipt(ID).unwrap().phase, "prepared");
    }
    #[test]
    fn rescan_cannot_destroy_signed_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = fixture_wallet(dir.path());
        let (ufvk, birthday) = fixture_account();
        wallet.replace_scan_db(&ufvk, &birthday).unwrap();
        let conn = connection(&wallet).unwrap();
        prepare(&conn);
        conn.execute(
            "UPDATE ext_native_payments_v1 SET phase='unknown', txids=?1",
            [serde_json::to_string(&vec![ID]).unwrap()],
        )
        .unwrap();
        assert_eq!(
            wallet
                .replace_scan_db(&ufvk, &birthday)
                .unwrap_err()
                .to_string(),
            "payment_rescan_would_discard_receipts"
        );
        assert_eq!(read(&conn, ID).unwrap().phase, "unknown");
    }
}
fn read(conn: &rusqlite::Connection, id: &str) -> Result<PaymentReceipt> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failed("payment_missing"));
    }
    let row = conn
        .query_row(
            "SELECT phase, txids, fee, expires FROM ext_native_payments_v1 WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                    r.get::<_, u64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|_| failed("payment_storage_failed"))?
        .ok_or_else(|| failed("payment_missing"))?;
    let txids: Vec<String> =
        serde_json::from_str(&row.1).map_err(|_| failed("payment_storage_failed"))?;
    if !matches!(
        row.0.as_str(),
        "prepared" | "signed" | "unknown" | "accepted" | "mined"
    ) || (row.0 == "prepared") != txids.is_empty()
        || txids.len() > 1
        || txids
            .iter()
            .any(|id| id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(failed("payment_storage_failed"));
    }
    Ok(PaymentReceipt {
        id: id.into(),
        phase: row.0,
        txids,
        fee_zat: row.2,
        expires_at: row.3,
    })
}

impl NativeWallet {
    /// Select exact inputs, outputs, change and fee once, without signing or network I/O.
    /// The opaque ID is wallet-bound. Reviews expire after two minutes or a tip change.
    /// This first boundary deliberately supports a single plain-address shielded payment.
    pub fn prepare_payment(
        &self,
        to: &str,
        amount: u64,
        memo: Option<&str>,
    ) -> Result<PaymentReceipt> {
        if to.to_ascii_lowercase().starts_with("zcash:") {
            return Err(failed("payment_plain_address_required"));
        }
        let request = self.send_request(to, amount, memo)?;
        let mut db = self.open_db()?;
        let height = db
            .chain_height()
            .map_err(|_| failed("payment_storage_failed"))?
            .ok_or(EngineError::SyncRequired)?;
        let hash = db
            .get_block_hash(height)
            .map_err(|_| failed("payment_storage_failed"))?
            .ok_or(EngineError::SyncRequired)?;
        let account = Self::primary_account_id(&db)?;
        let proposal = propose_transfer::<_, _, _, _, Infallible>(
            &mut db,
            &self.network,
            account,
            &GreedyInputSelector::new(),
            &SingleOutputChangeStrategy::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Orchard,
                DustOutputPolicy::default(),
            ),
            request,
            crate::confirmations_policy(self.network),
            &shielded_spend_policy(),
            None,
            None,
        )
        .map_err(|e| map_funds_err(format!("propose_transfer: {e}")))?;
        if proposal.steps().len() != 1 {
            return Err(failed("payment_single_step_required"));
        }
        let fee = crate::proposal_fee_zat(&proposal);
        let bytes = ProposalProto::from_standard_proposal(&proposal).encode_to_vec();
        if bytes.len() > 1024 * 1024 {
            return Err(failed("payment_proposal_limit"));
        }
        let mut nonce = [0u8; 32];
        SysRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| failed("payment_random_failed"))?;
        let id = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let now = clock()?;
        let conn = connection(self)?;
        // Expired unsigned reviews can be removed; signed recovery records never are.
        conn.execute(
            "DELETE FROM ext_native_payments_v1 WHERE phase='prepared' AND expires < ?1",
            [now.saturating_sub(86400)],
        )
        .map_err(|_| failed("payment_storage_failed"))?;
        let count: u64 = conn
            .query_row("SELECT COUNT(*) FROM ext_native_payments_v1", [], |r| {
                r.get(0)
            })
            .map_err(|_| failed("payment_storage_failed"))?;
        if count >= 10000 {
            return Err(failed("payment_storage_limit"));
        }
        conn.execute(
            "INSERT INTO ext_native_payments_v1 VALUES (?1,?2,?3,?4,?5,?6,?7,'prepared','[]')",
            params![
                id,
                bytes,
                fee,
                now,
                now + 120,
                u32::from(height),
                hash.0.to_vec()
            ],
        )
        .map_err(|_| failed("payment_storage_failed"))?;
        read(&conn, &id)
    }

    /// Prove/sign the stored proposal. The signed transaction, note reservations and
    /// receipt commit in ONE SQLite transaction before this returns. No broadcast.
    /// A failed/interrupted transaction leaves the review prepared and sends nothing.
    pub fn sign_payment(&self, auth: &SeedAuth, id: &str) -> Result<PaymentReceipt> {
        // Authentication always precedes an operation that could create spend access.
        let seed = self.load_seed(auth)?;
        let usk = UnifiedSpendingKey::from_seed(
            &self.network,
            seed.expose_secret(),
            self.account_index(),
        )
        .map_err(|_| failed("payment_key_failed"))?;
        let conn = connection(self)?;
        let receipt = read(&conn, id)?;
        if receipt.phase != "prepared" {
            return Err(failed("payment_already_signed"));
        }
        let (bytes, created, height, hash) = conn.query_row(
            "SELECT proposal, created, height, block_hash FROM ext_native_payments_v1 WHERE id=?1", [id],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u64>(1)?, r.get::<_, u32>(2)?, r.get::<_, Vec<u8>>(3)?)),
        ).map_err(|_| failed("payment_storage_failed"))?;
        let now = clock()?;
        if now < created || now >= receipt.expires_at {
            return Err(failed("payment_review_expired"));
        }
        let mut db = WalletDb::from_connection(conn, self.network, SystemClock, new_rng());
        let proto = ProposalProto::decode(bytes.as_slice())
            .map_err(|_| failed("payment_storage_failed"))?;
        let prover = crate::params::local_tx_prover()?;
        // Keep the phase check inside the write transaction as well. Concurrent
        // callers must fail rather than sign twice or overwrite a saved receipt.
        db.transactionally_with_extension(|wdb, ext| -> anyhow::Result<()> {
            let unchanged = wdb.chain_height()?.map(u32::from) == Some(height)
                && wdb.get_block_hash(height.into())?.is_some_and(|h| h.0.as_slice() == hash);
            if !unchanged { anyhow::bail!("payment_review_expired"); }
            if ext.execute("UPDATE ext_native_payments_v1 SET phase='prepared' WHERE id=?1 AND phase='prepared'", [id])? != 1 {
                anyhow::bail!("payment_already_signed");
            }
            let proposal = proto.try_into_standard_proposal(&self.network, wdb)
                .map_err(|_| anyhow::anyhow!("payment_review_expired"))?;
            if crate::proposal_fee_zat(&proposal) != receipt.fee_zat { anyhow::bail!("payment_storage_failed"); }
            let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                wdb, &self.network, &*prover, &*prover,
                &SpendingKeys::from_unified_spending_key(usk), OvkPolicy::Sender, &proposal, None,
            ).map_err(|_| anyhow::anyhow!("payment_sign_failed"))?;
            let ids = txids.iter().map(ToString::to_string).collect::<Vec<_>>();
            if ids.len() != 1 { anyhow::bail!("payment_single_step_required"); }
            // Expiry during proving rolls back transactions and reservations together.
            let now = clock()?;
            if now < created || now >= receipt.expires_at { anyhow::bail!("payment_review_expired"); }
            ext.execute("UPDATE ext_native_payments_v1 SET phase='signed', txids=?2 WHERE id=?1",
                params![id, serde_json::to_string(&ids)?])?;
            Ok(())
        }).map_err(EngineError::from)?;
        self.payment_receipt(id)
    }

    /// Read a wallet-bound durable receipt without network activity. A signed
    /// transaction can be recovered after process loss without signing again.
    pub fn payment_receipt(&self, id: &str) -> Result<PaymentReceipt> {
        let conn = connection(self)?;
        let mut receipt = read(&conn, id)?;
        if !receipt.txids.is_empty()
            && receipt.txids.iter().all(|txid| {
                self.transaction(txid)
                    .ok()
                    .flatten()
                    .is_some_and(|t| t.mined_height.is_some())
            })
        {
            receipt.phase = "mined".into();
        }
        Ok(receipt)
    }

    /// Submit ONLY the already-saved signed bytes. Retrying this operation cannot
    /// create a replacement payment. Transport refusal/loss remains unknown.
    pub async fn submit_payment(&self, id: &str) -> Result<PaymentReceipt> {
        let conn = connection(self)?;
        let receipt = self.payment_receipt(id)?;
        if matches!(receipt.phase.as_str(), "accepted" | "mined") {
            return Ok(receipt);
        }
        if !matches!(receipt.phase.as_str(), "signed" | "unknown") {
            return Err(failed("payment_not_signed"));
        }
        conn.execute(
            "UPDATE ext_native_payments_v1 SET phase='unknown' WHERE id=?1",
            [id],
        )
        .map_err(|_| failed("payment_storage_failed"))?;
        let mut db = self.open_db()?;
        let mut client = None;
        for id in &receipt.txids {
            let txid = TxId::from_hex(id).ok_or_else(|| failed("payment_storage_failed"))?;
            match self.broadcast_one(&mut client, &mut db, txid).await {
                Ok(_) => {}
                Err(e) if already_known_transaction(&e) => {}
                Err(_) => return self.payment_receipt(&receipt.id),
            }
        }
        conn.execute(
            "UPDATE ext_native_payments_v1 SET phase='accepted' WHERE id=?1",
            [&receipt.id],
        )
        .map_err(|_| failed("payment_storage_failed"))?;
        self.payment_receipt(&receipt.id)
    }
}
