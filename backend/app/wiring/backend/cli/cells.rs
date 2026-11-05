//! Operator commands for a cell's identity and allocation ranges.
use anyhow::Result;
use clap::Subcommand;
use codeyard_db_core::DbPool;
use codeyard_support::cells::Prefix;
use std::path::Path;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Operate the directory database without opening a cell database.
    Directory {
        #[command(subcommand)]
        action: DirectoryCommand,
    },
    /// Reserve this cell's ID range before adding it to the directory; stop its servers first.
    Initialize {
        #[arg(long)]
        prefix: Prefix,
    },
}

pub async fn run(command: &Command, pool: &DbPool, state: &Path) -> Result<()> {
    let _state = codeyard_support::fs::lock_state_dir(state)?;
    let maintenance = codeyard_db_cluster::cluster::maintenance_lock(pool).await?;
    let result = match command {
        Command::Initialize { prefix } => codeyard_db_core::cells::initialize(pool, *prefix).await,
        Command::Directory { .. } => anyhow::bail!("Directory commands use their own database"),
    };
    maintenance.release().await;
    result?;
    println!("Cell {} initialized.", pool.cell_prefix());
    Ok(())
}

#[derive(Debug, Subcommand)]
pub enum DirectoryCommand {
    /// Initialize the directory's separate PostgreSQL database.
    Initialize,
    /// Serve authenticated directory lookups and claims with the node key.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7445")]
        address: std::net::SocketAddr,
        #[arg(long, default_value_t = 10)]
        exchange_seconds: u64,
        #[arg(long, default_value_t = 4194304)]
        maximum_bytes: usize,
        #[arg(long, default_value_t = 128)]
        maximum_connections: usize,
    },
    /// Grant or revoke a router's read-only directory key.
    RouterKey {
        #[arg(long)]
        public_key: String,
        #[arg(long)]
        revoke: bool,
    },
    /// Register or replace a cell and its public node keys in the directory.
    Register {
        #[arg(long)]
        prefix: Prefix,
        #[arg(long)]
        name: String,
        #[arg(long)]
        region: String,
        #[arg(long, required = true, num_args = 1.., value_delimiter = ',')]
        web_url: Vec<String>,
        #[arg(long, num_args = 1.., value_delimiter = ',')]
        ssh_address: Vec<String>,
        #[arg(long, required = true, num_args = 1.., value_delimiter = ',')]
        node_key: Vec<String>,
        /// Accept new accounts; otherwise keep the cell closed.
        #[arg(long)]
        open: bool,
        #[arg(long, default_value_t = 4_000_000_000_000_i64)]
        maximum_database_bytes: i64,
        #[arg(long, default_value_t = 10_000_i64)]
        maximum_write_rate: i64,
    },
    /// List one bounded page of cells as JSON.
    List {
        #[arg(long)]
        after: Option<Prefix>,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(i64).range(1..=100))]
        limit: i64,
    },
}

/// Runs before opening a cell database or touching its state directory.
pub async fn run_directory(command: &DirectoryCommand, state: &Path) -> Result<()> {
    use codeyard_model::cells::{Cell, State};
    let url = std::env::var("DIRECTORY_DATABASE_URL").map_err(|_| {
        anyhow::anyhow!("DIRECTORY_DATABASE_URL is required and must be valid UTF-8")
    })?;
    let directory = codeyard_db_directory::Directory::connect(&url, Default::default()).await?;
    let result = async {
        directory.migrate().await?;
        match command {
            DirectoryCommand::Initialize => println!("Directory initialized."),
            DirectoryCommand::RouterKey { public_key, revoke } => {
                directory.set_router_key(public_key, !revoke).await?;
                println!("Router key updated.");
            }
            DirectoryCommand::Serve {
                address,
                exchange_seconds,
                maximum_bytes,
                maximum_connections,
            } => {
                codeyard_support::fs::private_dir(state)?;
                let _state = codeyard_support::fs::lock_state_dir(state)?;
                let key = codeyard_support::node_identity::load_or_create(state)?;
                let config = codeyard_cells_router::directory::Config {
                    exchange_seconds: *exchange_seconds,
                    maximum_bytes: *maximum_bytes,
                    maximum_connections: *maximum_connections,
                };
                let mut listener = codeyard_cells_router::directory::Listener::bind(
                    directory.clone(),
                    &key,
                    *address,
                    config,
                )
                .await?;
                println!(
                    "Directory listening on {} as {}.",
                    listener.address,
                    key.public_hex()
                );
                let signal = tokio::select! {
                    signal = crate::backend::node::runtime::shutdown_signal() => Some(signal),
                    _ = listener.ended() => None,
                };
                if let Some(signal) = signal {
                    listener.stop().await;
                    signal?;
                } else {
                    anyhow::bail!("Directory listener stopped unexpectedly");
                }
            }
            DirectoryCommand::Register {
                prefix,
                name,
                region,
                web_url,
                ssh_address,
                node_key,
                open,
                maximum_database_bytes,
                maximum_write_rate,
            } => {
                directory
                    .register_cell(Cell {
                        id: *prefix,
                        name: name.clone(),
                        region: region.clone(),
                        state: if *open { State::Open } else { State::Closed },
                        web_urls: web_url.clone(),
                        ssh_addresses: ssh_address.clone(),
                        node_keys: node_key.clone(),
                        database_bytes: 0,
                        write_rate: 0,
                        maximum_database_bytes: *maximum_database_bytes,
                        maximum_write_rate: *maximum_write_rate,
                    })
                    .await?;
                println!("Cell {prefix} registered.");
            }
            DirectoryCommand::List { after, limit } => {
                println!(
                    "{}",
                    serde_json::to_string(&directory.cells(*after, *limit).await?)?
                );
            }
        }
        anyhow::Ok(())
    }
    .await;
    directory.close().await;
    result
}
