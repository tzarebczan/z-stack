//! Replay a restore through the web (WASM) wallet store natively, and time each
//! phase. Native code is several times faster than WASM, but the split between
//! trial decryption, ordered ingest, snapshots and tree hashing is the same.
//!
//! ```text
//! cargo run --release -p z-engine --example web_replay -- <dir> <ufvk-file> <birthday>
//! ```
//!
//! `<dir>` holds `treestate.json` (GetTreeState at birthday - 1), optional
//! `roots-{sapling,orchard,ironwood}.json` (`{"startIndex":N,"roots":[...]}`),
//! and `blobs/*.bin` (length-delimited compact blocks, as `/lwd/blocks` serves
//! them), applied in file-name order. The viewing key is read from a file so it
//! never appears in a process listing. `REPLAY_THREADS` sizes the Rayon pool;
//! `REPLAY_SNAPSHOT_EVERY` (default 8) matches the web checkpoint cadence.
//! `REPLAY_SAVE_SNAPSHOT` / `REPLAY_FROM_SNAPSHOT` split scan from finalize,
//! `REPLAY_REWIND` rewinds a loaded snapshot first, and `REPLAY_VERIFY` checks
//! the built trees against a `GetTreeState` JSON file.

use std::time::Instant;
use z_engine::web::WebWallet;
use z_engine::Network;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        return Err("usage: web_replay <dir> <ufvk-file> <birthday>".into());
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let ufvk = std::fs::read_to_string(&args[2])?.trim().to_string();
    let birthday: u32 = args[3].parse()?;
    if let Some(n) = std::env::var("REPLAY_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()?;
    }
    let snapshot_every: usize = std::env::var("REPLAY_SNAPSHOT_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    println!("threads {}", rayon::current_num_threads());

    let total = Instant::now();
    // REPLAY_FROM_SNAPSHOT skips the scan and times finalize alone;
    // REPLAY_SAVE_SNAPSHOT writes the post-scan snapshot for that.
    if let Ok(path) = std::env::var("REPLAY_FROM_SNAPSHOT") {
        let t = Instant::now();
        let mut w = WebWallet::from_snapshot(&std::fs::read(path)?)?;
        println!("load snapshot {:.2}s", t.elapsed().as_secs_f64());
        // REPLAY_REWIND rewinds as a reorg would before finalizing; pair it
        // with a REPLAY_VERIFY tree state at the height it lands on.
        if let Some(h) = std::env::var("REPLAY_REWIND")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            let t = Instant::now();
            let kept = w.rewind_to_height(h)?;
            println!("rewind to {kept} {:.2}s", t.elapsed().as_secs_f64());
        }
        return finalize(&mut w, total);
    }
    let mut w = WebWallet::from_ufvk(Network::Mainnet, &ufvk, birthday, 0)?;
    let t = Instant::now();
    // Same order as the SDK: the birthday frontier decides where roots start.
    w.apply_tree_state_json(&std::fs::read_to_string(dir.join("treestate.json"))?)?;
    for pool in ["sapling", "orchard", "ironwood"] {
        let path = dir.join(format!("roots-{pool}.json"));
        if let Ok(json) = std::fs::read_to_string(&path) {
            let n = w.apply_subtree_roots_json(pool, &json)?;
            println!("roots {pool}: {n} (start {})", w.subtree_roots_start(pool));
        }
    }
    println!("seed {:.3}s", t.elapsed().as_secs_f64());

    let mut blobs: Vec<_> = std::fs::read_dir(dir.join("blobs"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    blobs.sort();
    let (mut apply_s, mut snap_s, mut snaps) = (0f64, 0f64, 0usize);
    let (mut notes, mut spends) = (0u32, 0u32);
    for (i, path) in blobs.iter().enumerate() {
        let blob = std::fs::read(path)?;
        let t = Instant::now();
        let d = w.apply_compact_blocks_summary(&blob)?;
        let dt = t.elapsed().as_secs_f64();
        apply_s += dt;
        notes += d.notes_found;
        spends += d.spends_found;
        println!(
            "blob {i:3} -> {} apply {:7.1}ms ({} bytes)",
            d.height,
            dt * 1e3,
            blob.len()
        );
        if snapshot_every > 0 && (i + 1) % snapshot_every == 0 {
            let t = Instant::now();
            let bytes = w.to_snapshot()?;
            let dt = t.elapsed().as_secs_f64();
            snap_s += dt;
            snaps += 1;
            println!("  snapshot {:.1}ms ({} bytes)", dt * 1e3, bytes.len());
        }
    }
    println!(
        "scan: {} blobs apply {apply_s:.2}s ({:.1}ms/blob) snapshots {snaps} x {:.1}ms; notes {notes} spends {spends}",
        blobs.len(),
        apply_s * 1e3 / blobs.len().max(1) as f64,
        snap_s * 1e3 / snaps.max(1) as f64
    );

    if let Ok(path) = std::env::var("REPLAY_SAVE_SNAPSHOT") {
        std::fs::write(path, w.to_snapshot()?)?;
    }
    finalize(&mut w, total)
}

fn finalize(w: &mut WebWallet, total: Instant) -> Result<(), Box<dyn std::error::Error>> {
    let t = Instant::now();
    let mut last = (0u64, 0u64);
    w.finalize_scan_trees_ticking(|tick| {
        if !tick.done {
            last = (tick.hashed.max(last.0), tick.total.max(last.1));
        }
    })?;
    println!(
        "finalize {:.2}s (hashed {} of {} leaves)",
        t.elapsed().as_secs_f64(),
        last.0,
        last.1
    );
    let t = Instant::now();
    let bytes = w.to_snapshot()?;
    println!(
        "final snapshot {:.1}ms ({} bytes)",
        t.elapsed().as_secs_f64() * 1e3,
        bytes.len()
    );
    // GetTreeState at the last scanned height, fetched alongside the blobs.
    if let Some(dir) = std::env::var_os("REPLAY_VERIFY") {
        let json = std::fs::read_to_string(std::path::Path::new(&dir))?;
        let t = Instant::now();
        let n = w.verify_trees_against(&json)?;
        println!(
            "verify: roots match GetTreeState, {n} note witnesses valid ({:.2}s)",
            t.elapsed().as_secs_f64()
        );
    }
    // A reload must come back with the hashed trees (no re-finalize).
    let t = Instant::now();
    let reloaded = WebWallet::from_snapshot(&bytes)?;
    println!(
        "reload {:.1}ms, trees live {}",
        t.elapsed().as_secs_f64() * 1e3,
        reloaded.sinsemilla_live()
    );
    if let Some(dir) = std::env::var_os("REPLAY_VERIFY") {
        let json = std::fs::read_to_string(std::path::Path::new(&dir))?;
        let n = reloaded.verify_trees_against(&json)?;
        println!("verify reloaded: roots match GetTreeState, {n} note witnesses valid");
    }
    let b = w.balance();
    println!(
        "balance {:?} history {} total {:.2}s",
        b,
        w.history(1000).len(),
        total.elapsed().as_secs_f64()
    );
    Ok(())
}
