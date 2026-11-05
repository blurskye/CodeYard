//! Operator commands for a cell's identity and allocation ranges.
use anyhow::Result;
use clap::Subcommand;
use codeyard_db_core::DbPool;
use codeyard_support::cells::Prefix;
use std::path::Path;

#[derive(Debug, Subcommand)]
pub enum Command {
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
    };
    maintenance.release().await;
    result?;
    println!("Cell {} initialized.", pool.cell_prefix());
    Ok(())
}
