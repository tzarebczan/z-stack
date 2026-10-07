//! Native CLI for z-stack.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use z_engine::native::{
    pick_local_validator, probe_validator, serve_lwd_pipe, Bridge, LwdPipeOpts, NativeWallet,
    SeedAuth, MAX_MEM_SYNC_BLOCKS, PIPE_CHANNELS, PIPE_CHUNK, PIPE_CONCURRENCY,
};
use z_engine::{format_zatoshis, LightServer, Network};

#[derive(Parser, Debug)]
#[command(name = "z-wallet", about = "z-stack native wallet (Zakura-backed)")]
struct Cli {
    #[arg(long, global = true, default_value = "./wallet-data")]
    wallet: PathBuf,

    /// Passphrase to encrypt/unlock seed.enc
    #[arg(long, global = true, env = "Z_STACK_PASSPHRASE")]
    passphrase: Option<String>,

    /// Store/load seed via Windows Credential Manager (Hello / biometrics where configured)
    #[arg(long, global = true, action = clap::ArgAction::Set, default_value_t = cfg!(windows))]
    windows_credential: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Params,
    Tip {
        #[arg(long)]
        server: Option<String>,
        #[arg(long, default_value = "mainnet")]
        network: String,
    },
    /// Probe local Zaino (light) and Zakura (JSON-RPC) independently.
    Probe {
        #[arg(long, default_value = "testnet")]
        network: String,
        #[arg(long)]
        server: Option<String>,
        #[arg(long)]
        rpc: Option<String>,
    },
    Create {
        #[arg(long, default_value = "mainnet")]
        network: String,
        #[arg(long)]
        server: Option<String>,
        /// Validator JSON-RPC (local Zakura). Independent of `--server`.
        #[arg(long)]
        rpc: Option<String>,
        #[arg(long)]
        birthday: Option<u32>,
        #[arg(long, default_value_t = 0)]
        account_index: u32,
    },
    Restore {
        #[arg(long)]
        mnemonic: Option<String>,
        /// Unified full viewing key (view-only restore; send later with send --mnemonic-stdin).
        #[arg(long)]
        ufvk: Option<String>,
        #[arg(long)]
        birthday: Option<u32>,
        /// Approx first-use date (YYYY-MM-DD) instead of --birthday.
        #[arg(long)]
        date: Option<String>,
        #[arg(long, default_value = "mainnet")]
        network: String,
        #[arg(long)]
        server: Option<String>,
        #[arg(long)]
        rpc: Option<String>,
        #[arg(long, default_value_t = 0)]
        account_index: u32,
    },
    Sync,
    /// Read-only: compare the wallet's commitment-tree roots and spendable-note
    /// witnesses with the light server's tree states. Prints heights and counts.
    VerifyTrees,
    Address {
        /// Rotate to a fresh diversified UA
        #[arg(long)]
        next: bool,
        /// Print the transparent P2PKH receiver instead of the UA
        #[arg(long)]
        transparent: bool,
    },
    Balance,
    Shield {
        /// Minimum transparent zatoshis to include (default 100_000)
        #[arg(long, default_value_t = 100_000)]
        threshold: u64,
        /// One-shot BIP-39 for a view-only wallet. Prefer `--mnemonic-stdin`.
        #[arg(long)]
        mnemonic: Option<String>,
        /// Read one-shot BIP-39 from stdin (one line).
        #[arg(long)]
        mnemonic_stdin: bool,
    },
    Send {
        #[arg(long)]
        to: String,
        /// Zatoshis. Optional when `--to` is a ZIP-321 `zcash:` URI that carries amounts.
        #[arg(long, default_value_t = 0)]
        amount: u64,
        #[arg(long)]
        memo: Option<String>,
        /// One-shot BIP-39 for a view-only wallet. Prefer `--mnemonic-stdin`.
        #[arg(long)]
        mnemonic: Option<String>,
        /// Read one-shot BIP-39 from stdin (one line).
        #[arg(long)]
        mnemonic_stdin: bool,
    },
    History {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Wipe notes, trees, history, and the compact-block cache. Keys and birthday stay.
    ResetScan {
        /// After wiping, immediately sync from birthday.
        #[arg(long)]
        and_sync: bool,
    },
    /// Loopback JSON API for the local web UI (`http://127.0.0.1:8787`).
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: String,
    },
    /// Fan-out compact-block pipe: native gRPC to Zaino, one HTTP/1.1 stream to the browser.
    /// Loopback bind only. Desktop / `z-wallet serve` talk native gRPC and do not need this.
    ///
    ///   z-wallet pipe --zaino http://127.0.0.1:8138 --network mainnet \
    ///     --rpc http://127.0.0.1:8232 --bind 127.0.0.1:1239
    Pipe {
        /// Loopback only (`127.0.0.1` / `::1`). Env: `Z_PIPE_BIND`.
        #[arg(long, env = "Z_PIPE_BIND", default_value = "127.0.0.1:1239")]
        bind: String,
        /// Zaino native gRPC URL, or `local` (mainnet `:8138`, testnet `:8137`, regtest `:28137`).
        /// Env: `ZAINO_URL`, `Z_STACK_PIPE_ZAINO`.
        #[arg(long, default_value = "local")]
        zaino: String,
        /// Env: `Z_NETWORK`.
        #[arg(long, env = "Z_NETWORK", default_value = "mainnet")]
        network: String,
        #[arg(long, default_value_t = PIPE_CONCURRENCY)]
        concurrency: usize,
        #[arg(long, default_value_t = PIPE_CHUNK)]
        chunk: u32,
        #[arg(long, default_value_t = PIPE_CHANNELS)]
        channels: usize,
        /// Loopback Zakura/Zebra JSON-RPC for sendraw + mempool. Default: probe local.
        /// Env: `ZAKURA_RPC`, `VALIDATOR_RPC`, `Z_STACK_VALIDATOR_RPC`.
        #[arg(long)]
        rpc: Option<String>,
    },
}

fn parse_network(s: &str) -> anyhow::Result<Network> {
    Network::parse(s).ok_or_else(|| anyhow::anyhow!("network must be mainnet|testnet|regtest"))
}

fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        std::env::var(k)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

/// `--zaino` wins; else `ZAINO_URL` / `Z_STACK_PIPE_ZAINO`; else `local` (per-network loopback).
fn resolve_cli_pipe_zaino(cli: String) -> String {
    if cli != "local" {
        return cli;
    }
    first_nonempty_env(&["ZAINO_URL", "Z_STACK_PIPE_ZAINO"]).unwrap_or(cli)
}

/// `--rpc` wins; else Zakura/Zebra env aliases; else `None` (pipe probes loopback).
fn resolve_cli_pipe_rpc(flag: Option<String>) -> Option<String> {
    flag.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            first_nonempty_env(&[
                "ZAKURA_RPC",
                "VALIDATOR_RPC",
                "Z_STACK_VALIDATOR_RPC",
                "Z_STACK_ZEBRA_RPC",
            ])
        })
}

fn parse_server(s: Option<String>, network: Network) -> LightServer {
    LightServer::parse(s.as_deref().unwrap_or(""), network)
}

fn take_one_shot_mnemonic(flag: Option<String>, stdin: bool) -> anyhow::Result<Option<String>> {
    if stdin {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let words = line.split_whitespace().collect::<Vec<_>>().join(" ");
        anyhow::ensure!(!words.is_empty(), "stdin had no mnemonic");
        return Ok(Some(words));
    }
    if let Some(m) = flag.map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")) {
        anyhow::ensure!(!m.is_empty(), "--mnemonic is empty");
        return Ok(Some(m));
    }
    Ok(std::env::var("Z_STACK_MNEMONIC")
        .ok()
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let Cli {
        wallet: wallet_dir,
        passphrase,
        windows_credential,
        cmd,
    } = Cli::parse();
    let auth = SeedAuth {
        passphrase,
        windows_credential,
        mnemonic: None,
        unlock_policy: z_engine::native::UnlockPolicy::Session,
    };

    match cmd {
        Cmd::Params => {
            let (spend, output) = z_engine::params::ensure_sapling_params()?;
            println!("Sapling spend:  {}", spend.display());
            println!("Sapling output: {}", output.display());
            println!("(Orchard/Ironwood proofs do not use these files.)");
        }
        Cmd::Tip { server, network } => {
            let network = parse_network(&network)?;
            let server = parse_server(server, network);
            let tip = NativeWallet::fetch_tip(&server).await?;
            println!("{} tip = {tip}", server.as_url());
        }
        Cmd::Probe {
            network,
            server,
            rpc,
        } => {
            let network = parse_network(&network)?;
            let server = parse_server(server.or_else(|| Some("local".into())), network);
            let light = NativeWallet::probe_light(&server).await;
            println!(
                "light    {}  ok={} chain={} tip={:?} t-scan={} {}",
                light.url,
                light.ok,
                light.chain,
                light.tip,
                light.t_scan,
                light.error.as_deref().unwrap_or("")
            );
            let validator = match rpc {
                Some(u) => probe_validator(&u),
                None => pick_local_validator(network)
                    .unwrap_or_else(|| probe_validator(LightServer::local_validator_rpc(network))),
            };
            println!(
                "validator {}  ok={} chain={} height={:?} {}",
                validator.url,
                validator.ok,
                validator.chain,
                validator.height,
                validator
                    .subversion
                    .as_deref()
                    .or(validator.error.as_deref())
                    .unwrap_or("")
            );
        }
        Cmd::Create {
            network,
            server,
            rpc,
            birthday,
            account_index,
        } => {
            if auth.passphrase.is_none() && !auth.windows_credential {
                anyhow::bail!("provide --passphrase and/or --windows-credential");
            }
            let network = parse_network(&network)?;
            let server = parse_server(server, network);
            let (mut wallet, created) = NativeWallet::create(
                &wallet_dir,
                network,
                Some(server),
                birthday,
                auth,
                account_index,
            )
            .await?;
            wallet.set_validator_rpc(rpc)?;
            println!("wallet:   {}", wallet.paths().root.display());
            println!("server:   {}", wallet.server_url());
            if let Some(r) = wallet.validator_rpc_url() {
                println!("rpc:      {r}");
            }
            println!("birthday: {}", created.birthday_height);
            println!("address:  {}", created.unified_address);
            println!("ufvk:     {}", created.ufvk);
            println!("mnemonic: {}", created.mnemonic);
            println!("Seed stored encrypted (seed.enc) and/or OS keychain (Hello / Keychain).");
            println!("Back up the mnemonic offline; it will not be shown again by the CLI.");
        }
        Cmd::Restore {
            mnemonic,
            ufvk,
            birthday,
            date,
            network,
            server,
            rpc,
            account_index,
        } => {
            let network = parse_network(&network)?;
            let server = parse_server(server, network);
            let _ = MAX_MEM_SYNC_BLOCKS;
            let tip = NativeWallet::fetch_tip(&server).await.unwrap_or(1);
            let birthday = match (birthday, date.as_deref()) {
                (Some(h), _) => h,
                (None, Some(d)) => z_engine::height_from_date(d, tip)?,
                (None, None) => {
                    anyhow::bail!("restore needs --birthday HEIGHT or --date YYYY-MM-DD")
                }
            };
            let (mut wallet, ua) = if let Some(ufvk) = ufvk {
                NativeWallet::restore_ufvk(
                    &wallet_dir,
                    &ufvk,
                    network,
                    Some(server),
                    birthday,
                    account_index,
                )
                .await?
            } else {
                let mnemonic = mnemonic
                    .ok_or_else(|| anyhow::anyhow!("restore needs --mnemonic or --ufvk"))?;
                if auth.passphrase.is_none() && !auth.windows_credential {
                    anyhow::bail!("provide --passphrase and/or --windows-credential");
                }
                NativeWallet::restore(
                    &wallet_dir,
                    &mnemonic,
                    network,
                    Some(server),
                    birthday,
                    auth,
                    account_index,
                )
                .await?
            };
            wallet.set_validator_rpc(rpc)?;
            println!("wallet:   {}", wallet.paths().root.display());
            println!("birthday: {birthday}");
            println!("address:  {ua}");
            println!("server:   {}", wallet.server_url());
            if wallet.is_view_only() {
                println!("mode:     view-only (send/shield --mnemonic-stdin)");
            }
        }
        Cmd::Sync => {
            let wallet = NativeWallet::open(&wallet_dir)?;
            let (tip, progress) = wallet.sync().await?;
            println!("synced to height {tip} ({})", progress.message);
        }
        Cmd::VerifyTrees => {
            let wallet = NativeWallet::open(&wallet_dir)?;
            let report = wallet.verify_commitment_trees().await?;
            println!("anchor:    {:?}", report.anchor_height);
            for r in &report.roots {
                let state = match r.matches {
                    Some(true) => "match",
                    Some(false) => "MISMATCH",
                    None => "no checkpoint",
                };
                println!("root      {:<9} @{}  {state}", r.pool, r.height);
            }
            for w in &report.witnesses {
                println!("witnesses {:<9} {}/{} match", w.pool, w.ok, w.checked);
            }
            if !report.all_match() {
                anyhow::bail!("commitment trees disagree with the light server");
            }
        }
        Cmd::Address { next, transparent } => {
            let wallet = NativeWallet::open(&wallet_dir)?;
            if transparent {
                match wallet.transparent_address()? {
                    Some(t) => println!("{t}"),
                    None => anyhow::bail!("current unified address has no transparent receiver"),
                }
            } else {
                let addr = if next {
                    wallet.next_unified_address()?
                } else {
                    wallet.unified_address()?
                };
                println!("{addr}");
            }
        }
        Cmd::Balance => {
            let wallet = NativeWallet::open(&wallet_dir)?;
            let b = wallet.balance()?;
            let line = |label: &str, zat: u64| {
                println!("{label:<13}{zat} zat / {} ZEC", format_zatoshis(zat));
            };
            line("transparent:", b.transparent_available);
            line("t-pending:", b.transparent_pending);
            line("ironwood:", b.ironwood_available);
            line("sapling:", b.sapling_available);
            line("orchard:", b.orchard_available);
            line("o-pending:", b.orchard_pending);
            line("total:", b.total_available);
            line("pending:", b.total_pending);
        }
        Cmd::Shield {
            threshold,
            mnemonic,
            mnemonic_stdin,
        } => {
            let mut auth = auth;
            auth.mnemonic = take_one_shot_mnemonic(mnemonic, mnemonic_stdin)?;
            if auth.passphrase.is_none() && !auth.windows_credential && auth.mnemonic.is_none() {
                anyhow::bail!("provide --passphrase, --windows-credential, or --mnemonic-stdin");
            }
            let wallet = NativeWallet::open(&wallet_dir)?;
            let txids = wallet.shield(&auth, threshold).await?;
            for t in txids {
                println!("shielded txid: {t}");
            }
        }
        Cmd::Send {
            to,
            amount,
            memo,
            mnemonic,
            mnemonic_stdin,
        } => {
            let mut auth = auth;
            auth.mnemonic = take_one_shot_mnemonic(mnemonic, mnemonic_stdin)?;
            if auth.passphrase.is_none() && !auth.windows_credential && auth.mnemonic.is_none() {
                anyhow::bail!("provide --passphrase, --windows-credential, or --mnemonic-stdin");
            }
            let wallet = NativeWallet::open(&wallet_dir)?;
            let txids = wallet.send(&auth, &to, amount, memo.as_deref()).await?;
            for t in txids {
                println!("sent txid: {t}");
            }
        }
        Cmd::History { limit } => {
            let wallet = NativeWallet::open(&wallet_dir)?;
            for e in wallet.history(limit)? {
                println!(
                    "{} {:>8} {:>12} zat  spent {} recv {} fee {:?}",
                    e.status(),
                    e.mined_height
                        .map(|h| h.to_string())
                        .unwrap_or_else(|| "-".into()),
                    e.account_delta_zat,
                    e.spent_zat,
                    e.received_zat,
                    e.fee_zat
                );
                println!("  {}", e.txid);
            }
        }
        Cmd::ResetScan { and_sync } => {
            let wallet = NativeWallet::open(&wallet_dir)?;
            let birthday = wallet.birthday_height();
            wallet.reset_scan().await?;
            println!("scan database cleared (birthday {birthday}); keys and seed kept");
            if and_sync {
                let (tip, progress) = wallet.sync().await?;
                println!("synced to height {tip} ({})", progress.message);
            } else {
                println!("run `z-wallet sync` to trial-decrypt from birthday");
            }
        }
        Cmd::Serve { bind } => {
            let need_secret = auth.passphrase.is_none() && !auth.windows_credential;
            let bridge = Bridge::new(wallet_dir, auth);
            println!("bridge http://{bind}  (loopback only; CORS 127.0.0.1:5173 / localhost:5173)");
            println!("token   {}", bridge.token());
            println!("  send Authorization: Bearer <token>  (GET / and /health are open)");
            println!(
                "  /lwd/tip /lwd/blocks /lwd/sendraw /lwd/utxos /lwd/mempool /lwd/mine = WASM block pipe"
            );
            println!("  POST /scan/reset = wipe scan db (keys stay); then POST /sync");
            if need_secret {
                println!(
                    "  native create/restore/spend need --passphrase and/or --windows-credential"
                );
            }
            bridge.serve(&bind).await?;
        }
        Cmd::Pipe {
            bind,
            zaino,
            network,
            concurrency,
            chunk,
            channels,
            rpc,
        } => {
            let network = parse_network(&network)?;
            let zaino = LightServer::parse(&resolve_cli_pipe_zaino(zaino), network);
            let rpc = resolve_cli_pipe_rpc(rpc);
            println!(
                "lwd-pipe http://{bind}  → {}  ({} gRPC streams × {} channels, {}-block chunks)",
                zaino.as_url(),
                concurrency,
                channels,
                chunk
            );
            println!("  WASM: Block transport = Zaino pipe, URL {bind} (no bridge token)");
            if let Some(ref u) = rpc {
                println!("  validator RPC {u} (sendraw / mempool)");
            } else {
                println!(
                    "  validator RPC: probe loopback Zakura/Zebra (or set --rpc / ZAKURA_RPC)"
                );
            }
            serve_lwd_pipe(LwdPipeOpts {
                bind,
                zaino,
                network,
                concurrency,
                chunk,
                channels,
                rpc,
            })
            .await?;
        }
    }
    Ok(())
}
