use clap::{Parser, Subcommand};
use enhance_pir::client::EnhancePirClient;

#[derive(Parser)]
#[command(
    name = "enhance-pir-cli",
    about = "Query the Ironwood Enhance PIR service"
)]
struct Cli {
    #[arg(long)]
    server: String,
    /// Public query domain for a dummy query; defaults to the first current shard.
    #[arg(long)]
    shard: Option<u64>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Metadata,
    Query { position: u64 },
    Dummy,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut client = EnhancePirClient::connect(&cli.server).await?;
    match cli.command {
        Command::Metadata => println!("{}", serde_json::to_string_pretty(client.manifest())?),
        Command::Query { position } => println!(
            "{}",
            hex::encode(
                client
                    .query_position_with_timing(position)
                    .await?
                    .0
                    .as_bytes()
            )
        ),
        Command::Dummy => {
            let shard = cli.shard.unwrap_or(client.manifest().coverage.shards[0].id);
            client.query_dummy(shard).await?;
            println!("dummy query completed");
        }
    }
    Ok(())
}
