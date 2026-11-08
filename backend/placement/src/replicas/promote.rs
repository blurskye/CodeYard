//! Promotion. Quorum repositories change terms and reconstruct canonical refs before
//! serving again. Silence only starts that recovery attempt. Legacy asynchronous copies
//! retain their version-based promotion and explicit forced-loss behavior.
//!
//! A fork is stored on the same node as its source, because pull requests between them need
//! both there. So a promotion moves the whole fork family that shares the repository's
//! primary, or nothing at all.
use anyhow::Context;

use anyhow::bail;

use anyhow::ensure;

use anyhow::Result;
use codeyard_database::models::ClusterNode;
use codeyard_database::DbPool;
use codeyard_db_placement::cluster::FamilyOutcome;
use codeyard_db_placement::cluster::PromoteOutcome;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;
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
    if codeyard_db_placement::cluster::writes::write_settings(pool)
        .await?
        .write_quorum
        .is_some()
    {
        ensure!(!force, "--force is not allowed with write quorum");
        return tokio::time::timeout(
            Duration::from_secs(10),
            promote_quorum(pool, repository, to, None),
        )
        .await?;
    }
    // The family is read once, so the repository's primary and its members agree.
    let family = codeyard_db_placement::cluster::list_fork_family(pool, repository).await?;
    let &(_, primary, _) = family
        .iter()
        .find(|(id, _, _)| *id == repository)
        .context("the repository does not exist")?;
    let from = primary.context("the repository has no primary node")?;
    let nodes = codeyard_db_placement::cluster::list_nodes(pool).await?;
    let mut members = members_on(&family, from);
    let to = match to {
        Some(node) => node,
        // Only a node that is reporting in can take the repository, so the default skips the
        // others. The transaction checks again, in case a node stops in between.
        None => {
            let mut candidates =
                codeyard_db_placement::cluster::list_replicas(pool, repository).await?;
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
        return match codeyard_db_placement::cluster::promote_manual_family(
            pool, repository, from, to,
        )
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
            codeyard_db_placement::cluster::promote_repository(pool, member, from, to, !force)
                .await?;
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
    replacement: Option<&[i64]>,
) -> Result<Vec<Promotion>> {
    use codeyard_db_placement::cluster::writes as journal;
    let family = journal::write_family(pool, repository).await?;
    let first = journal::recovery_view(pool, repository).await?;
    let from = first.primary_id;
    let mut voters: Vec<_> = first.voters.iter().map(|voter| voter.node_id).collect();
    voters.sort_unstable();
    ensure!(
        (2..=999).contains(&voters.len()),
        "promotion requires fixed voters"
    );
    let original = voters.clone();
    if let Some(replacement) = replacement {
        ensure!(
            replacement.len() == voters.len(),
            "replacement must preserve voter count"
        );
        voters = replacement.to_vec();
        voters.sort_unstable();
        voters.dedup();
        ensure!(
            voters.len() == original.len(),
            "duplicate replacement voter"
        );
    }
    let mut nodes = Vec::with_capacity(voters.len());
    for voter in &voters {
        let node = codeyard_db_placement::cluster::get_node_by_id(pool, *voter)
            .await?
            .context("voting node is missing")?;
        ensure!(node.state == "active", "replacement voter is draining");
        nodes.push(node);
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
            view.primary_id == from && view.serving == first.serving && membership == original,
            "fork family terms changed"
        );
        if first.serving {
            for voter in &voters {
                if *voter == from {
                    continue;
                }
                let current = view.voters.iter().any(|copy| {
                    copy.node_id == *voter
                        && copy.term == view.term
                        && copy.applied_version == view.committed_version
                });
                ensure!(
                    current
                        || (original.contains(voter) && *voter != to)
                        || codeyard_db_placement::cluster::replica_is_current(
                            pool, *member, *voter
                        )
                        .await?,
                    "replacement copy has not applied canonical refs"
                );
            }
        }
        terms.push((*member, view.term));
    }
    if first.serving {
        if from == to && voters == original {
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

/// Copy-then-promote and drain share the same fenced family switch.
pub async fn promote_to_voters(
    pool: &DbPool,
    repository: i64,
    target: i64,
    voters: &[i64],
) -> Result<Vec<Promotion>> {
    tokio::time::timeout(
        Duration::from_secs(10),
        promote_quorum(pool, repository, Some(target), Some(voters)),
    )
    .await?
}

/// Refill a missed voter from a current copy before enabling it or using it for reads.
/// ReplicaPack is also the backup transport; no new publication protocol is involved.
pub async fn catch_up_voter(
    context: &codeyard_git_storage::context::NodeContext,
    repository: i64,
    stop: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    use codeyard_db_placement::cluster::writes as journal;
    use codeyard_git_storage::repositories::store::RepoStore;
    let pool = context.database()?;
    let view = journal::recovery_view(pool, repository).await?;
    if !view
        .voters
        .iter()
        .any(|voter| voter.node_id == context.node_id)
        || view.voters.iter().any(|voter| {
            voter.node_id == context.node_id
                && voter.term == view.term
                && voter.applied_version == view.committed_version
        })
        || view
            .unfinished
            .as_ref()
            .is_some_and(|write| write.decision == "pending")
    {
        return Ok(());
    }
    let row = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository)
        .await?
        .context("repository disappeared during voter catch-up")?;
    let uuid = codeyard_git_storage::wire::repository_uuid(&row.disk_path)?;
    let path = codeyard_git_storage::context::term_path(&context.repos_path, uuid, view.term);
    let directory = path.clone();
    codeyard_git::repository::process::blocking_until(
        Instant::now() + Duration::from_secs(5),
        move || Ok(codeyard_security::fs::durable_dir(&directory)?),
    )
    .await?;
    codeyard_git_storage::context::literal_directory(&context.repos_path, &path).await?;
    let guards = tokio::time::timeout(Duration::from_millis(500), async {
        let publication = codeyard_git::locks::publication(&path).await?;
        let readers = codeyard_git::locks::exclusive(&path).await;
        Ok::<_, anyhow::Error>((publication, readers))
    })
    .await??;
    guards
        .0
        .scope(async {
            let latest = journal::recovery_view(pool, repository).await?;
            ensure!(
                latest.term == view.term
                    && latest.committed_version == view.committed_version
                    && latest
                        .unfinished
                        .as_ref()
                        .is_none_or(|write| write.decision != "pending"),
                "canonical refs changed before voter catch-up"
            );
            let nodes = codeyard_db_placement::cluster::list_nodes(pool).await?;
            for source in view.voters.iter().filter(|voter| {
                voter.node_id != context.node_id
                    && voter.term == view.term
                    && voter.applied_version == view.committed_version
                    && is_reporting_storage(&nodes, voter.node_id)
            }) {
                ensure!(!stop.is_cancelled(), "voter copy cancelled");
                if super::transfer::snapshot(context, source.node_id, uuid, &path, &[], stop)
                    .await
                    .is_err()
                {
                    continue;
                }
                let store = RepoStore::from_disk_path(path.to_str().context("invalid term path")?);
                store.restore_refs(&view.refs).await?;
                let current = journal::recovery_view(pool, repository).await?;
                ensure!(
                    current.term == view.term
                        && current.committed_version == view.committed_version
                        && current
                            .unfinished
                            .as_ref()
                            .is_none_or(|write| write.decision != "pending"),
                    "canonical refs changed during voter catch-up"
                );
                ensure!(
                    journal::mark_write_applied(
                        pool,
                        repository,
                        context.node_id,
                        view.term,
                        view.committed_version
                    )
                    .await?,
                    "voter term changed"
                );
                if let Some(write) = &view.unfinished {
                    journal::finish_write(pool, &write.id, view.term).await?;
                }
                return Ok(());
            }
            anyhow::bail!("no current voter could supply canonical objects")
        })
        .await
}

/// Each voter reconstructs only its own directories. Enabling requires a quorum
/// of durable applied versions and no unfinished operation in the whole family.
pub async fn recover_family(
    pool: &DbPool,
    repository: i64,
    node: i64,
    repos: &Path,
) -> Result<bool> {
    use codeyard_db_placement::cluster::writes as journal;
    let family = journal::write_family(pool, repository).await?;
    let mut terms = Vec::with_capacity(family.len());
    for (member, _, _) in family {
        let view = journal::recovery_view(pool, member).await?;
        if !view.serving && view.voters.iter().any(|voter| voter.node_id == node) {
            codeyard_git_storage::repositories::store::RepoStore::recover_term(
                pool, member, node, repos,
            )
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
    let quorum = codeyard_db_placement::cluster::writes::write_settings(pool)
        .await?
        .write_quorum
        .is_some();
    if !quorum {
        crate::replicas::reconcile(pool, node, repos).await?;
    }
    let nodes = codeyard_db_placement::cluster::list_nodes(pool).await?;
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
    let held = if quorum {
        codeyard_db_placement::cluster::writes::held_voters(pool, node).await?
    } else {
        codeyard_db_placement::cluster::list_held_replicas(pool, node).await?
    };
    let versions: HashMap<_, _> = held
        .iter()
        .map(|row| (row.repository_id, row.applied_version))
        .collect();
    for row in held {
        if quorum {
            let view =
                codeyard_db_placement::cluster::writes::recovery_view(pool, row.repository_id)
                    .await?;
            if !view.serving {
                // Reconstruction belongs to the owned copy queue, not this scan.
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
            codeyard_db_placement::cluster::writes::write_family(pool, row.repository_id).await?
        } else {
            codeyard_db_placement::cluster::list_fork_family(pool, row.repository_id).await?
        };
        let members = members_on(&family, from);
        decided.extend(members.iter().map(|(member, _)| *member));
        if members.is_empty() {
            // Another node has already taken the family off this primary.
            continue;
        }
        if quorum {
            let view =
                codeyard_db_placement::cluster::writes::recovery_view(pool, row.repository_id)
                    .await?;
            let settings = codeyard_db_placement::cluster::writes::write_settings(pool)
                .await?
                .write_quorum;
            let required = codeyard_db_placement::cluster::writes::required_votes(
                settings.as_deref().context("write quorum is disabled")?,
                view.voters.len(),
            )?;
            if view
                .voters
                .iter()
                .filter(|voter| is_reporting_storage(&nodes, voter.node_id))
                .count()
                < required as usize
            {
                continue;
            }
            match promote_quorum(pool, row.repository_id, Some(node), None).await {
                Ok(promotions) => {
                    taken.extend(promotions.into_iter().map(|promotion| promotion.repository))
                }
                Err(error) if error.is::<RecoveryPending>() => {}
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
            codeyard_db_placement::cluster::promote_fork_family(pool, row.repository_id, from, node)
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
    let row = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository)
        .await?
        .context("the repository does not exist")?;
    let owner = codeyard_db_repos::repositories::records::repository_owner_name(pool, repository)
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
        let copied = codeyard_db_placement::cluster::list_replicas(pool, member.0)
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
        let applied = codeyard_db_placement::cluster::list_replicas(pool, member)
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
        node.id == id
            && node.status == "active"
            && node.state == "active"
            && node.roles.iter().any(|role| role == "storage")
    })
}

/// Whether a promotion may move a repository to `id`: a storage node that is reporting in, by
/// the database clock. The promotion transaction checks the same thing again.
fn is_reporting_storage(nodes: &[ClusterNode], id: i64) -> bool {
    nodes
        .iter()
        .any(|node| node.id == id && node.state == "active" && node.is_serving_storage())
}
