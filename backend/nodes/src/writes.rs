//! Fixed voters and a database-decided quorum. Git preparation and publication belong to the stores;
//! only their durable votes and applied versions can complete a write here.
use anyhow::Context;

use anyhow::ensure;

use anyhow::Result;
use codeyard_database::models::RecoveryView;
use codeyard_database::models::RefChange;
use codeyard_database::models::WriteRecord;
use codeyard_database::DbPool;
use std::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio::time::timeout_at;
use tokio::time::Instant;

static QUORUM_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn quorum_failures() -> u64 {
    QUORUM_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

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
        let repository = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo)
            .await?
            .context("repository unavailable")?;
        if repository.storage_node_id.is_some() {
            ensure!(
                codeyard_db_placement::cluster::writes::write_settings(pool)
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
    voters: Vec<i64>,
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
    let result = begin_write(pool, repo, primary, term, id, changes).await;
    if result.is_err() {
        QUORUM_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    result
}

async fn begin_write(
    pool: &DbPool,
    repo: i64,
    primary: i64,
    term: i64,
    id: &str,
    changes: &[RefChange],
) -> Result<PreparedWrite> {
    let settings = timeout(
        CONTROL_TIMEOUT,
        codeyard_db_placement::cluster::writes::write_settings(pool),
    )
    .await??;
    ensure!(settings.write_quorum.is_some(), "write quorum is disabled");
    let mut deadline = Instant::now() + Duration::from_secs(settings.write_timeout_seconds as u64);
    let view = timeout_at(
        deadline,
        codeyard_db_placement::cluster::writes::recovery_view(pool, repo),
    )
    .await??;
    ensure!(
        view.primary_id == primary && view.term == term && view.serving,
        "repository term is not serving"
    );
    let required = codeyard_db_placement::cluster::writes::required_votes(
        settings
            .write_quorum
            .as_deref()
            .context("write quorum is disabled")?,
        view.voters.len(),
    )?;
    ensure!(
        required as usize > view.voters.len() / 2,
        "write quorum is below a majority of fixed voters"
    );
    ensure!(
        view.voters.iter().any(|voter| voter.node_id == primary
            && voter.term == term
            && voter.applied_version == view.committed_version),
        "primary has not applied committed refs"
    );
    let result = timeout_at(
        deadline,
        codeyard_db_placement::cluster::writes::begin_write(pool, repo, primary, term, id, changes),
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
                codeyard_db_placement::cluster::writes::load_write(pool, id),
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
            && record.required_votes == required
            && record.manifest_hash
                == codeyard_db_placement::cluster::writes::manifest_hash(
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
        voters: view.voters.iter().map(|voter| voter.node_id).collect(),
        deadline,
    })
}

/// Waits for votes recorded by the stores after their objects and pins are durable.
/// This never records a vote on another node's behalf.
pub async fn collect_votes(pool: &DbPool, write: &PreparedWrite) -> Result<bool> {
    loop {
        let record = match timeout_at(
            write.deadline,
            codeyard_db_placement::cluster::writes::load_write(pool, &write.record.id),
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
            .filter(|node| record.votes.iter().any(|vote| vote.node_id == **node))
            .count() as i64
            >= write.record.required_votes
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
        codeyard_db_placement::cluster::writes::decide_write(
            pool,
            &write.record.id,
            write.record.primary_id,
            write.record.term,
            commit && Instant::now() < write.deadline,
        ),
    )
    .await;
    if let Ok(Ok(record)) = result {
        if record.decision == "abort" {
            QUORUM_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        return Ok(record);
    }
    let record = timeout(
        CONTROL_TIMEOUT,
        codeyard_db_placement::cluster::writes::load_write(pool, &write.record.id),
    )
    .await
    .map_err(|_| OutcomeUnknown)?
    .map_err(|_| OutcomeUnknown)?
    .ok_or(OutcomeUnknown)?;
    if record.decision == "abort" {
        QUORUM_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    ensure!(record.decision != "pending", OutcomeUnknown);
    Ok(record)
}

/// Called after durable Git publication and mark_write_applied on each store. Only
/// finish_write's database check can release terminal success to HTTP or SSH.
pub async fn publish(pool: &DbPool, write: &WriteRecord) -> Result<()> {
    ensure!(write.decision == "commit", "write is not committed");
    match timeout(
        CONTROL_TIMEOUT,
        codeyard_db_placement::cluster::writes::finish_write(pool, &write.id, write.term),
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
        let view = codeyard_db_placement::cluster::writes::recovery_view(pool, repo).await?;
        ensure!(
            view.primary_id == primary && view.term == term && !view.serving,
            "recovery requires a fenced term"
        );
        if let Some(write) = &view.unfinished {
            if write.decision == "pending" {
                codeyard_db_placement::cluster::writes::decide_write(
                    pool, &write.id, primary, term, false,
                )
                .await?;
                return codeyard_db_placement::cluster::writes::recovery_view(pool, repo).await;
            }
        }
        Ok(view)
    })
    .await?
}
