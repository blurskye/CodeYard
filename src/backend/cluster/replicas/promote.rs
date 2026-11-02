//! Promotion. `promote` makes a copy the primary of its repository on an operator's
//! request, and `take_over` does it for a copy that holds every write when its primary has
//! been offline for a while. A copy that is behind is never promoted automatically: only
//! the operator knows whether losing those writes is acceptable.
//!
//! A fork is stored on the same node as its source, because pull requests between them need
//! both there. So a promotion moves the whole fork family that shares the repository's
//! primary, or nothing at all.
use crate::backend::db::{
    self,
    cluster::{FamilyOutcome, PromoteOutcome},
    models::ClusterNode,
    DbPool,
};
use anyhow::{bail, ensure, Context, Result};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};
use tracing::warn;

/// How long a node must have seen a primary offline before it takes over from it. A node
/// that restarts is back well within this, so a restart moves nothing.
pub const TAKE_OVER_AFTER: Duration = Duration::from_secs(60);

/// The primaries a node has seen offline, and since when. A node that restarts counts
/// again, so a cluster that restarts together moves nothing.
#[derive(Default)]
pub struct Silent {
    since: HashMap<i64, Instant>,
    /// Members of fork families that a take-over skipped, with their primary. Every pass checks
    /// a family again; this only keeps the operator from being told twice in one outage.
    skipped: HashMap<i64, i64>,
}

/// What a promotion did to one repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Promotion {
    pub repository: i64,
    pub from: i64,
    pub to: i64,
    /// Writes the new primary does not hold. Zero unless the promotion was forced.
    pub lost_writes: i64,
}

/// Makes a copy of `repository` the primary, along with the other members of its fork family
/// that have the same primary. Without `to`, the copy that holds the most writes is chosen.
/// Every member is checked before any moves: each needs a copy on `to` that has been made
/// and, unless `force` is set, holds every write. With `force`, a member that has no copy
/// on `to` stays where it is. Returns what moved, lowest id first.
pub async fn promote(
    pool: &DbPool,
    repository: i64,
    to: Option<i64>,
    force: bool,
) -> Result<Vec<Promotion>> {
    // The family is read once, so the repository's primary and its members agree.
    let family = db::cluster::list_fork_family(pool, repository).await?;
    let &(_, primary, _) = family
        .iter()
        .find(|(id, _, _)| *id == repository)
        .context("the repository does not exist")?;
    let from = primary.context("the repository has no primary node")?;
    let nodes = db::cluster::list_nodes(pool).await?;
    let mut members = members_on(&family, from);
    let to = match to {
        Some(node) => node,
        // Only a node that is reporting in can take the repository, so the default skips the
        // others. The transaction checks again, in case a node stops in between.
        None => {
            let mut candidates = db::cluster::list_replicas(pool, repository).await?;
            candidates.sort_by_key(|(id, applied)| (Reverse(*applied), *id));
            let mut selected = None;
            for (node, _) in candidates {
                if is_reporting_storage(&nodes, node)
                    && (force
                        || blocking_member(pool, &members, node, false)
                            .await?
                            .is_none())
                {
                    selected = Some(node);
                    break;
                }
            }
            selected.context(
                "no eligible copy of the fork family is on a storage node that is reporting in",
            )?
        }
    };
    ensure!(
        is_storage(&nodes, to),
        "that node is not an active storage node"
    );
    if force {
        // The operator accepts a split family: a member with no copy on `to` stays on the old
        // primary, to be promoted from its own copy. Without this a primary lost while its
        // family's copies sit on different nodes could not be promoted at all.
        members = members_with_a_copy(pool, members, to, repository).await?;
    }
    if let Some(problem) = blocking_member(pool, &members, to, force).await? {
        bail!("{problem}; nothing changed");
    }
    // The members move one by one, so a race can stop the sequence. The error then says
    // which repositories had already moved.
    let mut promoted = Vec::new();
    for (member, _) in members {
        let outcome = db::cluster::promote_repository(pool, member, from, to, !force).await?;
        let problem = match outcome {
            PromoteOutcome::Promoted {
                applied_version,
                write_version,
            } => {
                promoted.push(Promotion {
                    repository: member,
                    from,
                    to,
                    lost_writes: write_version - applied_version.max(0),
                });
                continue;
            }
            PromoteOutcome::Stale { .. } => "fell behind during the promotion",
            PromoteOutcome::NoReplica => "lost its copy during the promotion",
            PromoteOutcome::TargetOffline => "cannot move to a node that is not reporting in",
            PromoteOutcome::Moved => "changed primary during the promotion",
        };
        let name = repository_name(pool, member).await?;
        if promoted.is_empty() {
            bail!("{name} {problem}; nothing changed");
        }
        bail!(
            "{name} {problem}; {} repositories of its fork family had already moved to the new primary. Run promote for {name} again to move the rest",
            promoted.len()
        );
    }
    if !force
        && db::cluster::list_fork_family(pool, repository)
            .await?
            .iter()
            .any(|(_, primary, _)| *primary == Some(from))
    {
        bail!("the fork family changed during promotion; {} repositories had already moved. Run promote again to move the rest", promoted.len());
    }
    Ok(promoted)
}

/// Makes `node` the primary of every fork family whose members are all on a primary it has
/// seen offline for `after`, when it holds a current copy of each member. Returns the
/// repositories taken over. The family moves in one transaction (`promote_fork_family`), so
/// when several nodes hold such copies, the first to lock the family takes it and the others
/// find it moved. A family that cannot be taken whole is skipped, and the operator is told once
/// per outage. A primary that was revoked is the operator's doing and is left to
/// `cluster promote`.
pub async fn take_over(
    pool: &DbPool,
    node: i64,
    repos: &Path,
    silent: &mut Silent,
    after: Duration,
) -> Result<Vec<i64>> {
    super::reconcile(pool, node, repos).await?;
    let nodes = db::cluster::list_nodes(pool).await?;
    let now = Instant::now();
    let offline: Vec<i64> = nodes
        .iter()
        .filter(|candidate| candidate.status == "active" && !candidate.is_online())
        .map(|candidate| candidate.id)
        .collect();
    silent.since.retain(|id, _| offline.contains(id));
    for id in offline {
        silent.since.entry(id).or_insert(now);
    }
    let since = &silent.since;
    silent
        .skipped
        .retain(|_, primary| since.contains_key(primary));
    if !is_storage(&nodes, node) {
        return Ok(Vec::new());
    }
    let mut taken = Vec::new();
    // The families already decided in this pass, so each one is tried once, from its first row.
    let mut decided = HashSet::new();
    for row in db::cluster::list_held_replicas(pool, node).await? {
        let Some(from) = row.primary_node_id.filter(|from| *from != node) else {
            continue;
        };
        let gone = silent
            .since
            .get(&from)
            .is_some_and(|since| now.duration_since(*since) >= after);
        if !gone || decided.contains(&row.repository_id) {
            continue;
        }
        let family = db::cluster::list_fork_family(pool, row.repository_id).await?;
        let members = members_on(&family, from);
        decided.extend(members.iter().map(|(member, _)| *member));
        if members.is_empty() {
            // Another node has already taken the family off this primary.
            continue;
        }
        let problem = if members.len() < family.len() {
            Some("not every member of it is on the offline primary".to_owned())
        } else {
            blocking_member(pool, &members, node, false).await?
        };
        if let Some(problem) = problem {
            // A skipped family is checked again on every pass. `skipped` only keeps the warning
            // to once per outage.
            let reported = members
                .iter()
                .any(|(member, _)| silent.skipped.get(member) == Some(&from));
            for (member, _) in &members {
                silent.skipped.insert(*member, from);
            }
            if !reported {
                warn!(
                    repository = row.repository_id,
                    from,
                    %problem,
                    "primary offline; its fork family was not taken over"
                );
            }
            continue;
        }
        // A refusal means the family changed after it was read. Nothing moves, and the next
        // pass looks at it again.
        if let FamilyOutcome::Promoted(moved) =
            db::cluster::promote_fork_family(pool, row.repository_id, from, node).await?
        {
            for member in moved {
                silent.skipped.remove(&member);
                warn!(
                    repository = member,
                    from, "primary offline; took over the repository from a current copy"
                );
                taken.push(member);
            }
        }
    }
    Ok(taken)
}

/// `owner/name` of a repository, for messages.
pub async fn repository_name(pool: &DbPool, repository: i64) -> Result<String> {
    let row = db::repositories::records::get_repository_by_id(pool, repository)
        .await?
        .context("the repository does not exist")?;
    let owner = db::repositories::records::repository_owner_name(pool, repository)
        .await?
        .context("the repository has no owner")?;
    Ok(format!("{owner}/{}", row.name))
}

/// The members of a fork family, as `list_fork_family` lists them, whose primary is `primary`,
/// as (id, write version). These are the repositories that move together.
fn members_on(family: &[(i64, Option<i64>, i64)], primary: i64) -> Vec<(i64, i64)> {
    family
        .iter()
        .filter(|(_, node, _)| *node == Some(primary))
        .map(|&(id, _, write_version)| (id, write_version))
        .collect()
}

/// `members` without those that have no finished copy on `to`. `wanted` is always kept, so
/// that the check that follows names it when its own copy is missing.
async fn members_with_a_copy(
    pool: &DbPool,
    members: Vec<(i64, i64)>,
    to: i64,
    wanted: i64,
) -> Result<Vec<(i64, i64)>> {
    let mut kept = Vec::new();
    for member in members {
        let copied = db::cluster::list_replicas(pool, member.0)
            .await?
            .into_iter()
            .any(|(node, applied)| node == to && applied >= 0);
        if copied || member.0 == wanted {
            kept.push(member);
        }
    }
    Ok(kept)
}

/// The first member that cannot move to `to`, as a sentence that names it, or `None` when
/// every member can.
async fn blocking_member(
    pool: &DbPool,
    members: &[(i64, i64)],
    to: i64,
    force: bool,
) -> Result<Option<String>> {
    for &(member, write_version) in members {
        let applied = db::cluster::list_replicas(pool, member)
            .await?
            .into_iter()
            .find(|(node, _)| *node == to)
            .map(|(_, applied)| applied);
        if let Some(problem) = copy_problem(applied, write_version, force) {
            let name = repository_name(pool, member).await?;
            return Ok(Some(format!("{name} {problem}")));
        }
    }
    Ok(None)
}

/// Why a copy on the target cannot take over a repository, as words that follow its name,
/// or `None` when it can. `applied` is the copy's version, or `None` when the node has no
/// copy. A copy that was never made cannot be used, even when forced.
fn copy_problem(applied: Option<i64>, write_version: i64, force: bool) -> Option<String> {
    match applied {
        None => Some("has no copy on that node".to_owned()),
        Some(applied) if applied < 0 => Some("has not been copied to that node yet".to_owned()),
        Some(applied) if applied != write_version && !force => Some(format!(
            "holds {applied} of {write_version} writes on that node; promoting it loses {} of them, and `cluster promote --force` accepts that",
            write_version - applied
        )),
        Some(_) => None,
    }
}

fn is_storage(nodes: &[ClusterNode], id: i64) -> bool {
    nodes.iter().any(|node| {
        node.id == id && node.status == "active" && node.roles.iter().any(|role| role == "storage")
    })
}

/// Whether a promotion may move a repository to `id`: a storage node that is reporting in, by
/// the database clock. The promotion transaction checks the same thing again.
fn is_reporting_storage(nodes: &[ClusterNode], id: i64) -> bool {
    is_storage(nodes, id) && nodes.iter().any(|node| node.id == id && node.is_online())
}
