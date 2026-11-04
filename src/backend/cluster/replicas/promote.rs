//! Promotion. Quorum repositories change terms and reconstruct canonical refs before
//! serving again. Silence only starts that recovery attempt. Legacy asynchronous copies
//! retain their version-based promotion and explicit forced-loss behavior.
//!
//! A fork is stored on the same node as its source, because pull requests between them need
//! both there. So a promotion moves the whole fork family that shares the repository's
//! primary, or nothing at all.

use crate::backend::db::cluster::FamilyOutcome;
use crate::backend::db::cluster::PromoteOutcome;
use anyhow::{bail, ensure, Context, Result};
use codeyard_db_core::models::ClusterNode;
use codeyard_db_core::DbPool;
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
    if crate::backend::db::cluster::writes::write_settings(pool)
        .await?
        .write_quorum
        .is_some()
    {
        ensure!(!force, "--force is not allowed with write quorum");
        return tokio::time::timeout(
            Duration::from_secs(10),
            promote_quorum(pool, repository, to),
        )
        .await?;
    }
    // The family is read once, so the repository's primary and its members agree.
    let family = crate::backend::db::cluster::list_fork_family(pool, repository).await?;
    let &(_, primary, _) = family
        .iter()
        .find(|(id, _, _)| *id == repository)
        .context("the repository does not exist")?;
    let from = primary.context("the repository has no primary node")?;
    let nodes = crate::backend::db::cluster::list_nodes(pool).await?;
    let mut members = members_on(&family, from);
    let to = match to {
        Some(node) => node,
        // Only a node that is reporting in can take the repository, so the default skips the
        // others. The transaction checks again, in case a node stops in between.
        None => {
            let mut candidates =
                crate::backend::db::cluster::list_replicas(pool, repository).await?;
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
    if !force {
        return match crate::backend::db::cluster::promote_manual_family(pool, repository, from, to)
            .await?
        {
            FamilyOutcome::Promoted(members) => Ok(members
                .into_iter()
                .map(|repository| Promotion {
                    repository,
                    from,
                    to,
                    lost_writes: 0,
                })
                .collect()),
            FamilyOutcome::Refused => {
                bail!("the fork family changed during promotion; nothing changed")
            }
        };
    }
    // Forced promotion explicitly accepts a split family and reports partial progress.
    let mut promoted = Vec::new();
    for (member, _) in members {
        let outcome =
            crate::backend::db::cluster::promote_repository(pool, member, from, to, !force).await?;
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
    Ok(promoted)
}

#[derive(Debug, thiserror::Error)]
#[error("repository {repository} is fenced on node {target}; canonical recovery is pending")]
pub struct RecoveryPending {
    pub repository: i64,
    pub target: i64,
}

async fn promote_quorum(
    pool: &DbPool,
    repository: i64,
    target: Option<i64>,
) -> Result<Vec<Promotion>> {
    use crate::backend::db::cluster::writes as journal;
    let family = journal::write_family(pool, repository).await?;
    let first = journal::recovery_view(pool, repository).await?;
    let from = first.primary_id;
    let mut voters: Vec<_> = first.voters.iter().map(|voter| voter.node_id).collect();
    voters.sort_unstable();
    ensure!(
        voters.len() == 2,
        "promotion requires the same two fixed voters"
    );
    let mut nodes = Vec::with_capacity(2);
    for voter in &voters {
        nodes.push(
            crate::backend::db::cluster::get_node_by_id(pool, *voter)
                .await?
                .context("voting node is missing")?,
        );
    }
    let to = match target {
        Some(target) => target,
        None if !first.serving => from,
        None => first
            .voters
            .iter()
            .find(|voter| voter.node_id != from && is_reporting_storage(&nodes, voter.node_id))
            .map(|voter| voter.node_id)
            .context("no voting copy is reporting in")?,
    };
    ensure!(
        is_reporting_storage(&nodes, to),
        "target storage node is not reporting in"
    );
    ensure!(voters.contains(&to), "target is not a fixed voter");
    let mut terms = Vec::with_capacity(family.len());
    for (member, primary, _) in &family {
        ensure!(
            *primary == Some(from),
            "fork family has different primaries"
        );
        let view = journal::recovery_view(pool, *member).await?;
        let mut membership: Vec<_> = view.voters.iter().map(|voter| voter.node_id).collect();
        membership.sort_unstable();
        ensure!(
            view.primary_id == from && view.serving == first.serving && membership == voters,
            "fork family terms changed"
        );
        if first.serving && from != to {
            ensure!(
                view.voters.iter().any(|voter| voter.node_id == to
                    && voter.term == view.term
                    && voter.applied_version == view.committed_version),
                "voting copy has not applied canonical refs"
            );
        }
        terms.push((*member, view.term));
    }
    if first.serving {
        if from == to {
            return Ok(Vec::new());
        }
        ensure!(
            journal::switch_write_family(pool, repository, from, &terms, to, &voters).await?,
            "fork family changed during promotion; nothing changed"
        );
        for (_, term) in &mut terms {
            *term = term.checked_add(1).context("write term overflow")?;
        }
    } else {
        ensure!(
            from == to,
            "resume canonical recovery on the fenced primary before moving it again"
        );
    }
    if !journal::enable_write_family(pool, &terms).await? {
        return Err(RecoveryPending {
            repository,
            target: to,
        }
        .into());
    }
    Ok(terms
        .into_iter()
        .map(|(repository, _)| Promotion {
            repository,
            from,
            to,
            lost_writes: 0,
        })
        .collect())
}

/// Each voter reconstructs only its own directories. Enabling still requires both
/// voters' durable applied versions and no unfinished operation in the whole family.
pub async fn recover_family(
    pool: &DbPool,
    repository: i64,
    node: i64,
    repos: &Path,
) -> Result<bool> {
    use crate::backend::db::cluster::writes as journal;
    let family = journal::write_family(pool, repository).await?;
    let mut terms = Vec::with_capacity(family.len());
    for (member, _, _) in family {
        let view = journal::recovery_view(pool, member).await?;
        if !view.serving && view.voters.iter().any(|voter| voter.node_id == node) {
            crate::backend::repositories::store::RepoStore::recover_term(pool, member, node, repos)
                .await?;
        }
        terms.push((member, view.term));
    }
    journal::enable_write_family(pool, &terms).await
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
    let quorum = crate::backend::db::cluster::writes::write_settings(pool)
        .await?
        .write_quorum
        .is_some();
    if !quorum {
        crate::backend::cluster::replicas::reconcile(pool, node, repos).await?;
    }
    let nodes = crate::backend::db::cluster::list_nodes(pool).await?;
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
    let held = crate::backend::db::cluster::list_held_replicas(pool, node).await?;
    let versions: HashMap<_, _> = held
        .iter()
        .map(|row| (row.repository_id, row.applied_version))
        .collect();
    for row in held {
        if quorum {
            let view =
                crate::backend::db::cluster::writes::recovery_view(pool, row.repository_id).await?;
            if !view.serving {
                if let Err(error) = recover_family(pool, row.repository_id, node, repos).await {
                    warn!(repository = row.repository_id, %error, "canonical recovery remains fenced");
                }
                continue;
            }
        }
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
        let family = if quorum {
            crate::backend::db::cluster::writes::write_family(pool, row.repository_id).await?
        } else {
            crate::backend::db::cluster::list_fork_family(pool, row.repository_id).await?
        };
        let members = members_on(&family, from);
        decided.extend(members.iter().map(|(member, _)| *member));
        if members.is_empty() {
            // Another node has already taken the family off this primary.
            continue;
        }
        if quorum {
            match promote_quorum(pool, row.repository_id, Some(node)).await {
                Ok(promotions) => {
                    taken.extend(promotions.into_iter().map(|promotion| promotion.repository))
                }
                Err(error) if error.is::<RecoveryPending>() => {
                    if recover_family(pool, row.repository_id, node, repos).await? {
                        taken.extend(members.iter().map(|(member, _)| *member));
                    }
                }
                Err(error) => {
                    if silent.skipped.insert(row.repository_id, from) != Some(from) {
                        warn!(repository = row.repository_id, %error, "primary offline; canonical recovery was refused");
                    }
                }
            }
            continue;
        }
        let problem = if members.len() < family.len() {
            Some("not every member of it is on the offline primary".to_owned())
        } else {
            let blocked = members.iter().find_map(|(member, version)| {
                copy_problem(versions.get(member).copied(), *version, false)
                    .map(|problem| (*member, problem))
            });
            match blocked {
                Some((member, problem)) => Some(format!(
                    "{} {problem}",
                    repository_name(pool, member).await?
                )),
                None => None,
            }
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
            crate::backend::db::cluster::promote_fork_family(pool, row.repository_id, from, node)
                .await?
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
    let row = crate::backend::db::repositories::records::get_repository_by_id(pool, repository)
        .await?
        .context("the repository does not exist")?;
    let owner = crate::backend::db::repositories::records::repository_owner_name(pool, repository)
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
        let copied = crate::backend::db::cluster::list_replicas(pool, member.0)
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
        let applied = crate::backend::db::cluster::list_replicas(pool, member)
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
    nodes
        .iter()
        .any(|node| node.id == id && node.is_serving_storage())
}
