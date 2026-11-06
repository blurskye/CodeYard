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
    /// Publish a bounded page of native names, emails, keys and object homes; stop cell servers first.
    Publish {
        #[arg(long)]
        directory_address: std::net::SocketAddr,
        #[arg(long)]
        directory_key: String,
        #[arg(long, value_parser = ["user", "organization", "repository"])]
        kind: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 250)]
        limit: i64,
    },
    /// Renew an 80%-used range through its pinned directory; stop cell servers first.
    RenewRange {
        #[arg(long)]
        directory_address: std::net::SocketAddr,
        #[arg(long)]
        directory_key: String,
    },
    /// Reserve this cell's ID range before adding it to the directory; stop its servers first.
    Initialize {
        #[arg(long)]
        prefix: Prefix,
    },
    /// Queue a page of this cell's repositories, with their issues and pull requests, for the
    /// search index; the running servers deliver them. Use after the index was emptied.
    SearchReindex {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(i64).range(1..=1000))]
        limit: i64,
    },
}

pub async fn run(command: &Command, pool: &DbPool, state: &Path) -> Result<()> {
    if let Command::SearchReindex { after, limit } = command {
        let (next, count) =
            codeyard_db_core::cells::search::queue_all(pool, *after, *limit).await?;
        println!("{}", serde_json::json!({ "after": next, "count": count }));
        return Ok(());
    }
    let _state = codeyard_support::fs::lock_state_dir(state)?;
    let maintenance = codeyard_db_cluster::cluster::maintenance_lock(pool).await?;
    let result = match command {
        Command::Initialize { prefix } => codeyard_db_core::cells::initialize(pool, *prefix).await,
        Command::Publish {
            directory_address,
            directory_key,
            kind,
            after,
            limit,
        } => {
            async {
                let client = codeyard_cells_router::directory::Client::new(
                    codeyard_support::node_identity::load_or_create(state)?,
                    *directory_address,
                    codeyard_support::node_identity::parse_public_key(directory_key)?,
                    Default::default(),
                )?;
                pool.set_directory(std::sync::Arc::new(client))?;
                let DbPool::Postgres(pool) = pool else {
                    unreachable!("directory attachment requires PostgreSQL")
                };
                let kind = codeyard_model::cells::ObjectKind::parse(kind)
                    .ok_or_else(|| anyhow::anyhow!("Invalid object kind"))?;
                let (next, count) =
                    codeyard_db_core::cells::directory::publish_native(pool, kind, *after, *limit)
                        .await?;
                println!("{}", serde_json::json!({ "after": next, "count": count }));
                Ok(())
            }
            .await
        }
        Command::RenewRange {
            directory_address,
            directory_key,
        } => {
            async {
                let previous = pool.cell_prefix();
                let last_id = codeyard_db_core::cells::highest_id(pool).await?;
                anyhow::ensure!(
                    previous.needs_next_prefix(last_id),
                    "no cell sequence has reached 80% use"
                );
                let key = codeyard_support::node_identity::load_or_create(state)?;
                let directory = codeyard_cells_router::directory::Client::new(
                    key,
                    *directory_address,
                    codeyard_support::node_identity::parse_public_key(directory_key)?,
                    Default::default(),
                )?;
                let reply = directory
                    .request(&codeyard_cells_router::directory::Request::NextPrefix {
                        previous,
                        last_id,
                    })
                    .await?;
                let codeyard_cells_router::directory::Reply::Prefix(prefix) = reply else {
                    anyhow::bail!("directory returned an invalid prefix reservation");
                };
                codeyard_db_core::cells::renew(pool, previous, prefix).await
            }
            .await
        }
        Command::Directory { .. } => anyhow::bail!("Directory commands use their own database"),
        Command::SearchReindex { .. } => unreachable!("reindexing runs beside the servers"),
    };
    maintenance.release().await;
    result?;
    if matches!(command, Command::Publish { .. }) {
        return Ok(());
    }
    if matches!(command, Command::RenewRange { .. }) {
        println!(
            "Cell {} now issues prefix {}.",
            pool.cell_id(),
            pool.cell_prefix()
        );
    } else {
        println!("Cell {} initialized.", pool.cell_prefix());
    }
    Ok(())
}

#[derive(Debug, Subcommand)]
pub enum DirectoryCommand {
    /// Reserve a successor for a cell range that has reached 80% use.
    AllocatePrefix {
        #[arg(long)]
        node_key: String,
        #[arg(long)]
        previous: Prefix,
        #[arg(long)]
        last_id: i64,
    },
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
        #[arg(long, num_args = 1.., value_delimiter = ',')]
        rpc_address: Vec<String>,
        #[arg(long, required = true, num_args = 1.., value_delimiter = ',')]
        node_key: Vec<String>,
        /// Accept new accounts; otherwise keep the cell closed.
        #[arg(long)]
        open: bool,
        #[arg(long, default_value_t = 4_000_000_000_000_i64)]
        maximum_database_bytes: i64,
        #[arg(long, default_value_t = 10_000_i64)]
        maximum_write_rate: i64,
        /// A web node in another region that serves this cell's reads, as `region=url`.
        /// Repeat it for each node.
        #[arg(long)]
        read_region: Vec<String>,
    },
    /// Serve an organization's reads from its home cell's web nodes in a region, or stop.
    Presence {
        #[arg(long)]
        organization: String,
        #[arg(long)]
        region: String,
        #[arg(long)]
        remove: bool,
    },
    /// Update every cell in waves: a canary, then 1%, 10% and the rest. `--apply` runs once per
    /// cell with its prefix and name, and each cell must answer `/health` before the next
    /// wave. The first failure stops the rollout and leaves later cells unchanged.
    Rollout {
        #[arg(long)]
        apply: std::path::PathBuf,
        #[arg(long)]
        canary: Option<Prefix>,
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=86400))]
        apply_seconds: u64,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=3600))]
        health_seconds: u64,
    },
    /// Empty the global search index; every cell then runs `cell search-reindex`.
    SearchReset,
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
            DirectoryCommand::AllocatePrefix {
                node_key,
                previous,
                last_id,
            } => {
                println!(
                    "{}",
                    directory.next_prefix(node_key, *previous, *last_id).await?
                );
            }
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
                rpc_address,
                node_key,
                open,
                maximum_database_bytes,
                maximum_write_rate,
                read_region,
            } => {
                let mut read_regions: Vec<codeyard_model::cells::ReadRegion> = Vec::new();
                for value in read_region {
                    let (region, url) = value
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("--read-region takes region=url"))?;
                    match read_regions.iter_mut().find(|read| read.region == region) {
                        Some(read) => read.web_urls.push(url.to_owned()),
                        None => read_regions.push(codeyard_model::cells::ReadRegion {
                            region: region.to_owned(),
                            web_urls: vec![url.to_owned()],
                        }),
                    }
                }
                directory
                    .register_cell(Cell {
                        id: *prefix,
                        name: name.clone(),
                        region: region.clone(),
                        state: if *open { State::Open } else { State::Closed },
                        web_urls: web_url.clone(),
                        ssh_addresses: ssh_address.clone(),
                        rpc_addresses: rpc_address.clone(),
                        node_keys: node_key.clone(),
                        database_bytes: 0,
                        write_rate: 0,
                        maximum_database_bytes: *maximum_database_bytes,
                        maximum_write_rate: *maximum_write_rate,
                        read_regions,
                    })
                    .await?;
                println!("Cell {prefix} registered.");
            }
            DirectoryCommand::Presence {
                organization,
                region,
                remove,
            } => {
                let key = codeyard_model::cells::Key::new(
                    codeyard_model::cells::KeyKind::Name,
                    organization,
                )?;
                let location = directory
                    .lookup(&key)
                    .await?
                    .filter(|location| {
                        location.object.kind == codeyard_model::cells::ObjectKind::Organization
                    })
                    .ok_or_else(|| anyhow::anyhow!("No organization is named {organization}"))?;
                let changed = directory
                    .set_presence(location.object.id, region, !remove)
                    .await?;
                println!(
                    "{}",
                    serde_json::json!({
                        "organization": organization,
                        "region": region,
                        "present": !remove,
                        "changed": changed,
                        "regions": directory.presence(location.object.id).await?,
                    })
                );
            }
            DirectoryCommand::Rollout {
                apply,
                canary,
                apply_seconds,
                health_seconds,
            } => {
                rollout(
                    &directory,
                    apply,
                    *canary,
                    std::time::Duration::from_secs(*apply_seconds),
                    std::time::Duration::from_secs(*health_seconds),
                )
                .await?
            }
            DirectoryCommand::SearchReset => {
                directory.reset_search().await?;
                println!("Search index emptied.");
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

/// The rollout waves for `count` cells: one canary, then up to 1%, 10% and all of them.
fn waves(count: usize) -> Vec<usize> {
    let mut sizes = Vec::new();
    let mut done = 0;
    for target in [1, count.div_ceil(100), count.div_ceil(10), count] {
        let target = target.min(count);
        if target > done {
            sizes.push(target - done);
            done = target;
        }
    }
    sizes
}

async fn rollout(
    directory: &codeyard_db_directory::Directory,
    apply: &Path,
    canary: Option<Prefix>,
    apply_limit: std::time::Duration,
    health_limit: std::time::Duration,
) -> Result<()> {
    use codeyard_model::cells::State;
    const MAXIMUM_CELLS: usize = 65_536;
    let mut cells = Vec::new();
    let mut after = None;
    loop {
        let page = directory.cells(after, 100).await?;
        after = page.last().map(|cell| cell.id);
        let last = page.len() < 100;
        cells.extend(
            page.into_iter()
                .filter(|cell| cell.state != State::Disabled),
        );
        anyhow::ensure!(cells.len() <= MAXIMUM_CELLS, "Too many cells to roll out");
        if last {
            break;
        }
    }
    if let Some(canary) = canary {
        let index = cells
            .iter()
            .position(|cell| cell.id == canary)
            .ok_or_else(|| anyhow::anyhow!("The canary {canary} is not an enabled cell"))?;
        let cell = cells.remove(index);
        cells.insert(0, cell);
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut cells = cells.into_iter();
    for (wave, size) in waves(cells.len()).into_iter().enumerate() {
        let wave = wave + 1;
        let batch: Vec<_> = cells.by_ref().take(size).collect();
        for cell in &batch {
            let applied = tokio::time::timeout(
                apply_limit,
                tokio::process::Command::new(apply)
                    .arg(cell.id.to_string())
                    .arg(&cell.name)
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .status(),
            )
            .await;
            let failure = match applied {
                Ok(Ok(status)) if status.success() => None,
                Ok(Ok(status)) => Some(format!("the update exited with {status}")),
                Ok(Err(error)) => Some(format!("the update did not start: {error}")),
                Err(_) => Some("the update ran past its time limit".to_owned()),
            };
            if let Some(reason) = failure {
                println!(
                    "{}",
                    serde_json::json!({"wave": wave, "cell": cell.id, "name": cell.name, "result": "failed", "reason": reason})
                );
                anyhow::bail!("Rollout stopped at wave {wave}; later cells were not changed");
            }
        }
        for cell in &batch {
            let deadline = tokio::time::Instant::now() + health_limit;
            'nodes: for url in &cell.web_urls {
                loop {
                    let healthy = client
                        .get(format!("{url}/health"))
                        .send()
                        .await
                        .is_ok_and(|response| response.status().is_success());
                    if healthy {
                        continue 'nodes;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        println!(
                            "{}",
                            serde_json::json!({"wave": wave, "cell": cell.id, "name": cell.name, "result": "failed", "reason": format!("{url} is not healthy")})
                        );
                        anyhow::bail!(
                            "Rollout stopped at wave {wave}; later cells were not changed"
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
            println!(
                "{}",
                serde_json::json!({"wave": wave, "cell": cell.id, "name": cell.name, "result": "updated"})
            );
        }
    }
    Ok(())
}

/// Native CLI constructors use the same pinned directory client as the server.
pub fn attach_directory(pool: &DbPool, state: &Path) -> Result<()> {
    let address = std::env::var("DIRECTORY_ADDRESS")
        .ok()
        .map(|value| value.parse())
        .transpose()?;
    let key = std::env::var("DIRECTORY_PUBLIC_KEY").ok();
    let settings =
        crate::backend::node::router::DirectorySettings::from_options(address, key.as_deref())?;
    match settings {
        Some(settings) => pool.set_directory(std::sync::Arc::new(settings.client(state)?)),
        None => {
            anyhow::ensure!(
                pool.cell_id() == Prefix::ZERO,
                "A nonzero cell requires DIRECTORY_ADDRESS and DIRECTORY_PUBLIC_KEY"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rollout_waves_start_with_one_canary_and_cover_every_cell_once() {
        assert_eq!(super::waves(0), Vec::<usize>::new());
        assert_eq!(super::waves(1), vec![1]);
        assert_eq!(super::waves(2), vec![1, 1]);
        assert_eq!(super::waves(20), vec![1, 1, 18]);
        assert_eq!(super::waves(1000), vec![1, 9, 90, 900]);
    }
}
