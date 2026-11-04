//! Two fixed voters, both required. Git preparation and publication belong to the stores;
//! only their durable votes and applied versions can complete a write here.

use anyhow::{ensure, Context, Result};
use codeyard_db_core::models::RecoveryView;
use codeyard_db_core::models::RefChange;
use codeyard_db_core::models::WriteRecord;
use codeyard_db_core::DbPool;
use std::time::Duration;
use tokio::time::{sleep, timeout, timeout_at, Instant};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const VOTE_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
#[error("write outcome unknown; fetch before retrying")]
pub struct OutcomeUnknown;

#[derive(Debug, thiserror::Error)]
#[error("this publisher is unavailable with write quorum")]
pub struct PublisherUnavailable;

/// Slice 1 admits only receive-pack. Other publishers must not escape the journal
/// through the legacy attempted-write counter while their prepared/apply path is absent.
pub async fn require_legacy_publisher(pool: &DbPool, repo: i64) -> Result<()> {
    timeout(CONTROL_TIMEOUT, async {
        let repository =
            crate::backend::db::repositories::records::get_repository_by_id(pool, repo)
                .await?
                .context("repository unavailable")?;
        if repository.storage_node_id.is_some() {
            ensure!(
                crate::backend::db::cluster::writes::write_settings(pool)
                    .await?
                    .write_quorum
                    .is_none(),
                PublisherUnavailable
            );
        }
        Ok(())
    })
    .await?
}

/// A preparation's immutable inputs and local wait budget. The database deadline is
/// authoritative; the monotonic budget also bounds waiting when a peer's clock differs.
pub struct PreparedWrite {
    record: WriteRecord,
    voters: [i64; 2],
    deadline: Instant,
}

impl PreparedWrite {
    pub fn record(&self) -> &WriteRecord {
        &self.record
    }
}

pub async fn begin(
    pool: &DbPool,
    repo: i64,
    primary: i64,
    term: i64,
    id: &str,
    changes: &[RefChange],
) -> Result<PreparedWrite> {
    let settings = timeout(
        CONTROL_TIMEOUT,
        crate::backend::db::cluster::writes::write_settings(pool),
    )
    .await??;
    ensure!(settings.write_quorum.is_some(), "write quorum is disabled");
    let mut deadline = Instant::now() + Duration::from_secs(settings.write_timeout_seconds as u64);
    let view = timeout_at(
        deadline,
        crate::backend::db::cluster::writes::recovery_view(pool, repo),
    )
    .await??;
    ensure!(
        view.primary_id == primary && view.term == term && view.serving,
        "repository term is not serving"
    );
    ensure!(
        view.voters.len() == 2,
        "write quorum requires two fixed voters"
    );
    ensure!(
        view.voters
            .iter()
            .all(|voter| { voter.term == term && voter.applied_version == view.committed_version })
            && view.voters.iter().any(|voter| voter.node_id == primary),
        "voters have not applied committed refs"
    );
    ensure!(
        crate::backend::db::cluster::writes::required_votes(
            settings
                .write_quorum
                .as_deref()
                .context("write quorum is disabled")?,
            2,
        )? == 2,
        "two-voter mode requires both votes"
    );
    let result = timeout_at(
        deadline,
        crate::backend::db::cluster::writes::begin_write(pool, repo, primary, term, id, changes),
    )
    .await;
    let record = match result {
        Ok(Ok(record)) => record,
        _ => {
            // A lost INSERT reply is not permission to prepare another operation
            // or start a fresh wait budget. A recovered pending write must abort.
            deadline = Instant::now();
            timeout(
                CONTROL_TIMEOUT,
                crate::backend::db::cluster::writes::load_write(pool, id),
            )
            .await
            .map_err(|_| OutcomeUnknown)?
            .map_err(|_| OutcomeUnknown)?
            .ok_or(OutcomeUnknown)?
        }
    };
    ensure!(
        record.repository_id == repo
            && record.primary_id == primary
            && record.term == term
            && record.required_votes == 2
            && record.manifest_hash
                == crate::backend::db::cluster::writes::manifest_hash(
                    repo,
                    primary,
                    term,
                    record.base_version,
                    changes
                )?,
        "write identity mismatch"
    );
    Ok(PreparedWrite {
        record,
        voters: [view.voters[0].node_id, view.voters[1].node_id],
        deadline,
    })
}

/// Waits for votes recorded by the stores after their objects and pins are durable.
/// This never records a vote on another node's behalf.
pub async fn collect_votes(pool: &DbPool, write: &PreparedWrite) -> Result<bool> {
    loop {
        let record = match timeout_at(
            write.deadline,
            crate::backend::db::cluster::writes::load_write(pool, &write.record.id),
        )
        .await
        {
            Ok(result) => result?.context("write disappeared")?,
            Err(_) => return Ok(false),
        };
        ensure!(
            record.manifest_hash == write.record.manifest_hash,
            "write identity mismatch"
        );
        match record.decision.as_str() {
            "commit" => return Ok(true),
            "abort" => return Ok(false),
            "pending" => {}
            _ => anyhow::bail!("invalid write decision"),
        }
        if write
            .voters
            .iter()
            .all(|node| record.votes.iter().any(|vote| vote.node_id == *node))
        {
            return Ok(true);
        }
        if timeout_at(write.deadline, sleep(VOTE_INTERVAL))
            .await
            .is_err()
        {
            return Ok(false);
        }
    }
}

/// An uncertain SQL reply is resolved by the same operation id, never by rollback.
pub async fn decide(pool: &DbPool, write: &PreparedWrite, commit: bool) -> Result<WriteRecord> {
    let result = timeout(
        CONTROL_TIMEOUT,
        crate::backend::db::cluster::writes::decide_write(
            pool,
            &write.record.id,
            write.record.primary_id,
            write.record.term,
            commit && Instant::now() < write.deadline,
        ),
    )
    .await;
    if let Ok(Ok(record)) = result {
        return Ok(record);
    }
    let record = timeout(
        CONTROL_TIMEOUT,
        crate::backend::db::cluster::writes::load_write(pool, &write.record.id),
    )
    .await
    .map_err(|_| OutcomeUnknown)?
    .map_err(|_| OutcomeUnknown)?
    .ok_or(OutcomeUnknown)?;
    ensure!(record.decision != "pending", OutcomeUnknown);
    Ok(record)
}

/// Called after durable Git publication and mark_write_applied on each store. Only
/// finish_write's database check can release terminal success to HTTP or SSH.
pub async fn publish(pool: &DbPool, write: &WriteRecord) -> Result<()> {
    ensure!(write.decision == "commit", "write is not committed");
    match timeout(
        CONTROL_TIMEOUT,
        crate::backend::db::cluster::writes::finish_write(pool, &write.id, write.term),
    )
    .await
    {
        Ok(Ok(true)) => Ok(()),
        _ => Err(OutcomeUnknown.into()),
    }
}

/// Recovery runs in a fenced, non-serving term. The caller must reconstruct from
/// these canonical refs, clean aborted preparations and finish before enabling it.
pub async fn recover(pool: &DbPool, repo: i64, primary: i64, term: i64) -> Result<RecoveryView> {
    timeout(CONTROL_TIMEOUT, async {
        let view = crate::backend::db::cluster::writes::recovery_view(pool, repo).await?;
        ensure!(
            view.primary_id == primary && view.term == term && !view.serving,
            "recovery requires a fenced term"
        );
        if let Some(write) = &view.unfinished {
            if write.decision == "pending" {
                crate::backend::db::cluster::writes::decide_write(
                    pool, &write.id, primary, term, false,
                )
                .await?;
                return crate::backend::db::cluster::writes::recovery_view(pool, repo).await;
            }
        }
        Ok(view)
    })
    .await?
}
