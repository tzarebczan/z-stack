use super::super::{
    new_rng, open_wallet_db, LightServer, SystemClock, UnlockPolicy, WalletDb, WalletMeta,
    WalletPaths,
};
use super::*;
use crate::{
    keys::{account_from_mnemonic, REGTEST_FAUCET_MNEMONIC},
    Network,
};
use zcash_client_backend::{
    data_api::{AccountBirthday, AccountPurpose, WalletWrite},
    proto::{
        compact_formats::{ChainMetadata, CompactBlock, CompactOrchardAction, CompactTx},
        service::TreeState,
    },
    scanning::{scan_block, Nullifiers, ScanningKeys},
};
use zcash_keys::keys::UnifiedFullViewingKey;
const START: u32 = 1_000_001;
const BIRTHDAY: u32 = START + 4;
const END: u32 = START + 9;
struct Fixture {
    _dir: tempfile::TempDir,
    wallet: NativeWallet,
    source: NativeWalletSource,
    bundles: Vec<Vec<u8>>,
    public: TxId,
    private: TxId,
}
fn fixture(private_only: bool) -> Fixture {
    fixture_with_memo(private_only, "complete public memo")
}
fn fixture_with_memo(private_only: bool, public_memo: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = NativeWalletSource::new(
        tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy(),
        Network::Regtest,
        [0x71; 32],
    );
    crate::native::source::enroll_wallet(root, &source).unwrap();
    let account = account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap();
    let ufvk = UnifiedFullViewingKey::decode(&Network::Regtest, &account.ufvk).unwrap();
    let web = crate::web::WebWallet::from_account(
        account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0).unwrap(),
        BIRTHDAY,
    )
    .unwrap();
    let make_raw = |ironwood| {
        crate::web::shielded_memo_transaction(
            &web,
            &web,
            if ironwood {
                "private Ironwood"
            } else {
                public_memo
            },
            50000,
            ironwood,
        )
    };
    let mut public_tx = Transaction::read(make_raw(false).as_slice(), BranchId::Nu6_3).unwrap();
    // Public decryption fixture has an unresolved transparent funding input, so native
    // storage does not interpret an output-only synthetic value balance as a negative fee.
    public_tx = public_tx
        .into_data()
        .map_bundles::<zcash_primitives::transaction::Authorized>(
            |_| {
                Some(transparent::bundle::Bundle {
                    vin: vec![transparent::bundle::TxIn::from_parts(
                        transparent::bundle::OutPoint::new([0x22; 32], 0),
                        transparent::address::Script(zcash_script::script::Code(vec![])),
                        u32::MAX,
                    )],
                    vout: vec![],
                    authorization: transparent::bundle::Authorized,
                })
            },
            |sapling| sapling,
            |orchard| orchard,
        )
        .freeze()
        .unwrap();
    let mut public_raw = Vec::new();
    public_tx.write(&mut public_raw).unwrap();
    let raws = [public_raw, make_raw(true)];
    let parsed: Vec<_> = raws
        .iter()
        .map(|raw| Transaction::read(raw.as_slice(), BranchId::Nu6_3).unwrap())
        .collect();
    let public = parsed[0].txid();
    let private = parsed[1].txid();
    let mut previous = vec![0; 32];
    let mut blocks = Vec::new();
    let mut orchard_size = 0;
    let mut ironwood_size = 0;
    for height in START..=END {
        let mut block = CompactBlock {
            height: height.into(),
            hash: vec![(height - START + 1) as u8; 32],
            prev_hash: previous,
            time: 1,
            ..Default::default()
        };
        if height == BIRTHDAY {
            for (index, tx) in parsed
                .iter()
                .enumerate()
                .filter(|(index, _)| !private_only || *index == 1)
            {
                let ironwood = index == 1;
                let bundle = if ironwood {
                    tx.ironwood_bundle()
                } else {
                    tx.orchard_bundle()
                }
                .unwrap();
                let actions: Vec<_> = bundle
                    .actions()
                    .iter()
                    .map(CompactOrchardAction::from)
                    .collect();
                if ironwood {
                    ironwood_size += actions.len() as u32;
                } else {
                    orchard_size += actions.len() as u32;
                }
                block.vtx.push(CompactTx {
                    index: index as u64,
                    txid: tx.txid().as_ref().to_vec(),
                    actions: if ironwood { vec![] } else { actions.clone() },
                    ironwood_actions: if ironwood { actions } else { vec![] },
                    ..Default::default()
                });
            }
        }
        block.chain_metadata = Some(ChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: orchard_size,
            ironwood_commitment_tree_size: ironwood_size,
        });
        previous = block.hash.clone();
        blocks.push(block);
    }
    let state = TreeState {
        height: u64::from(BIRTHDAY - 1),
        hash: crate::web::to_hex(&blocks[3].hash),
        ..Default::default()
    }
    .to_chain_state()
    .unwrap();
    let mut db = open_wallet_db(&root.join("data.sqlite"), Network::Regtest)
        .unwrap()
        .with_enhancement_mode(EnhancementMode::PrivateIronwood);
    zcash_client_sqlite::wallet::init::init_wallet_db(&mut db, None).unwrap();
    db.import_account_ufvk(
        "public vector",
        &ufvk,
        &AccountBirthday::from_parts(state.clone(), None),
        AccountPurpose::Spending { derivation: None },
        None,
    )
    .unwrap();
    let keys = ScanningKeys::from_account_ufvks(db.get_unified_full_viewing_keys().unwrap());
    let scanned = blocks
        .iter()
        .filter(|block| block.height >= u64::from(BIRTHDAY))
        .map(|block| {
            scan_block(
                &Network::Regtest,
                block.clone(),
                &keys,
                &Nullifiers::empty(),
                None,
            )
            .unwrap()
        })
        .collect();
    db.update_chain_tip(END.into()).unwrap();
    db.put_blocks(&state, scanned).unwrap();
    drop(db);
    crate::native::source::enroll_wallet(root, &source).unwrap();
    let wallet = NativeWallet {
        paths: WalletPaths::new(root),
        network: Network::Regtest,
        network_access: false,
        server: LightServer::Url("source-wallet:offline".into()),
        meta: WalletMeta {
            network: "regtest".into(),
            server: "source-wallet:offline".into(),
            birthday_height: BIRTHDAY,
            account_index: 0,
            validator_rpc: None,
            ufvk: Some(account.ufvk),
            view_only: true,
            unlock_policy: UnlockPolicy::EachSpend,
            os_unlock: false,
            allow_deep_sync: false,
        },
    };
    wallet.save_meta().unwrap();
    let bundles=blocks.iter().map(|block|serde_json::to_vec(&serde_json::json!({"start":block.height,"end":block.height,"blocks":crate::web::to_hex(&crate::web::encode_one(block)),"transactions":if block.height==u64::from(BIRTHDAY){raws.iter().enumerate().filter(|(i,_)|!private_only||*i==1).map(|(_,raw)|crate::web::to_hex(raw)).collect::<Vec<_>>()}else{vec![]}})).unwrap()).collect();
    Fixture {
        _dir: dir,
        wallet,
        source,
        bundles,
        public,
        private,
    }
}
fn raw(wallet: &NativeWallet, id: TxId) -> Option<Transaction> {
    wallet.open_db().unwrap().get_transaction(id).unwrap()
}
fn apply_all(f: &Fixture, pass: &mut NativeSharedMemosPass) -> Result<u32> {
    f.bundles.iter().try_fold(0, |sum, bytes| {
        f.wallet
            .apply_shared_memos_with_source(&f.source, pass, bytes)
            .map(|count| sum + count)
    })
}
#[tokio::test]
async fn complete_public_bucket_applies_only_public_work_after_unknown_birthday_prefix() {
    let f = fixture(false);
    let mut pass = f.wallet.begin_shared_memos_with_source(&f.source).unwrap();
    assert_eq!(
        pass.ranges(),
        (START..=END)
            .map(|height| (height, height))
            .collect::<Vec<_>>()
    );
    assert!(raw(&f.wallet, f.public).is_none());
    assert!(raw(&f.wallet, f.private).is_none());
    let pending = f.wallet.history(10).unwrap();
    assert!(pending
        .iter()
        .all(|tx| tx.memo_status == crate::MemoStatus::Pending));
    for bundle in &f.bundles[..9] {
        assert_eq!(
            f.wallet
                .apply_shared_memos_with_source(&f.source, &mut pass, bundle)
                .unwrap(),
            0
        );
        assert!(raw(&f.wallet, f.public).is_none());
    }
    assert_eq!(
        f.wallet
            .apply_shared_memos_with_source(&f.source, &mut pass, &f.bundles[9])
            .unwrap(),
        1
    );
    assert!(raw(&f.wallet, f.public).is_some());
    assert!(raw(&f.wallet, f.private).is_none());
    let history = f.wallet.history(10).unwrap();
    assert_eq!(
        history
            .iter()
            .find(|tx| tx.txid == f.public.to_string())
            .unwrap()
            .memo_status,
        crate::MemoStatus::Available
    );
    assert_eq!(
        history
            .iter()
            .find(|tx| tx.txid == f.private.to_string())
            .unwrap()
            .memo_status,
        crate::MemoStatus::Pending
    );
    assert!(history
        .iter()
        .any(|tx| tx.memos.iter().any(|memo| memo == "complete public memo")));
    assert!(!history
        .iter()
        .any(|tx| tx.memos.iter().any(|memo| memo == "private Ironwood")));
    assert!(f
        .wallet
        .begin_shared_memos_with_source(&f.source)
        .unwrap()
        .ranges()
        .is_empty());
    assert!(f
        .wallet
        .apply_shared_memos_with_source(&f.source, &mut pass, &f.bundles[9])
        .is_err());
    let reopened = NativeWallet::open_with_wallet_source(&f.wallet.paths.root, &f.source).unwrap();
    assert!(raw(&reopened, f.public).is_some());
}
#[tokio::test]
async fn private_work_alone_never_creates_public_ranges() {
    let f = fixture(true);
    assert!(f
        .wallet
        .begin_shared_memos_with_source(&f.source)
        .unwrap()
        .ranges()
        .is_empty());
    assert!(raw(&f.wallet, f.private).is_none());
}
#[tokio::test]
async fn missing_malformed_swapped_and_omitted_data_consumes_without_partial_apply() {
    for mode in 0..4 {
        let f = fixture(false);
        let mut pass = f.wallet.begin_shared_memos_with_source(&f.source).unwrap();
        for bundle in &f.bundles[..4] {
            f.wallet
                .apply_shared_memos_with_source(&f.source, &mut pass, bundle)
                .unwrap();
        }
        let mut bundle: serde_json::Value = serde_json::from_slice(&f.bundles[4]).unwrap();
        match mode {
            0 => {
                bundle["transactions"].as_array_mut().unwrap().pop();
            }
            1 => {
                let text = bundle["transactions"][1].as_str().unwrap().to_owned() + "00";
                bundle["transactions"][1] = text.into();
            }
            2 => {
                bundle["transactions"].as_array_mut().unwrap().swap(0, 1);
            }
            _ => {
                let frames = crate::web::from_hex(bundle["blocks"].as_str().unwrap()).unwrap();
                let mut blocks = crate::web::decode_delimited(&frames).unwrap();
                blocks[0].vtx.remove(0);
                bundle["blocks"] = crate::web::to_hex(&crate::web::encode_one(&blocks[0])).into();
                bundle["transactions"].as_array_mut().unwrap().remove(0);
            }
        }
        assert!(f
            .wallet
            .apply_shared_memos_with_source(
                &f.source,
                &mut pass,
                &serde_json::to_vec(&bundle).unwrap()
            )
            .is_err());
        assert!(pass.ranges().is_empty());
        assert!(raw(&f.wallet, f.public).is_none());
        assert!(f
            .wallet
            .apply_shared_memos_with_source(&f.source, &mut pass, &f.bundles[4])
            .is_err());
    }
}
#[tokio::test]
async fn wrong_binding_cancel_reorg_and_failed_atomic_commit_preserve_wallet() {
    for mode in 0..4 {
        let f = fixture(false);
        let mut pass = f.wallet.begin_shared_memos_with_source(&f.source).unwrap();
        assert!(f.wallet.ordinary_scan_lease().is_err());
        for bundle in &f.bundles[..9] {
            f.wallet
                .apply_shared_memos_with_source(&f.source, &mut pass, bundle)
                .unwrap();
        }
        if mode == 0 {
            let wrong = NativeWalletSource::new(
                tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy(),
                Network::Regtest,
                [0x72; 32],
            );
            assert!(f
                .wallet
                .apply_shared_memos_with_source(&wrong, &mut pass, &f.bundles[9])
                .is_err());
        } else if mode == 1 {
            pass.cancel();
            assert!(f
                .wallet
                .apply_shared_memos_with_source(&f.source, &mut pass, &f.bundles[9])
                .is_err());
        } else {
            let conn = rusqlite::Connection::open(&f.wallet.paths.data_db).unwrap();
            if mode == 2 {
                conn.execute(
                    "UPDATE blocks SET hash=?1 WHERE height=?2",
                    rusqlite::params![[99u8; 32].as_slice(), END],
                )
                .unwrap();
            } else {
                conn.execute_batch("CREATE TRIGGER fixture_refuse_raw BEFORE UPDATE OF raw ON transactions WHEN NEW.raw IS NOT NULL BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
            }
            drop(conn);
            assert!(f
                .wallet
                .apply_shared_memos_with_source(&f.source, &mut pass, &f.bundles[9])
                .is_err());
        }
        assert!(raw(&f.wallet, f.public).is_none());
        assert!(raw(&f.wallet, f.private).is_none());
        assert!(f.wallet.ordinary_scan_lease().is_ok());
    }
}

#[tokio::test]
async fn authentic_empty_memo_is_empty_but_missing_or_invalid_evidence_is_unknown() {
    let f = fixture_with_memo(false, "");
    let mut pass = f.wallet.begin_shared_memos_with_source(&f.source).unwrap();
    assert_eq!(apply_all(&f, &mut pass).unwrap(), 1);
    let status = || {
        f.wallet
            .history(10)
            .unwrap()
            .into_iter()
            .find(|tx| tx.txid == f.public.to_string())
            .unwrap()
            .memo_status
    };
    assert_eq!(status(), crate::MemoStatus::Empty);
    let conn = rusqlite::Connection::open(&f.wallet.paths.data_db).unwrap();
    for memo in [None, Some(vec![0xffu8; 513])] {
        conn.execute("UPDATE orchard_received_notes SET memo=?1 WHERE transaction_id=(SELECT id_tx FROM transactions WHERE txid=?2)",rusqlite::params![memo,f.public.as_ref()]).unwrap();
        assert_eq!(status(), crate::MemoStatus::Unknown);
    }
}
#[tokio::test]
async fn public_apply_preserves_private_rediscover_and_suspended_work() {
    use zcash_client_backend::data_api::enhance_pir::{EnhancePirSuspension, EnhancePirWork};
    for suspended in [0, 1] {
        let f = fixture(false);
        let conn = rusqlite::Connection::open(&f.wallet.paths.data_db).unwrap();
        conn.execute("INSERT OR IGNORE INTO blocks(height,hash,time,sapling_tree,sapling_commitment_tree_size,orchard_commitment_tree_size,ironwood_commitment_tree_size) SELECT ?1,?2,time,sapling_tree,0,0,0 FROM blocks WHERE height=?3",rusqlite::params![BIRTHDAY-1,[4u8;32].as_slice(),BIRTHDAY]).unwrap();
        conn.execute("INSERT INTO ironwood_enhance_discovery_queue(transaction_id,suspended) SELECT id_tx,?1 FROM transactions WHERE txid=?2",rusqlite::params![suspended,f.private.as_ref()]).unwrap();
        let private_work = || {
            f.wallet
                .open_db()
                .unwrap()
                .with_enhancement_mode(EnhancementMode::PrivateIronwood)
                .transaction_enhancement_work()
                .unwrap()
                .into_iter()
                .filter(|w| matches!(w, TransactionEnhancementWork::Private(_)))
                .collect::<Vec<_>>()
        };
        let before = private_work();
        if suspended == 0 {
            assert!(before.iter().any(|w| matches!(
                w,
                TransactionEnhancementWork::Private(EnhancePirWork::Rediscover(_))
            )));
        } else {
            assert!(before.iter().any(|w| matches!(
                w,
                TransactionEnhancementWork::Private(EnhancePirWork::Suspended(
                    EnhancePirSuspension::Discovery(_)
                ))
            )));
        }
        let mut pass = f.wallet.begin_shared_memos_with_source(&f.source).unwrap();
        assert_eq!(apply_all(&f, &mut pass).unwrap(), 1);
        assert_eq!(private_work(), before);
        assert!(raw(&f.wallet, f.private).is_none());
        assert_eq!(
            f.wallet
                .history(10)
                .unwrap()
                .iter()
                .find(|tx| tx.txid == f.private.to_string())
                .unwrap()
                .memo_status,
            crate::MemoStatus::Pending
        );
    }
}

#[tokio::test]
async fn rediscovery_row_decode_error_keeps_authenticated_empty_memo_unknown() {
    let f = fixture_with_memo(false, "");
    let mut pass = f.wallet.begin_shared_memos_with_source(&f.source).unwrap();
    assert_eq!(apply_all(&f, &mut pass).unwrap(), 1);
    let status = || {
        f.wallet
            .transaction(&f.public.to_string())
            .unwrap()
            .unwrap()
            .memo_status
    };
    assert_eq!(status(), crate::MemoStatus::Empty);
    let conn = rusqlite::Connection::open(&f.wallet.paths.data_db).unwrap();
    // Rediscovery work carries the block hash, so its typed work is valid even
    // when one durable queue member has a corrupt txid. Its ID lookup must fail closed.
    conn.execute("INSERT OR IGNORE INTO blocks(height,hash,time,sapling_tree,sapling_commitment_tree_size,orchard_commitment_tree_size,ironwood_commitment_tree_size) SELECT ?1,?2,time,sapling_tree,0,0,0 FROM blocks WHERE height=?3",rusqlite::params![BIRTHDAY-1,[4u8;32].as_slice(),BIRTHDAY]).unwrap();
    conn.execute(
        "INSERT INTO transactions(txid,mined_height,tx_index,min_observed_height) VALUES(?1,?2,99,?2)",
        rusqlite::params![[1u8, 2].as_slice(), BIRTHDAY],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO ironwood_enhance_routing(transaction_id,route) VALUES(?1,0)",
        [id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ironwood_enhance_discovery_queue(transaction_id,suspended) VALUES(?1,0)",
        [id],
    )
    .unwrap();
    conn.pragma_update(None, "query_only", true).unwrap();
    let changes = conn.total_changes();
    let db = WalletDb::from_connection(&conn, Network::Regtest, SystemClock, new_rng())
        .with_enhancement_mode(EnhancementMode::PrivateIronwood);
    assert!(db
        .transaction_enhancement_work()
        .unwrap()
        .iter()
        .any(|work| matches!(
            work,
            TransactionEnhancementWork::Private(
                zcash_client_backend::data_api::enhance_pir::EnhancePirWork::Rediscover(_)
            )
        )));
    assert_eq!(status(), crate::MemoStatus::Unknown);
    assert!(super::super::memo_status::work(&conn, Network::Regtest).is_none());
    assert_eq!(
        conn.total_changes(),
        changes,
        "status lookup must remain query-only"
    );
}
