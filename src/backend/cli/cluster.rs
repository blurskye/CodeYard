use crate::backend::cluster::{hostname, NEEDS_POSTGRES};
use crate::backend::{
    cluster::{
        adopt_repositories, approve, create_invite, join, load_existing, load_or_create,
        parse_duration,
        replicas::{self, Promotion},
        require_postgres, revoke, InviteOptions, JoinTicket, Membership, NodeAddress, NodeKey,
        Relay,
    },
    db::{
        self,
        cluster::Authority,
        models::{NodeStatusChange, ReplicaSetting},
        DbPool,
    },
    node::NodeRoles,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use std::{path::Path, time::Duration};
const STALE_AFTER_SECONDS: i64 = 60;

/// The address this node last reported while serving, for putting in a ticket.
pub async fn own_address(pool: &DbPool, key: &NodeKey) -> Result<NodeAddress> {
    let not_serving = || {
        anyhow!("this node is not serving in cluster mode yet; start `codeyard serve` with cluster settings (for example IROH_BIND=0.0.0.0:7444) and try again")
    };
    let node = db::cluster::get_node_by_public_key(pool, &key.public_hex())
        .await?
        .ok_or_else(not_serving)?;
    let hint = node.addr_hint.ok_or_else(not_serving)?;
    match node.last_seen {
        Some(seen) if (Utc::now() - seen).num_seconds() <= STALE_AFTER_SECONDS => {}
        Some(seen) => eprintln!(
            "warning: this node last reported {}; make sure `codeyard serve` is running or the ticket will not work",
            ago(seen)
        ),
        None => eprintln!(
            "warning: this node is not reporting; make sure `codeyard serve` is running or the ticket will not work"
        ),
    }
    serde_json::from_str(&hint).context("the stored node address is unreadable")
}

/// `codeyard cluster ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Print a new secret key and its public key
    Keygen,
    /// Print a ticket that lets another machine join this cluster
    Invite {
        /// Roles the new node may run, e.g. storage or web,ssh
        #[arg(long)]
        roles: NodeRoles,
        /// Let any number of nodes join until the ticket expires (for autoscaling)
        #[arg(long)]
        reusable: bool,
        /// How long the ticket works: 30m, 1h, 7d, 2w (at most 365d)
        #[arg(long, default_value = "1h", value_parser = parse_duration)]
        expires: Duration,
        /// New nodes wait as pending until `cluster approve`
        #[arg(long)]
        approve: bool,
    },
    /// List the invites that can still admit a node
    Invites,
    /// Cancel an invite from `cluster invites`; nodes that joined with it stay
    CancelInvite { id: i64 },
    /// Join using a ticket supplied through CLUSTER_JOIN_TOKEN
    Join {
        /// Populated from the environment after command-line parsing
        #[arg(skip)]
        ticket: String,
        /// Name for this node (defaults to the host name)
        #[arg(long, env = "NODE_NAME")]
        name: Option<String>,
    },
    /// Show the nodes of this cluster
    List,
    /// Let a pending (or revoked) node connect
    Approve { name: String },
    /// Stop a node from connecting
    Revoke { name: String },
    /// Take over the repositories that have no storage node yet. Run this once, on the
    /// node that holds their directories, when a single machine joins a cluster.
    Adopt,
    /// Show how many copies each repository keeps, and each node's copies
    Replicas {
        #[command(subcommand)]
        command: Option<ReplicasCommand>,
    },
    /// Make a copy the primary of a repository. Use it when the primary is gone, or to
    /// move a repository on purpose
    Promote {
        /// The repository as owner/name (leave it out with --all-from)
        repository: Option<String>,
        /// The node to promote (default: the copy that holds the most writes)
        #[arg(long, conflicts_with = "all_from")]
        to: Option<String>,
        /// Promote every repository whose primary is this node
        #[arg(long, value_name = "NODE", conflicts_with = "repository")]
        all_from: Option<String>,
        /// Accept losing the writes that the new primary does not hold, and leave behind
        /// fork family members that have no copy there
        #[arg(long)]
        force: bool,
    },
}

#[derive(clap::Subcommand)]
pub enum ReplicasCommand {
    /// Keep 1 copy (no replicas), a count from 2 to 999 in all, or a copy on every storage node
    Set { value: String },
}

impl Command {
    /// `keygen` and `join` work on a machine that has no database at all.
    pub fn needs_database(&self) -> bool {
        !matches!(self, Command::Keygen | Command::Join { .. })
    }
}

/// Runs `keygen` and `join`. `database_url` is only used to refuse joining from an
/// installation that uses SQLite.
pub async fn run_offline(
    command: &Command,
    state_dir: &Path,
    database_url: Option<&str>,
) -> Result<()> {
    match command {
        Command::Keygen => {
            let key = NodeKey::generate();
            println!("NODE_KEY={}", key.secret_hex());
            println!("NODE_PUBLIC_KEY={}", key.public_hex());
            eprintln!("Keep NODE_KEY secret: give it to the node as NODE_KEY or NODE_KEY_FILE.");
            eprintln!("List NODE_PUBLIC_KEY in CLUSTER_NODES on the core nodes.");
            Ok(())
        }
        Command::Join { ticket, name } => {
            let sqlite = match database_url {
                Some(url) => url.starts_with("sqlite:"),
                None => state_dir.join("gitserv.db").exists(),
            };
            ensure!(!sqlite, NEEDS_POSTGRES);
            ensure!(
                !ticket.trim().is_empty(),
                "set CLUSTER_JOIN_TOKEN in the environment or .env, then run `codeyard cluster join`"
            );
            let ticket = JoinTicket::decode(ticket)?;
            let relay = Relay::parse(std::env::var("IROH_RELAY").ok().as_deref())?;
            let name = match name {
                Some(name) => name.clone(),
                None => hostname()?,
            };
            let key = load_or_create(state_dir)?;
            let membership = join(&ticket, &key, &relay, &name, None).await?;
            crate::backend::cluster::worker::save_core(state_dir, &ticket)?;
            print_membership(&membership, &key);
            Ok(())
        }
        _ => bail!("this command needs the database"),
    }
}

fn print_membership(membership: &Membership, key: &NodeKey) {
    let action = if membership.newly_joined {
        "Joined the cluster"
    } else {
        "Already part of the cluster"
    };
    println!(
        "{action} as '{}' (roles: {}, status: {}).",
        membership.name, membership.roles, membership.status
    );
    println!("Public key: {}", key.public_hex());
    if membership.status == "pending" {
        println!(
            "Waiting for approval: on a core node run `codeyard cluster approve {}`.",
            membership.name
        );
    }
}

/// Runs the commands that manage membership in the database.
pub async fn run(
    command: &Command,
    pool: &DbPool,
    state_dir: &Path,
    repos_path: &Path,
) -> Result<()> {
    if !command.needs_database() {
        return run_offline(command, state_dir, None).await;
    }
    require_postgres(pool)?;
    match command {
        Command::Invite {
            roles,
            reusable,
            expires,
            approve,
        } => {
            let key = load_existing(state_dir)?.ok_or_else(|| {
                anyhow!(
                    "this node has no key yet; start `codeyard serve` with cluster settings first"
                )
            })?;
            let address = own_address(pool, &key).await?;
            let options = InviteOptions {
                roles: roles.clone(),
                reusable: *reusable,
                requires_approval: *approve,
                expires_in: *expires,
            };
            let ticket = create_invite(pool, key.public_key(), address, &options).await?;
            let valid_until = Utc::now() + chrono::Duration::from_std(*expires)?;
            eprintln!(
                "Invite for roles {} ({}, valid until {}{}).",
                options.roles,
                if *reusable { "reusable" } else { "one-time" },
                valid_until.format("%Y-%m-%d %H:%M UTC"),
                if *approve {
                    ", new nodes need approval"
                } else {
                    ""
                },
            );
            eprintln!("On the new machine, load the ticket into CLUSTER_JOIN_TOKEN and run `codeyard cluster join`.");
            println!("{}", ticket.encode());
            Ok(())
        }
        Command::List => list(pool).await,
        Command::Invites => invites(pool).await,
        Command::CancelInvite { id } => {
            ensure!(
                db::cluster::delete_join_token(pool, *id).await?,
                "no invite with id {id}"
            );
            println!("Invite {id} cancelled; nodes that joined with it stay.");
            Ok(())
        }
        // The operator needs no account, so only an unknown name refuses.
        Command::Approve { name } => match approve(pool, Authority::Operator, name).await? {
            NodeStatusChange::Changed { before, node } => {
                println!("'{}' was {before}; it is now {}.", node.name, node.status);
                Ok(())
            }
            _ => bail!("no node is named '{name}'; `codeyard cluster list` shows the names"),
        },
        Command::Revoke { name } => match revoke(pool, Authority::Operator, name).await? {
            NodeStatusChange::Changed { before, node } => {
                println!(
                    "'{}' was {before}; it is now {} and can no longer connect.",
                    node.name, node.status
                );
                Ok(())
            }
            _ => bail!("no node is named '{name}'; `codeyard cluster list` shows the names"),
        },
        Command::Adopt => {
            let key = load_existing(state_dir)?.ok_or_else(|| {
                anyhow!(
                    "this node has no key yet; start `codeyard serve` with cluster settings first"
                )
            })?;
            let node = db::cluster::get_node_by_public_key(pool, &key.public_hex())
                .await?
                .ok_or_else(|| {
                    anyhow!("this node is not registered; start `codeyard serve` with cluster settings first")
                })?;
            ensure!(
                node.status == "active" && node.roles.iter().any(|role| role == "storage"),
                "only an active node with the storage role can take over repositories"
            );
            let adopted = adopt_repositories(pool, node.id, repos_path).await?;
            println!("{adopted} repositories now live on '{}'.", node.name);
            Ok(())
        }
        Command::Replicas { command } => match command {
            None => replicas_status(pool).await,
            Some(ReplicasCommand::Set { value }) => set_replicas(pool, value).await,
        },
        Command::Promote {
            repository,
            to,
            all_from,
            force,
        } => {
            promote_command(
                pool,
                repository.as_deref(),
                to.as_deref(),
                all_from.as_deref(),
                *force,
            )
            .await
        }
        Command::Keygen | Command::Join { .. } => unreachable!("handled before the database"),
    }
}

/// What the replica setting means for a repository.
fn describe(setting: ReplicaSetting) -> String {
    match setting {
        ReplicaSetting::Copies(1) => "one copy, on its primary".to_owned(),
        ReplicaSetting::Copies(count) => {
            format!("{count} copies in all, the primary included")
        }
        ReplicaSetting::All => "a copy on every storage node".to_owned(),
    }
}

async fn replicas_status(pool: &DbPool) -> Result<()> {
    let setting: ReplicaSetting = db::cluster::get_replicas(pool).await?.parse()?;
    println!("Each repository keeps {}.", describe(setting));
    let nodes = db::cluster::list_nodes(pool).await?;
    let counts = db::cluster::replica_counts(pool).await?;
    println!(
        "{:<20} {:<8} {:<10} {:<8} {:<8} Behind",
        "Node", "Status", "Primaries", "Copies", "Current"
    );
    for node in nodes {
        let (primaries, copies, current) = counts
            .iter()
            .find(|count| count.node_id == node.id)
            .map_or((0, 0, 0), |count| {
                (count.primaries, count.replicas, count.current)
            });
        println!(
            "{:<20} {:<8} {:<10} {:<8} {:<8} {}",
            node.name,
            node.status,
            primaries,
            copies,
            current,
            copies - current,
        );
    }
    Ok(())
}

async fn set_replicas(pool: &DbPool, value: &str) -> Result<()> {
    let setting: ReplicaSetting = value.parse()?;
    ensure!(
        db::cluster::set_replicas(pool, &setting.to_string()).await?,
        "this installation has no configuration yet; finish its setup first"
    );
    println!(
        "Each repository keeps {}. Copies follow within {} seconds.",
        describe(setting),
        replicas::INTERVAL.as_secs()
    );
    Ok(())
}

async fn node_id(pool: &DbPool, name: &str) -> Result<i64> {
    db::cluster::get_node_by_name(pool, name)
        .await?
        .map(|node| node.id)
        .ok_or_else(|| {
            anyhow!("no node is named '{name}'; `codeyard cluster list` shows the names")
        })
}

async fn node_name(pool: &DbPool, id: i64) -> Result<String> {
    Ok(db::cluster::get_node_by_id(pool, id)
        .await?
        .map_or_else(|| format!("node {id}"), |node| node.name))
}

/// Whether `node` is the primary of `repository` now.
async fn is_primary(pool: &DbPool, repository: i64, node: i64) -> Result<bool> {
    Ok(
        db::repositories::records::get_repository_by_id(pool, repository)
            .await?
            .is_some_and(|row| row.storage_node_id == Some(node)),
    )
}

async fn report_promotion(pool: &DbPool, promotion: Promotion) -> Result<()> {
    let repository = replicas::repository_name(pool, promotion.repository).await?;
    let from = node_name(pool, promotion.from).await?;
    let to = node_name(pool, promotion.to).await?;
    println!(
        "{repository}: '{to}' is the primary now. '{from}' keeps a copy and catches up within {} seconds.",
        replicas::INTERVAL.as_secs()
    );
    if promotion.lost_writes > 0 {
        eprintln!(
            "warning: {} writes that '{from}' had are not on '{to}'; they are lost.",
            promotion.lost_writes
        );
    }
    Ok(())
}

async fn promote_command(
    pool: &DbPool,
    repository: Option<&str>,
    to: Option<&str>,
    all_from: Option<&str>,
    force: bool,
) -> Result<()> {
    match (repository, all_from) {
        (Some(name), None) => {
            let (owner, repo) = name
                .split_once('/')
                .context("name the repository as owner/name")?;
            let row = db::repositories::records::get_repository(pool, owner, repo)
                .await?
                .ok_or_else(|| anyhow!("no repository is named {name}"))?;
            let to = match to {
                Some(node) => Some(node_id(pool, node).await?),
                None => None,
            };
            for promotion in replicas::promote(pool, row.id, to, force).await? {
                report_promotion(pool, promotion).await?;
            }
            Ok(())
        }
        (None, Some(node)) => {
            let primary = node_id(pool, node).await?;
            let mut repositories: Vec<i64> = db::cluster::list_primary_repositories(pool, primary)
                .await?
                .into_iter()
                .map(|(id, _, _, _, _)| id)
                .collect();
            repositories.dedup();
            if repositories.is_empty() {
                println!("No repository has '{node}' as its primary.");
                return Ok(());
            }
            // A promotion moves the fork family along, so a repository that is no longer on
            // this node has already moved and is not promoted again.
            let mut failures = Vec::new();
            for repository in repositories {
                if !is_primary(pool, repository, primary).await? {
                    continue;
                }
                match replicas::promote(pool, repository, None, force).await {
                    Ok(promotions) => {
                        for promotion in promotions {
                            report_promotion(pool, promotion).await?;
                        }
                    }
                    Err(error) => failures.push((repository, error)),
                }
            }
            // A repository that a later family promotion moved is not counted as failed.
            let mut failed = 0;
            for (repository, error) in failures {
                if is_primary(pool, repository, primary).await? {
                    failed += 1;
                    let name = replicas::repository_name(pool, repository).await?;
                    eprintln!("{name}: {error:#}");
                }
            }
            ensure!(
                failed == 0,
                "{failed} repositories were not promoted; the reasons are above"
            );
            Ok(())
        }
        _ => bail!("name a repository, or use --all-from NODE for a node that is down"),
    }
}

async fn list(pool: &DbPool) -> Result<()> {
    let nodes = db::cluster::list_nodes(pool).await?;
    if nodes.is_empty() {
        println!("No nodes yet. `codeyard serve` with cluster settings adds this one.");
        return Ok(());
    }
    println!(
        "{:<20} {:<8} {:<28} {:<10} {:<10} Public key",
        "Name", "Status", "Roles", "Last seen", "Version"
    );
    for node in nodes {
        println!(
            "{:<20} {:<8} {:<28} {:<10} {:<10} {}",
            node.name,
            node.status,
            node.roles.join(","),
            node.last_seen.map_or("never".to_owned(), ago),
            node.version.as_deref().unwrap_or("-"),
            node.public_key,
        );
    }
    Ok(())
}

async fn invites(pool: &DbPool) -> Result<()> {
    let invites = db::cluster::list_join_tokens(pool).await?;
    if invites.is_empty() {
        println!("No open invites. `codeyard cluster invite` creates one.");
        return Ok(());
    }
    println!(
        "{:<6} {:<28} {:<9} {:<9} {:<5} Valid until",
        "Id", "Roles", "Kind", "Approval", "Uses"
    );
    for invite in invites {
        println!(
            "{:<6} {:<28} {:<9} {:<9} {:<5} {}",
            invite.id,
            invite.roles.join(","),
            if invite.reusable {
                "reusable"
            } else {
                "one-time"
            },
            if invite.requires_approval {
                "required"
            } else {
                "-"
            },
            invite.uses,
            invite.expires_at.format("%Y-%m-%d %H:%M UTC"),
        );
    }
    Ok(())
}

fn ago(seen: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - seen).num_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_times_read_naturally() {
        let now = Utc::now();
        assert_eq!(ago(now), "0s ago");
        assert_eq!(ago(now - chrono::Duration::seconds(125)), "2m ago");
        assert_eq!(ago(now - chrono::Duration::hours(5)), "5h ago");
        assert_eq!(ago(now - chrono::Duration::days(3)), "3d ago");
        assert_eq!(ago(now + chrono::Duration::seconds(30)), "0s ago");
    }
}
