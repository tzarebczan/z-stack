//! Probe (and optionally start) the local Zaino + Zakura stack.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "z-node-launcher",
    about = "Local Zaino + Zakura: probe, wait, optional compose"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print compose paths and wallet URLs.
    Status {
        #[arg(long, default_value = "testnet")]
        network: String,
    },
    /// Exit 0 if local Zaino gRPC and Zakura RPC accept TCP.
    Ready {
        #[arg(long, default_value = "testnet")]
        network: String,
    },
    /// Probe in a loop. Tries docker compose --profile node if still down.
    Up {
        #[arg(long, default_value = "testnet")]
        network: String,
        #[arg(long)]
        docker: bool,
        #[arg(long, default_value_t = 45)]
        wait_secs: u64,
    },
    Snapshot {
        #[arg(long, default_value = "pruned")]
        mode: String,
    },
    Down {
        #[arg(long)]
        regtest: bool,
    },
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn compose_regtest() -> PathBuf {
    repo_root().join("infra/compose/docker-compose.regtest.yml")
}

fn compose_operator() -> PathBuf {
    repo_root().join("infra/compose/docker-compose.yml")
}

struct Endpoints {
    zaino: &'static str,
    zakura: &'static str,
}

fn endpoints(network: &str) -> Endpoints {
    match network {
        "regtest" => Endpoints {
            zaino: "127.0.0.1:28137",
            zakura: "127.0.0.1:29232",
        },
        "mainnet" => Endpoints {
            zaino: "127.0.0.1:8138",
            zakura: "127.0.0.1:8232",
        },
        _ => Endpoints {
            zaino: "127.0.0.1:8137",
            zakura: "127.0.0.1:28232",
        },
    }
}

fn tcp_up(addr: &str) -> bool {
    let Ok(mut addrs) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(sa) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&sa, Duration::from_millis(400)).is_ok()
}

fn probe(network: &str) -> (bool, bool) {
    let e = endpoints(network);
    (tcp_up(e.zaino), tcp_up(e.zakura))
}

fn print_status(network: &str) {
    let e = endpoints(network);
    let (z, v) = probe(network);
    println!("network  {network}");
    println!("Zaino    {}  {}", e.zaino, if z { "up" } else { "down" });
    println!("Zakura   {}  {}", e.zakura, if v { "up" } else { "down" });
    if z && v {
        println!("ready    TCP up — not chain identity. Confirm with:");
        println!("          z-wallet probe --network {network} --server local");
    } else {
        println!("ready    no");
        println!("hint     start your local Zaino/Zakura, or: z-node-launcher up --network {network} --docker");
        println!("docs     docs/NODE.md");
    }
    if network == "mainnet" || network == "testnet" {
        println!("note     default endpoints: testnet Zaino :8137, mainnet Zaino :8138");
        if network == "mainnet" {
            println!("          the mainnet compose template requires production configuration");
        }
    }
}

fn docker_compose(args: &[&str]) -> Result<()> {
    let status = Command::new("docker")
        .args(args)
        .status()
        .context("docker not found on PATH")?;
    anyhow::ensure!(status.success(), "docker {:?}", args);
    Ok(())
}

fn wait_ready(network: &str, wait_secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    loop {
        let (z, v) = probe(network);
        if z && v {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(800));
    }
}

const PRUNED_MANIFEST: &str = "https://zakura.valargroup.dev/mainnet-pruned/snapshots.json";
const ARCHIVE_MANIFEST: &str = "https://zakura.valargroup.dev/mainnet/snapshots.json";

#[derive(serde::Deserialize)]
struct SnapMeta {
    height: u64,
    url: String,
    sha256: String,
    size: Option<String>,
    filename: Option<String>,
    roles: Option<Vec<String>>,
}

fn http_get(url: &str) -> Result<String> {
    let out = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .context("curl not found on PATH")?;
    anyhow::ensure!(
        out.status.success(),
        "GET {url} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).context("snapshot json is not utf-8")
}

fn snapshot_cmd(mode: &str) -> Result<()> {
    let (kind, manifest) = match mode {
        "archive" => ("archive", ARCHIVE_MANIFEST),
        _ => ("pruned", PRUNED_MANIFEST),
    };
    println!("fetching {kind} manifest {manifest}");
    let body = http_get(manifest)?;
    let snaps: Vec<SnapMeta> = serde_json::from_str(&body).context("snapshots.json")?;
    let latest = snaps
        .iter()
        .find(|s| {
            s.roles
                .as_ref()
                .is_some_and(|r| r.iter().any(|x| x == "latest"))
        })
        .or(snaps.first())
        .ok_or_else(|| anyhow::anyhow!("empty snapshot manifest"))?;
    let name = latest.filename.clone().unwrap_or_else(|| {
        latest
            .url
            .rsplit('/')
            .next()
            .unwrap_or("snapshot.tar.zst")
            .into()
    });
    println!("latest   height {}", latest.height);
    println!("size     {}", latest.size.as_deref().unwrap_or("?"));
    println!("sha256   {}", latest.sha256);
    println!("url      {}", latest.url);
    println!();
    println!("# stop zakurad first, then:");
    println!("curl -fL --retry 5 -o {name} {}", latest.url);
    println!("# verify:");
    println!(
        "python -c \"import hashlib,sys; h=hashlib.sha256(open(sys.argv[1],'rb').read()).hexdigest(); print(h); assert h==sys.argv[2]\" {name} {}",
        latest.sha256
    );
    println!("# extract (Linux/mac): mkdir -p ~/.cache/zakura && zstd -dc {name} | tar -x -C ~/.cache/zakura");
    println!("# Windows: use tar + zstd, then point Zakura at the extracted state");
    println!("docs     https://zakura.com/snapshots/");
    println!("Testnet has no published Zakura snapshots — sync from network or copy a lab node.");
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Status { network } => {
            println!("compose (operator): {}", compose_operator().display());
            println!("compose (regtest):  {}", compose_regtest().display());
            print_status(&network);
        }
        Cmd::Ready { network } => {
            print_status(&network);
            let (z, v) = probe(&network);
            if !(z && v) {
                std::process::exit(1);
            }
        }
        Cmd::Up {
            network,
            docker,
            wait_secs,
        } => {
            let (z, v) = probe(&network);
            if z && v {
                print_status(&network);
                return Ok(());
            }
            if docker && network == "mainnet" {
                anyhow::bail!(
                    "refusing docker-up mainnet: the operator compose template needs production configuration. Point z-desktop / z-wallet at your configured Zaino URL (native gRPC, no bridge token)."
                );
            }
            if docker || network == "regtest" {
                if network == "regtest" {
                    let f = compose_regtest();
                    docker_compose(&["compose", "-f", &f.to_string_lossy(), "up", "-d"])?;
                } else {
                    let f = compose_operator();
                    println!("starting operator compose --profile node ({})", f.display());
                    docker_compose(&[
                        "compose",
                        "-f",
                        &f.to_string_lossy(),
                        "--profile",
                        "node",
                        "up",
                        "-d",
                    ])?;
                }
            } else {
                println!("local stack is down. Start Zaino + Zakura, then Probe in the wallet.");
                println!("or re-run with --docker if this machine has the compose images.");
                print_status(&network);
                std::process::exit(1);
            }
            if wait_ready(&network, wait_secs) {
                print_status(&network);
            } else {
                print_status(&network);
                anyhow::bail!("still down after {wait_secs}s");
            }
        }
        Cmd::Snapshot { mode } => {
            snapshot_cmd(&mode)?;
        }
        Cmd::Down { regtest } => {
            if regtest {
                let f = compose_regtest();
                docker_compose(&["compose", "-f", &f.to_string_lossy(), "down"])?;
            } else {
                let f = compose_operator();
                docker_compose(&[
                    "compose",
                    "-f",
                    &f.to_string_lossy(),
                    "--profile",
                    "node",
                    "down",
                ])?;
            }
        }
    }
    Ok(())
}
