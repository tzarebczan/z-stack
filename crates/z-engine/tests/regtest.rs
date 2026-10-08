//! Native create / sync / shield / send against local Zebra+Zaino regtest.
//!
//! ```text
//! pnpm regtest:up
//! Z_STACK_REGTEST=1 cargo test -p z-engine --test regtest -- --ignored --nocapture --test-threads=1
//! ```

use anyhow::Context;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use z_engine::keys::{REGTEST_FAUCET_MNEMONIC, REGTEST_FAUCET_TRANSPARENT};
use z_engine::native::{NativeWallet, SeedAuth};
use z_engine::{LightServer, Network};

fn enabled() -> bool {
    std::env::var("Z_STACK_REGTEST").ok().as_deref() == Some("1")
}

fn zebra_rpc(method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    })
    .to_string();
    let mut stream = TcpStream::connect("127.0.0.1:29232")?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let req = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1:29232\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(req.as_bytes())?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    let json = resp
        .split("\r\n\r\n")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("no HTTP body: {resp}"))?;
    let v: serde_json::Value = serde_json::from_str(json.trim())?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        anyhow::bail!("RPC {method}: {err}");
    }
    Ok(v["result"].clone())
}

fn generate(n: u32) -> anyhow::Result<()> {
    zebra_rpc("generate", serde_json::json!([n]))?;
    Ok(())
}

fn require_regtest_chain(info: &serde_json::Value) -> anyhow::Result<()> {
    // Zebra labels regtest "test", so identify the configured activation
    // schedule before mining or broadcasting in this opt-in fixture.
    let chain = info["chain"].as_str().unwrap_or("");
    let upgrades = info["upgrades"]
        .as_object()
        .context("missing upgrade schedule")?;
    let at = |name: &str, height: u64| {
        upgrades.values().any(|u| {
            u["name"].as_str() == Some(name) && u["activationheight"].as_u64() == Some(height)
        })
    };
    anyhow::ensure!(
        (chain == "test" || chain == "regtest")
            && at("NU5", 2)
            && at("NU6.2", 2)
            && at("NU6.3", u64::from(z_engine::regtest_nu6_3_height())),
        "validator does not match the repository regtest activation schedule"
    );
    let configured = z_engine::regtest_nu7_height();
    let scheduled = upgrades
        .values()
        .find(|u| u["name"].as_str() == Some("NU7"))
        .and_then(|u| u["activationheight"].as_u64());
    anyhow::ensure!(
        configured.map(u64::from) == scheduled
            || (configured.is_none()
                && scheduled.is_none_or(|h| h > info["blocks"].as_u64().unwrap_or(0))),
        "validator NU7 schedule does not match the client"
    );
    Ok(())
}

async fn wait_zaino_height(min: u32) -> anyhow::Result<u32> {
    let server = LightServer::LocalRegtest;
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last = String::new();
    while Instant::now() < deadline {
        match NativeWallet::fetch_tip(&server).await {
            Ok(h) if h >= min => return Ok(h),
            Ok(h) => last = format!("tip {h} < {min}"),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("Zaino not at height {min}: {last}")
}

fn auth() -> SeedAuth {
    SeedAuth::passphrase("regtest")
}

#[tokio::test]
#[ignore = "needs docker compose regtest (Z_STACK_REGTEST=1)"]
async fn create_sync_shield_send() -> anyhow::Result<()> {
    if !enabled() {
        eprintln!("skip: set Z_STACK_REGTEST=1");
        return Ok(());
    }

    let info = zebra_rpc("getblockchaininfo", serde_json::json!([]))?;
    require_regtest_chain(&info)?;
    let mut height = info["blocks"].as_u64().unwrap_or(0) as u32;
    if height < 2 {
        generate(2 - height)?;
        height = 2;
    }
    wait_zaino_height(height).await?;

    let faucet_dir = TempDir::new()?;
    let server = LightServer::LocalRegtest;

    let (faucet, faucet_ua) = NativeWallet::restore(
        faucet_dir.path(),
        REGTEST_FAUCET_MNEMONIC,
        Network::Regtest,
        Some(server.clone()),
        1,
        auth(),
        0,
    )
    .await?;
    let taddr = faucet.transparent_address()?.expect("faucet t-addr");
    anyhow::ensure!(
        taddr == REGTEST_FAUCET_TRANSPARENT,
        "compose miner_address must be {REGTEST_FAUCET_TRANSPARENT}, got {taddr}"
    );
    println!("faucet UA {faucet_ua}");
    println!("faucet t-addr {taddr}");

    // Coinbase maturity is 100 blocks. Do not mine extra to the faucet right
    // before shielding — new coinbases would be immature.
    if height < 110 {
        generate(110 - height)?;
        height = 110;
        wait_zaino_height(height).await?;
    }
    let tip = wait_zaino_height(height).await?;
    println!("zaino tip {tip}");

    let (scanned, _) = faucet.sync().await?;
    println!("faucet synced {scanned}");
    let bal = faucet.balance()?;
    println!(
        "faucet transparent={} orchard={}",
        bal.transparent_available, bal.orchard_available
    );
    let mut funded_tip = tip;
    // A reused chain can already have its faucet coins shielded. Reuse those
    // funds, as the SDK fixture does, instead of requiring another 100,000 zat
    // of transparent funding. Fresh chains still require a successful shield.
    if bal.orchard_available < 60_000 {
        anyhow::ensure!(
            bal.transparent_available >= 100_000,
            "faucet needs shielding funds"
        );
        let txids = faucet.shield(&auth(), 100_000).await?;
        anyhow::ensure!(!txids.is_empty(), "shield produced no transaction");
        println!("shield txids {txids:?}");
        anyhow::ensure!(
            faucet.balance()?.orchard_pending > 0,
            "shield should show pending orchard until mined"
        );
        generate(20)?;
        funded_tip = tip + 20;
        wait_zaino_height(funded_tip).await?;
        faucet.sync().await?;
        anyhow::ensure!(
            faucet.balance()?.orchard_available >= 60_000,
            "shield did not fund the send"
        );
    } else {
        println!("reusing confirmed faucet Orchard funds");
    }
    // Proving and recipient confirmation are mandatory on every run, including
    // reused chains. A failed proof is never treated as an optional fixture step.
    let recipient_dir = tempfile::Builder::new()
        .prefix("z-native-recipient-")
        .tempdir()?;
    let recipient_path = recipient_dir.path().to_path_buf();
    // Keep only this disposable encrypted wallet when diagnosing a failed live
    // scan; the public faucet and ordinary successful fixtures remain temporary.
    let _recipient_guard = if std::env::var("Z_STACK_REGTEST_KEEP_RECIPIENT").as_deref() == Ok("1")
    {
        println!(
            "retained encrypted recipient: {}",
            recipient_dir.keep().display()
        );
        None
    } else {
        Some(recipient_dir)
    };
    let (_recv, created) = NativeWallet::create(
        &recipient_path,
        Network::Regtest,
        Some(server),
        Some(1),
        auth(),
        0,
    )
    .await?;
    let sent = faucet
        .send(&auth(), &created.unified_address, 50_000, None)
        .await?;
    anyhow::ensure!(!sent.is_empty(), "send produced no transaction");
    println!("send txids {sent:?}");
    generate(15)?;
    wait_zaino_height(funded_tip + 15).await?;
    NativeWallet::open(&recipient_path)?.sync().await?;
    let recv_bal = NativeWallet::open(&recipient_path)?.balance()?;
    println!("recipient orchard={}", recv_bal.orchard_available);
    anyhow::ensure!(
        recv_bal.orchard_available == 50_000,
        "recipient did not receive exactly the sent amount"
    );

    Ok(())
}

async fn sync_through(wallet: &NativeWallet, height: u32, phase: &str) -> anyhow::Result<()> {
    wait_zaino_height(height).await?;
    if wallet.scanned_height()? >= height {
        // Also exercises checkpoint recovery for wallets created by old builds.
        wallet.catch_up().await?;
        println!("{phase}: already scanned={height}");
        return Ok(());
    }
    let started = Instant::now();
    let (scanned, _) = wallet.sync().await.with_context(|| phase.to_owned())?;
    anyhow::ensure!(scanned >= height, "{phase}: scanned {scanned} < {height}");
    println!(
        "{phase}: scanned={scanned} ms={}",
        started.elapsed().as_millis()
    );
    Ok(())
}

async fn mine_through(height: &mut u32, target: u32) -> anyhow::Result<()> {
    // Bound each generate RPC; a single 9000-block call can exceed its timeout.
    let mut retries = 0;
    while *height < target {
        let count = (target - *height).min(250);
        let result = generate(count);
        // generate can commit a prefix before returning an error. Always read
        // the durable tip before retrying rather than mining that prefix twice.
        let info = zebra_rpc("getblockchaininfo", serde_json::json!([]))?;
        *height = u32::try_from(info["blocks"].as_u64().context("missing mined height")?)?;
        if let Err(error) = result {
            if !error.to_string().contains("no available capacity") || retries >= 3 {
                return Err(error);
            }
            retries += 1;
            eprintln!("mining queue busy at {height}; retry {retries}/3");
            tokio::time::sleep(Duration::from_secs(1)).await;
        } else {
            retries = 0;
        }
        println!("mined {height}/{target}");
    }
    wait_zaino_height(*height).await?;
    Ok(())
}

/// Exercise the failure branch even on platforms where an open SQLite file can
/// normally be renamed. A directory where the replacement database is built
/// blocks replacement.
async fn assert_blocked_wipe_retains_wallet(wallet: &NativeWallet) -> anyhow::Result<()> {
    let scanned = wallet.scanned_height()?;
    let balance = serde_json::to_value(wallet.balance()?)?;
    let history = serde_json::to_value(wallet.history(500)?)?;
    let blocker = wallet.paths().data_db.with_file_name("data.sqlite.new");
    std::fs::create_dir(&blocker)?;
    let result = wallet.reset_scan().await;
    std::fs::remove_dir(&blocker)?;
    let error = result.expect_err("blocked database replacement must not report a successful wipe");
    anyhow::ensure!(
        error.to_string().contains("existing data was retained"),
        "unexpected reset failure: {error}"
    );
    anyhow::ensure!(
        wallet.scanned_height()? == scanned,
        "blocked wipe changed scan height"
    );
    anyhow::ensure!(
        serde_json::to_value(wallet.balance()?)? == balance,
        "blocked wipe changed funds"
    );
    anyhow::ensure!(
        serde_json::to_value(wallet.history(500)?)? == history,
        "blocked wipe changed history"
    );
    println!("blocked wipe preserved scan, funds and history");
    Ok(())
}

struct MinedTransaction {
    height: u32,
    spent: u64,
    received: u64,
}

fn mined_transaction(wallet: &NativeWallet, txid: &str) -> anyhow::Result<MinedTransaction> {
    // Broadcast returns canonical RPC hex; sqlite stores the internal byte
    // order. Inspect the durable row directly so SDK history formatting is
    // independent of this scanner/prover gate.
    let id = zcash_protocol::TxId::from_hex(txid).context("invalid broadcast txid")?;
    let conn = rusqlite::Connection::open_with_flags(
        &wallet.paths().data_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    conn.query_row(
        "SELECT mined_height, total_spent, total_received FROM v_transactions
         WHERE txid = ?1 AND mined_height IS NOT NULL",
        [id.as_ref().as_slice()],
        |row| {
            Ok(MinedTransaction {
                height: row.get(0)?,
                spent: row.get(1)?,
                received: row.get(2)?,
            })
        },
    )
    .with_context(|| format!("missing mined transaction {txid}"))
}

/// Exercise the desktop's native sync path with actual Orchard commitments,
/// an 8000-height sparse persist, a late receive, a birthday wipe, and a spend.
/// Run alone or with --test-threads=1: regtest tests share the faucet and chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "uses >9000 blocks of history and proves Orchard transactions; needs local regtest"]
async fn frontier_skip_restore_wipe_spend() -> anyhow::Result<()> {
    anyhow::ensure!(
        enabled(),
        "set Z_STACK_REGTEST=1 for the live restore/spend gate"
    );
    let info = zebra_rpc("getblockchaininfo", serde_json::json!([]))
        .context("regtest Zebra RPC at 127.0.0.1:29232; start pnpm regtest:up first")?;
    require_regtest_chain(&info)?;
    let mut height = u32::try_from(info["blocks"].as_u64().context("missing Zebra height")?)?;
    // This fixture stays below regtest's Ironwood activation and the ordinary
    // restore depth guard, including on a chain left up by an earlier run.
    anyhow::ensure!(height < 100_000, "use a regtest chain below height 100000");
    mine_through(&mut height, 110).await?;
    // Reuse an existing note-free history on reruns: this fresh recipient has
    // never received funds. Keep the first receipt at exactly offset9356 so
    // the two birthdays still exercise opposite sides of the streamed prefix.
    let birthday = height
        .saturating_sub(z_engine::scan::HISTORIC_PERSIST_BLOCKS + z_engine::scan::FETCH_CHUNK_LOCAL)
        .max(1);
    let faucet_dir = TempDir::new()?;
    let recipient_dir = TempDir::new()?;
    let restored_dir = TempDir::new()?;
    let prefix_restored_dir = TempDir::new()?;
    let server = LightServer::LocalRegtest;
    // Optional reuse affects only the public funding wallet, never the fresh
    // recipient, restore, wipe or witness checks. This avoids importing every
    // mined coinbase again while iterating on a long-lived disposable chain.
    let (faucet, faucet_ua) = if let Some(path) = std::env::var_os("Z_STACK_REGTEST_FAUCET_DIR") {
        let path = std::path::PathBuf::from(path);
        anyhow::ensure!(
            path.is_absolute() && path.starts_with(std::env::temp_dir()),
            "faucet fixture must be in the temporary directory"
        );
        let wallet = NativeWallet::open(&path)?;
        anyhow::ensure!(
            wallet.server_url() == server.as_url(),
            "faucet fixture server mismatch"
        );
        let address = wallet.unified_address()?;
        (wallet, address)
    } else {
        NativeWallet::restore(
            faucet_dir.path(),
            REGTEST_FAUCET_MNEMONIC,
            Network::Regtest,
            Some(server.clone()),
            1,
            auth(),
            0,
        )
        .await?
    };
    anyhow::ensure!(
        faucet.transparent_address()?.as_deref() == Some(REGTEST_FAUCET_TRANSPARENT),
        "regtest faucet address mismatch"
    );
    sync_through(&faucet, height, "faucet initial sync").await?;
    let (recipient, created) = NativeWallet::create(
        recipient_dir.path(),
        Network::Regtest,
        Some(server.clone()),
        Some(birthday),
        auth(),
        0,
    )
    .await?;

    // Advance the Orchard tree AFTER the recipient's birthday, with a note it
    // cannot decrypt. Thousands of coinbase-only blocks alone would leave the
    // frontier sizes unchanged and never exercise frontier_covers == false.
    // Keep this after both restore birthdays used below, so the prefix and
    // suffix cases both skip commitments before they encounter their note.
    mine_through(&mut height, birthday + 512).await?;
    sync_through(&faucet, height, "faucet before unrelated commitments").await?;
    let unrelated = if faucet.balance()?.orchard_available >= 100_000 {
        faucet.send(&auth(), &faucet_ua, 50_000, None).await?
    } else {
        faucet.shield(&auth(), 100_000).await?
    };
    anyhow::ensure!(!unrelated.is_empty(), "no unrelated Orchard transaction");
    let target = height + 20;
    mine_through(&mut height, target).await?;
    sync_through(&faucet, height, "faucet Orchard funding").await?;
    let unrelated_entry = mined_transaction(&faucet, &unrelated[0])?;
    anyhow::ensure!(
        unrelated_entry.height > birthday,
        "unrelated commitments must be mined after birthday"
    );
    anyhow::ensure!(
        faucet.balance()?.orchard_available >= 160_000,
        "faucet needs Orchard funds"
    );

    // Put the first receive beyond a complete sparse persist, a further RPC
    // chunk, and the streamed prefix of the next chunk.
    let receive_height = birthday
        + z_engine::scan::HISTORIC_PERSIST_BLOCKS
        + z_engine::scan::FETCH_CHUNK_LOCAL
        + z_engine::scan::STREAM_DECRYPT_BLOCKS
        + 100;
    mine_through(&mut height, receive_height - 1).await?;
    sync_through(&recipient, height, "note-free scan").await?;
    anyhow::ensure!(
        recipient.balance()?.total_available == 0,
        "recipient funded before long gap"
    );
    let conn = rusqlite::Connection::open_with_flags(
        &recipient.paths().data_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let rows: u32 = conn.query_row("SELECT COUNT(*) FROM blocks", [], |row| row.get(0))?;
    anyhow::ensure!(
        // Keep the recent 100-height checkpoint window plus sparse historic
        // watermarks; never regress to a row for each of the >9000 heights.
        rows <= 128,
        "expected sparse block persistence, found {rows} rows"
    );
    drop(conn);
    println!(
        "note-free span={} blocks_rows={rows}",
        height - birthday + 1
    );
    sync_through(&faucet, height, "faucet before late receive").await?;
    let funding = faucet
        .send(&auth(), &created.unified_address, 150_000, None)
        .await?;
    anyhow::ensure!(funding.len() == 1, "expected one funding transaction");
    let target = height + 15;
    mine_through(&mut height, target).await?;
    sync_through(&faucet, height, "confirm late funding").await?;
    // A shielded suffix after the first receipt used to be scanned twice by
    // finish_note_tail. It must remain unrelated to this recipient.
    let suffix = faucet.send(&auth(), &faucet_ua, 50_000, None).await?;
    anyhow::ensure!(!suffix.is_empty(), "missing shielded suffix transaction");
    let target = height + 10;
    mine_through(&mut height, target).await?;

    // Optional encrypted fixture lets subsequent correctness/performance
    // changes replay this real long-gap history without mining it again.
    // Only test-chain data is retained, and never a plaintext mnemonic.
    if let Some(path) = std::env::var_os("Z_STACK_REGTEST_REPLAY_DIR") {
        let path = std::path::PathBuf::from(path);
        anyhow::ensure!(
            path.is_absolute() && path.starts_with(std::env::temp_dir()),
            "replay fixture must be an absolute temporary-directory path"
        );
        std::fs::create_dir(&path).context("replay directory must not already exist")?;
        std::fs::copy(recipient_dir.path().join("seed.enc"), path.join("seed.enc"))?;
        std::fs::write(
            path.join("fixture.json"),
            serde_json::to_vec(&serde_json::json!({
                "network": "regtest", "birthday": birthday,
                "funding": funding[0], "min_height": height,
            }))?,
        )?;
        println!("encrypted replay fixture: {}", path.display());
    }

    // Restore into a different directory with only the in-memory mnemonic;
    // never copy the funded wallet's trees or write a plaintext seed file.
    let (restored, restored_ua) = NativeWallet::restore(
        restored_dir.path(),
        &created.mnemonic,
        Network::Regtest,
        Some(server.clone()),
        birthday,
        auth(),
        0,
    )
    .await?;
    anyhow::ensure!(
        restored_ua == created.unified_address,
        "restore changed address"
    );
    sync_through(&restored, height, "cold restore after long gap").await?;
    anyhow::ensure!(
        restored.balance()?.orchard_available == 150_000,
        "restore did not recover the note"
    );
    let received = mined_transaction(&restored, &funding[0])?;
    anyhow::ensure!(
        received.height >= receive_height,
        "first receive was not after the required note-free span"
    );

    assert_blocked_wipe_retains_wallet(&restored).await?;
    restored
        .reset_scan()
        .await
        .context("wipe scan from birthday")?;
    let conn = rusqlite::Connection::open_with_flags(
        &restored.paths().data_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let rows: u32 = conn.query_row("SELECT COUNT(*) FROM blocks", [], |row| row.get(0))?;
    anyhow::ensure!(rows == 0, "wipe retained scanned block rows");
    drop(conn);
    // No blocks row exists at this rewind height. Report a failed/no-op
    // rewind rather than claiming that the scan was reopened.
    anyhow::ensure!(
        restored.rewind_scan_to_gap().await.is_err(),
        "empty rewind falsely reported success"
    );
    sync_through(&restored, height, "birthday wipe rescan").await?;
    let before_send = restored.balance()?.orchard_available;
    anyhow::ensure!(
        before_send == 150_000,
        "wipe rescan changed recovered balance"
    );
    let sent = restored
        .send(&auth(), &faucet_ua, 20_000, None)
        .await
        .context("prove and broadcast restored Orchard note")?;
    anyhow::ensure!(sent.len() == 1, "expected one restored-note spend");
    let target = height + 10;
    mine_through(&mut height, target).await?;
    sync_through(&restored, height, "confirm restored-note spend").await?;
    sync_through(&faucet, height, "confirm return payment").await?;
    let spend = mined_transaction(&restored, &sent[0])?;
    anyhow::ensure!(spend.spent > 0, "restored spend was not mined");
    let payment = mined_transaction(&faucet, &sent[0])?;
    anyhow::ensure!(
        payment.received >= 20_000,
        "faucet did not receive the restored-note spend"
    );
    anyhow::ensure!(
        restored.balance()?.orchard_available < before_send,
        "note remained unspent"
    );
    let after_send = restored.balance()?.orchard_available;
    // Shift the birthday by the streamed prefix width. The same first receipt
    // now lies at offset 100 rather than 356 of its 1000-block chunk. This cold
    // restore also sees the receipt and its spend in one batch, without the
    // local outgoing-transaction state that could conceal a missed nullifier.
    let prefix_birthday = birthday + z_engine::scan::STREAM_DECRYPT_BLOCKS;
    let (prefix_restored, _) = NativeWallet::restore(
        prefix_restored_dir.path(),
        &created.mnemonic,
        Network::Regtest,
        Some(server),
        prefix_birthday,
        auth(),
        0,
    )
    .await?;
    sync_through(
        &prefix_restored,
        height,
        "prefix restore with same-batch spend",
    )
    .await?;
    anyhow::ensure!(
        prefix_restored.balance()?.orchard_available == after_send,
        "same-batch restore recovered a spent note or lost change"
    );
    anyhow::ensure!(
        mined_transaction(&prefix_restored, &sent[0])?.spent > 0,
        "cold restore omitted the spend transaction"
    );
    let prefix_sent = prefix_restored
        .send(&auth(), &faucet_ua, 5_000, None)
        .await
        .context("prove restored change from the prefix path")?;
    anyhow::ensure!(prefix_sent.len() == 1, "expected one prefix-restored spend");
    let target = height + 10;
    mine_through(&mut height, target).await?;
    sync_through(&prefix_restored, height, "confirm prefix-restored spend").await?;
    sync_through(&faucet, height, "confirm prefix return payment").await?;
    anyhow::ensure!(
        mined_transaction(&prefix_restored, &prefix_sent[0])?.spent > 0
            && mined_transaction(&faucet, &prefix_sent[0])?.received >= 5_000,
        "prefix-restored spend was not confirmed at both wallets"
    );
    println!("frontier skip: suffix/prefix restore, shielded suffix, same-batch spend, birthday wipe, and confirmed spends passed");
    Ok(())
}

/// Revalidate a long-gap fixture after further scanner changes, without
/// rebuilding its chain. The initial gate saves only an encrypted test seed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires encrypted fixture from frontier_skip_restore_wipe_spend"]
async fn frontier_replay_restore_wipe_spend() -> anyhow::Result<()> {
    anyhow::ensure!(enabled(), "set Z_STACK_REGTEST=1");
    let Some(path) = std::env::var_os("Z_STACK_REGTEST_REPLAY_DIR") else {
        eprintln!("skip optional replay: set Z_STACK_REGTEST_REPLAY_DIR to an encrypted fixture");
        return Ok(());
    };
    let path = std::path::PathBuf::from(path);
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path.join("fixture.json"))?)?;
    anyhow::ensure!(fixture["network"] == "regtest", "fixture must be regtest");
    let birthday = u32::try_from(fixture["birthday"].as_u64().context("fixture birthday")?)?;
    let minimum = fixture["min_height"]
        .as_u64()
        .context("fixture minimum height")?;
    let info = zebra_rpc("getblockchaininfo", serde_json::json!([]))?;
    require_regtest_chain(&info)?;
    let mut height = u32::try_from(info["blocks"].as_u64().context("missing chain height")?)?;
    anyhow::ensure!(
        u64::from(height) >= minimum && height < 100_000,
        "wrong fixture chain"
    );
    let words = z_engine::native::SeedStore::new(path).load(Some("regtest"), false)?;
    use secrecy::ExposeSecret;
    let fresh = TempDir::new()?;
    let (wallet, _) = NativeWallet::restore(
        fresh.path(),
        words.expose_secret(),
        Network::Regtest,
        Some(LightServer::LocalRegtest),
        birthday,
        auth(),
        0,
    )
    .await?;
    sync_through(&wallet, height, "replay cold restore").await?;
    let balance = wallet.balance()?.orchard_available;
    anyhow::ensure!(balance >= 20_000, "replay fixture exhausted");
    let funding = fixture["funding"]
        .as_str()
        .context("fixture funding txid")?;
    anyhow::ensure!(
        mined_transaction(&wallet, funding)?.received >= 150_000,
        "missing original note"
    );
    assert_blocked_wipe_retains_wallet(&wallet).await?;
    wallet.reset_scan().await?;
    sync_through(&wallet, height, "replay birthday wipe").await?;
    anyhow::ensure!(
        wallet.balance()?.orchard_available == balance,
        "replay wipe changed balance"
    );
    let faucet =
        z_engine::keys::account_from_mnemonic(REGTEST_FAUCET_MNEMONIC, Network::Regtest, 0)?;
    let sent = wallet
        .send(&auth(), &faucet.unified_address, 5_000, None)
        .await?;
    anyhow::ensure!(sent.len() == 1, "expected one replay spend");
    let target = height + 10;
    mine_through(&mut height, target).await?;
    sync_through(&wallet, height, "confirm replay spend").await?;
    anyhow::ensure!(
        mined_transaction(&wallet, &sent[0])?.spent > 0,
        "replay spend not mined"
    );
    // A fresh view must agree without persisted local outgoing-tx metadata.
    let check_dir = TempDir::new()?;
    let (check, _) = NativeWallet::restore(
        check_dir.path(),
        words.expose_secret(),
        Network::Regtest,
        Some(LightServer::LocalRegtest),
        birthday,
        auth(),
        0,
    )
    .await?;
    sync_through(&check, height, "replay independent spent balance").await?;
    anyhow::ensure!(
        check.balance()?.orchard_available == wallet.balance()?.orchard_available,
        "fresh replay did not recover the spent balance"
    );
    println!("frontier replay: restore, wipe, prove, confirmed spend and fresh balance passed");
    Ok(())
}

/// Ironwood on a chain that activates NU6.3 early, with an Orchard note
/// received before it: shield into Ironwood, pay a fresh address (it lands in
/// Ironwood), spend Ironwood, and spend the Orchard note across the
/// turnstile. `pnpm regtest:native:up` starts such a validator (NU6.3 at 150);
/// export the same `Z_STACK_REGTEST_NU6_3` for this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs regtest with early NU6.3 (pnpm regtest:native:up)"]
async fn ironwood_turnstile_shield_send() -> anyhow::Result<()> {
    exercise_ironwood_roundtrip().await
}

/// Exercise the same funded receive/spend/reload path after crossing NU7.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs isolated NU7 regtest; configure Z_STACK_REGTEST_NU7"]
async fn nu7_shielded_roundtrip() -> anyhow::Result<()> {
    anyhow::ensure!(enabled(), "set Z_STACK_REGTEST=1");
    let nu7 = z_engine::regtest_nu7_height().context("set Z_STACK_REGTEST_NU7")?;
    anyhow::ensure!(nu7 <= 10_000, "fixture NU7 must activate early");
    let info = zebra_rpc("getblockchaininfo", serde_json::json!([]))?;
    require_regtest_chain(&info)?;
    let mut height = u32::try_from(info["blocks"].as_u64().context("missing height")?)?;
    mine_through(&mut height, nu7 + 1).await?;
    exercise_ironwood_roundtrip().await
}

async fn exercise_ironwood_roundtrip() -> anyhow::Result<()> {
    anyhow::ensure!(enabled(), "set Z_STACK_REGTEST=1");
    let nu63 = z_engine::regtest_nu6_3_height();
    anyhow::ensure!(
        nu63 <= 10_000,
        "set Z_STACK_REGTEST_NU6_3 to the validator's early NU6.3 height"
    );
    let info = zebra_rpc("getblockchaininfo", serde_json::json!([]))?;
    require_regtest_chain(&info)?;
    let mut height = u32::try_from(info["blocks"].as_u64().context("missing chain height")?)?;
    mine_through(&mut height, 110).await?;
    let server = LightServer::LocalRegtest;
    let fresh = |birthday: u32| {
        let server = server.clone();
        async move {
            let dir = TempDir::new()?;
            let (wallet, created) = NativeWallet::create(
                dir.path(),
                Network::Regtest,
                Some(server),
                Some(birthday),
                auth(),
                0,
            )
            .await?;
            anyhow::Ok((dir, wallet, created.unified_address))
        }
    };
    let faucet_dir = TempDir::new()?;
    let (faucet, _) = NativeWallet::restore(
        faucet_dir.path(),
        REGTEST_FAUCET_MNEMONIC,
        Network::Regtest,
        Some(server.clone()),
        1,
        auth(),
        0,
    )
    .await?;
    sync_through(&faucet, height, "faucet").await?;

    // Before NU6.3: a wallet whose only note is Orchard.
    let (_holder_dir, holder, holder_ua) = fresh(height).await?;
    let orchard_note = height + 3 < nu63;
    if orchard_note {
        if faucet.balance()?.orchard_available < 200_000 {
            faucet.shield(&auth(), 100_000).await?;
            let target = height + 1;
            mine_through(&mut height, target).await?;
            sync_through(&faucet, height, "Orchard shield").await?;
        }
        faucet.send(&auth(), &holder_ua, 100_000, None).await?;
        let target = height + 1;
        mine_through(&mut height, target).await?;
        sync_through(&holder, height, "Orchard receipt").await?;
        anyhow::ensure!(
            holder.balance()?.orchard_available == 100_000,
            "pre-NU6.3 payment should be an Orchard note"
        );
    } else {
        println!("chain is already past NU6.3; skipping the Orchard receipt");
    }
    mine_through(&mut height, nu63 + 1).await?;
    sync_through(&faucet, height, "faucet after NU6.3").await?;

    let before = faucet.balance()?;
    if before.transparent_available >= 100_000 {
        faucet.shield(&auth(), 100_000).await?;
        let target = height + 1;
        mine_through(&mut height, target).await?;
        sync_through(&faucet, height, "Ironwood shield").await?;
        anyhow::ensure!(
            faucet.balance()?.ironwood_available > before.ironwood_available,
            "shielding after NU6.3 should fund Ironwood"
        );
    }

    let (_a_dir, a, a_ua) = fresh(height).await?;
    let (_b_dir, b, b_ua) = fresh(height).await?;
    faucet.send(&auth(), &a_ua, 300_000, None).await?;
    let target = height + 1;
    mine_through(&mut height, target).await?;
    sync_through(&a, height, "payment after NU6.3").await?;
    let got = a.balance()?;
    anyhow::ensure!(
        got.ironwood_available == 300_000 && got.orchard_available == 0,
        "a payment after NU6.3 should be an Ironwood note: {got:?}"
    );

    a.send(&auth(), &b_ua, 100_000, None).await?;
    let target = height + 1;
    mine_through(&mut height, target).await?;
    sync_through(&b, height, "Ironwood spend").await?;
    anyhow::ensure!(
        b.balance()?.ironwood_available == 100_000,
        "Ironwood-to-Ironwood payment"
    );

    if orchard_note {
        // A wallet left at a pre-NU6.3 tip builds with the NU6.2 branch id,
        // which the validator rejects; sync first, as the apps do.
        sync_through(&holder, height, "holder after NU6.3").await?;
        holder.send(&auth(), &b_ua, 40_000, None).await?;
        let target = height + 1;
        mine_through(&mut height, target).await?;
        sync_through(&b, height, "turnstile receipt").await?;
        sync_through(&holder, height, "turnstile spend").await?;
        anyhow::ensure!(
            b.balance()?.ironwood_available == 140_000,
            "an Orchard spend after NU6.3 should pay into Ironwood"
        );
        anyhow::ensure!(
            holder.balance()?.orchard_available < 100_000,
            "the Orchard note was not spent"
        );
    }
    println!(
        "ironwood: shield, payment and Ironwood spend passed; Orchard turnstile spend {}",
        if orchard_note { "passed" } else { "skipped" }
    );
    Ok(())
}
