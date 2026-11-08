//! Incremental log durability, cursors and retention on both native engines.
use crate::support::database::Database;

use codeyard_ci::config::CiConfig;

use codeyard_database::models::CiRun;
use codeyard_db_actions::action_logs::Target;
use codeyard_runner::progress::Event;
use codeyard_runner::progress::Update;
use codeyard_types::ci::LogLine;
use std::io::Read;

const WORKER: &str = "chunk-worker";
const WORKFLOW: &str = ".github/workflows/ci.yml";
const RESOURCE: &str = "/chunk-test/logs";

struct Fixture {
    db: Database,
    owner: i64,
    run: CiRun,
    jobs: [i64; 2],
}

impl Fixture {
    async fn new(pg: bool) -> Self {
        let db = Database::new(pg).await;
        let owner = db.actor("chunk_owner").await;
        let repo = codeyard_db_repos::repositories::records::create_repository(
            db.pool(),
            owner,
            "project",
            None,
            true,
            "/fixture.git",
        )
        .await
        .unwrap();
        let sha = "1".repeat(40);
        codeyard_db_actions::ci::enqueue_ci_run(
            db.pool(),
            codeyard_db_actions::ci::NewCiRun {
                repo_id: repo.id,
                triggered_by_id: owner,
                pipeline: "ci",
                event: "workflow_dispatch",
                pull_request_id: None,
                branch: "main",
                head_sha: &sha,
                workflow_path: WORKFLOW,
                workflow_blobs: &serde_json::json!({ ".github/workflows/ci.yml": sha }),
                workflow_inputs: None,
                environment: None,
                manual: false,
            },
            &[owner],
        )
        .await
        .unwrap();
        let run = codeyard_db_actions::ci::claim_ci_run(
            db.pool(),
            WORKER,
            "local",
            &[owner],
            &CiConfig::default().platform_labels(),
        )
        .await
        .unwrap()
        .unwrap();
        let generation = codeyard_support::token_hash::token_hash("chunk-runtime");
        codeyard_db_actions::action_objects::issue_token(
            db.pool(),
            run.id,
            WORKER,
            &generation,
            &run.run_id.to_string(),
        )
        .await
        .unwrap();
        super::actions_objects::job_credentials(&db, &run, &["build"]).await;
        let mut jobs = [0; 2];
        for (variant, job) in jobs.iter_mut().enumerate() {
            for update in [
                Update::Queued {
                    workflow_name: "Build".into(),
                    name: format!("Build {variant}"),
                    labels: vec!["linux".into()],
                    matrix: None,
                },
                Update::Started,
                Update::StepStarted {
                    number: 1,
                    name: "Compile".into(),
                },
                Update::StepStarted {
                    number: 2,
                    name: "Test".into(),
                },
            ] {
                *job = codeyard_db_actions::action_jobs::apply(
                    db.pool(),
                    WORKER,
                    run.id,
                    &generation,
                    WORKFLOW,
                    &Event {
                        job: "build".into(),
                        variant: variant as u32,
                        at_ms: chrono::Utc::now().timestamp_millis() as u64,
                        update,
                    },
                )
                .await
                .unwrap()
                .id;
            }
        }
        Self {
            db,
            owner,
            run,
            jobs,
        }
    }

    fn line(&self, number: i64) -> LogLine {
        LogLine {
            stage: WORKFLOW.into(),
            line: format!("line {number}"),
            level: "info".into(),
            job_id: Some(self.jobs[0]),
            step_number: Some(1),
        }
    }

    async fn append(&self, sequence: i64, lines: &[LogLine]) -> bool {
        codeyard_db_actions::ci::append_ci_logs(
            self.db.pool(),
            self.run.id,
            WORKER,
            sequence,
            lines,
        )
        .await
        .unwrap()
    }

    async fn capability(&self, secret: &str, job: Option<i64>, step: Option<i32>) -> String {
        let hash = codeyard_support::token_hash::token_hash(secret);
        assert!(codeyard_db_actions::action_logs::issue(
            self.db.pool(),
            self.owner,
            self.run.repo_id,
            RESOURCE,
            Target {
                run: self.run.id,
                job,
                step
            },
            &hash
        )
        .await
        .unwrap());
        hash
    }
}

async fn idempotent_append(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let batch = vec![f.line(1); 64];
    let (first, replay) = tokio::join!(
        codeyard_db_actions::ci::append_ci_logs(f.db.pool(), f.run.id, WORKER, 0, &batch),
        codeyard_db_actions::ci::append_ci_logs(f.db.pool(), f.run.id, WORKER, 0, &batch),
    );
    assert!(first.unwrap() && replay.unwrap());
    assert_eq!(
        f.db.scalar(
            "SELECT COUNT(*) FROM ci_log_chunks WHERE run_id=$1",
            &[f.run.id]
        )
        .await,
        1
    );
    assert_eq!(
        f.db.scalar("SELECT log_lines FROM ci_runs WHERE id=$1", &[f.run.id])
            .await,
        64
    );
    let compressed_size = "SELECT object_bytes FROM ci_log_chunks WHERE run_id=$1";
    let size: i64 = batch
        .iter()
        .map(|line| (line.line.len() + line.stage.len()) as i64)
        .sum();
    assert!(f.db.scalar(compressed_size, &[f.run.id]).await < size);
    assert_eq!(
        f.db.scalar("SELECT log_bytes FROM ci_runs WHERE id=$1", &[f.run.id])
            .await,
        size
    );
    assert_eq!(
        f.db.scalar(
            "SELECT byte_size FROM ci_log_chunks WHERE run_id=$1",
            &[f.run.id]
        )
        .await,
        size
    );
    assert!(codeyard_db_actions::ci::append_ci_logs(
        f.db.pool(),
        f.run.id,
        WORKER,
        0,
        &[f.line(2)]
    )
    .await
    .is_err());
    assert!(codeyard_db_actions::ci::append_ci_logs(
        f.db.pool(),
        f.run.id,
        WORKER,
        2,
        &[f.line(2)]
    )
    .await
    .is_err());
    assert!(
        codeyard_db_actions::ci::append_ci_logs(f.db.pool(), f.run.id, "other", 0, &batch)
            .await
            .is_err()
    );
    let mixed = [
        f.line(65),
        LogLine {
            job_id: Some(f.jobs[1]),
            step_number: Some(2),
            ..f.line(66)
        },
        f.line(67),
    ];
    assert!(f.append(1, &mixed).await);
    assert!(f.append(1, &mixed).await);
    assert_eq!(
        f.db.scalar(
            "SELECT COUNT(*) FROM ci_log_chunks WHERE run_id=$1",
            &[f.run.id]
        )
        .await,
        2
    );
    let lines = codeyard_db_actions::ci::list_ci_logs(f.db.pool(), f.run.id, 3)
        .await
        .unwrap();
    assert_eq!(
        lines.iter().map(|line| line.id).collect::<Vec<_>>(),
        [65, 66, 67]
    );
    assert_eq!(lines[1].job_id, Some(f.jobs[1]));
    assert_eq!(lines[1].step_number, Some(2));
    f.db.finish().await;
}

async fn ordered_read_across_500_chunks(pg: bool) {
    let mut f = Fixture::new(pg).await;
    for sequence in 0..500 {
        assert!(
            f.append(
                sequence,
                &[f.line(sequence * 2 + 1), f.line(sequence * 2 + 2)]
            )
            .await
        );
    }
    assert_eq!(
        f.db.scalar(
            "SELECT COUNT(*) FROM ci_log_chunks WHERE run_id=$1",
            &[f.run.id]
        )
        .await,
        500
    );
    let lines = codeyard_db_actions::ci::list_ci_logs(f.db.pool(), f.run.id, 1000)
        .await
        .unwrap();
    assert_eq!(lines.len(), 1000);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line.id, index as i64 + 1);
        assert_eq!(line.line, format!("line {}", index + 1));
        assert_eq!(line.job_id, Some(f.jobs[0]));
    }
    let hash = f.capability("whole", None, None).await;
    let snapshot = codeyard_db_actions::action_logs::read(f.db.pool(), &hash, RESOURCE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.lines.len(), 1000);
    let (bytes, kind, _) = codeyard_ci::logs::archive::encode(snapshot).unwrap();
    assert_eq!(kind, "application/zip");
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut text = String::new();
    zip.by_name("0_Build 0.txt")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text.lines().count(), 1000);
    assert!(text.lines().last().unwrap().ends_with(" line 1000"));
    f.db.finish().await;
}

async fn range_read(pg: bool) {
    let mut f = Fixture::new(pg).await;
    for sequence in 0..5 {
        let batch: Vec<_> = (1..=64)
            .map(|offset| f.line(sequence * 64 + offset))
            .collect();
        assert!(f.append(sequence, &batch).await);
    }
    let lines = codeyard_db_actions::ci::list_ci_log_history(f.db.pool(), f.run.id, 63, 5)
        .await
        .unwrap();
    assert_eq!(
        lines.iter().map(|line| line.id).collect::<Vec<_>>(),
        [64, 65, 66, 67, 68]
    );
    let mut after = 0;
    let mut all = Vec::new();
    loop {
        let page = codeyard_db_actions::ci::list_ci_log_history(f.db.pool(), f.run.id, after, 37)
            .await
            .unwrap();
        let Some(last) = page.last() else { break };
        after = last.id;
        all.extend(page.into_iter().map(|line| line.id));
    }
    assert_eq!(all, (1..=320).collect::<Vec<_>>());
    assert_eq!(
        codeyard_db_actions::ci::list_ci_logs(f.db.pool(), f.run.id, 5)
            .await
            .unwrap()[0]
            .id,
        316
    );
    assert!(
        codeyard_db_actions::ci::list_ci_logs(f.db.pool(), f.run.id, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        codeyard_db_actions::ci::list_ci_log_history(f.db.pool(), f.run.id, i64::MAX, 5)
            .await
            .unwrap()
            .is_empty()
    );
    let hash = f.capability("frozen-range", Some(f.jobs[0]), Some(1)).await;
    f.append(5, &[f.line(321)]).await;
    let snapshot = codeyard_db_actions::action_logs::read(f.db.pool(), &hash, RESOURCE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.lines.len(), 320);
    assert_eq!(snapshot.lines.last().unwrap().id, 320);
    assert!(
        codeyard_db_actions::action_logs::read(f.db.pool(), &hash, "/different")
            .await
            .unwrap()
            .is_none()
    );
    f.db.finish().await;
}

async fn kill_mid_job(pg: bool) {
    let mut f = Fixture::new(pg).await;
    for sequence in 0..5 {
        f.append(sequence, &[f.line(sequence + 1)]).await;
    }
    // A killed worker sends no final result or heartbeat. Recovery uses the durable lease.
    let stopped = if pg {
        "UPDATE ci_runs SET heartbeat_at=NOW()-INTERVAL '1 hour' WHERE id=$1"
    } else {
        "UPDATE ci_runs SET heartbeat_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour') WHERE id=$1"
    };
    f.db.execute(stopped, &[f.run.id]).await;
    let (failed, _, _) = codeyard_db_actions::ci::fail_stale_ci_runs(f.db.pool(), 90)
        .await
        .unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].status, "failed");
    let job = codeyard_db_actions::action_jobs::get(f.db.pool(), f.run.repo_id, f.jobs[0], false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "completed");
    assert_eq!(job.conclusion.as_deref(), Some("failure"));
    assert_eq!(
        codeyard_db_actions::action_jobs::steps(f.db.pool(), f.run.repo_id, f.jobs[0])
            .await
            .unwrap()[0]
            .conclusion
            .as_deref(),
        Some("failure")
    );
    assert!(
        codeyard_db_actions::ci::finish_ci_run(f.db.pool(), f.run.id, WORKER, "success", None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(codeyard_db_actions::ci::append_ci_logs(
        f.db.pool(),
        f.run.id,
        WORKER,
        5,
        &[f.line(6)]
    )
    .await
    .is_err());
    let lines = codeyard_db_actions::ci::list_ci_log_history(f.db.pool(), f.run.id, 0, 100)
        .await
        .unwrap();
    assert_eq!(
        lines
            .iter()
            .map(|line| line.line.as_str())
            .collect::<Vec<_>>(),
        ["line 1", "line 2", "line 3", "line 4", "line 5"]
    );
    let hash = f.capability("killed", None, None).await;
    assert_eq!(
        codeyard_db_actions::action_logs::read(f.db.pool(), &hash, RESOURCE)
            .await
            .unwrap()
            .unwrap()
            .lines
            .len(),
        5
    );
    f.db.finish().await;
}

async fn truncation_marker_can_extend_a_full_batch(pg: bool) {
    let mut f = Fixture::new(pg).await;
    f.db.execute(
        "UPDATE ci_runs SET log_lines=99983 WHERE id=$1",
        &[f.run.id],
    )
    .await;
    let mut batch = vec![
        LogLine {
            stage: String::new(),
            line: "x".repeat(16 * 1024),
            ..f.line(1)
        };
        16
    ];
    batch.push(LogLine {
        stage: String::new(),
        line: String::new(),
        ..f.line(2)
    });
    assert!(!f.append(0, &batch).await);
    let lines = codeyard_db_actions::ci::list_ci_logs(f.db.pool(), f.run.id, 100)
        .await
        .unwrap();
    assert_eq!(lines.len(), 17);
    assert!(lines[..16]
        .iter()
        .all(|line| line.line == "x".repeat(16 * 1024)));
    assert_eq!(lines[16].id, 100_000);
    assert_eq!(lines[16].level, "warn");
    assert!(lines[16].line.starts_with("Log output truncated:"));
    assert!(
        f.db.scalar(
            "SELECT byte_size FROM ci_log_chunks WHERE run_id=$1",
            &[f.run.id]
        )
        .await
            > 256 * 1024
    );
    assert_eq!(
        f.db.scalar("SELECT log_lines FROM ci_runs WHERE id=$1", &[f.run.id])
            .await,
        100_000
    );
    assert!(!f.append(1, &[f.line(3)]).await);
    assert_eq!(
        f.db.scalar(
            "SELECT COUNT(*) FROM ci_log_chunks WHERE run_id=$1",
            &[f.run.id]
        )
        .await,
        1
    );
    f.db.finish().await;
}

async fn expiry(pg: bool) {
    let mut f = Fixture::new(pg).await;
    f.append(0, &[f.line(1), f.line(2)]).await;
    codeyard_db_actions::ci::finish_ci_run(f.db.pool(), f.run.id, WORKER, "success", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.db.scalar(
            "SELECT COUNT(*) FROM ci_runs WHERE id=$1 AND log_expires_at>0",
            &[f.run.id]
        )
        .await,
        1
    );
    let hash = f.capability("expiring", None, None).await;
    assert!(
        codeyard_db_actions::action_logs::read(f.db.pool(), &hash, RESOURCE)
            .await
            .unwrap()
            .is_some()
    );
    f.db.execute(
        "UPDATE ci_runs SET log_expires_at=0 WHERE id=$1",
        &[f.run.id],
    )
    .await;
    assert!(
        codeyard_db_actions::ci::list_ci_logs(f.db.pool(), f.run.id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        codeyard_db_actions::ci::list_ci_log_history(f.db.pool(), f.run.id, 0, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        codeyard_db_actions::action_logs::read(f.db.pool(), &hash, RESOURCE)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!codeyard_db_actions::action_logs::issue(
        f.db.pool(),
        f.owner,
        f.run.repo_id,
        RESOURCE,
        Target {
            run: f.run.id,
            job: None,
            step: None
        },
        &codeyard_support::token_hash::token_hash("expired")
    )
    .await
    .unwrap());
    codeyard_db_actions::action_objects::expire_objects(f.db.pool())
        .await
        .unwrap();
    assert_eq!(
        f.db.scalar(
            "SELECT COUNT(*) FROM ci_log_chunks WHERE run_id=$1",
            &[f.run.id]
        )
        .await,
        0
    );
    assert_eq!(
        f.db.scalar("SELECT log_version FROM ci_runs WHERE id=$1", &[f.run.id])
            .await,
        1
    );
    assert_eq!(
        f.db.scalar(
            "SELECT log_lines+log_bytes FROM ci_runs WHERE id=$1",
            &[f.run.id]
        )
        .await,
        0
    );
    assert!(
        codeyard_db_actions::action_logs::read(f.db.pool(), &hash, RESOURCE)
            .await
            .unwrap()
            .is_none()
    );
    f.db.finish().await;
}

#[tokio::test]
async fn sqlite_idempotent_append() {
    idempotent_append(false).await;
}
#[tokio::test]
async fn postgres_idempotent_append() {
    idempotent_append(true).await;
}
#[tokio::test]
async fn sqlite_ordered_read_across_500_chunks() {
    ordered_read_across_500_chunks(false).await;
}
#[tokio::test]
async fn postgres_ordered_read_across_500_chunks() {
    ordered_read_across_500_chunks(true).await;
}
#[tokio::test]
async fn sqlite_range_read() {
    range_read(false).await;
}
#[tokio::test]
async fn postgres_range_read() {
    range_read(true).await;
}
#[tokio::test]
async fn sqlite_kill_mid_job() {
    kill_mid_job(false).await;
}
#[tokio::test]
async fn postgres_kill_mid_job() {
    kill_mid_job(true).await;
}
#[tokio::test]
async fn sqlite_truncation_marker_can_extend_a_full_batch() {
    truncation_marker_can_extend_a_full_batch(false).await;
}
#[tokio::test]
async fn postgres_truncation_marker_can_extend_a_full_batch() {
    truncation_marker_can_extend_a_full_batch(true).await;
}
#[tokio::test]
async fn sqlite_expiry() {
    expiry(false).await;
}
#[tokio::test]
async fn postgres_expiry() {
    expiry(true).await;
}

#[derive(Clone, Copy)]
enum ObjectFault {
    Put,
    Revoke,
    Deleted,
    Corrupt,
}
struct FaultingObjects {
    inner: std::sync::Arc<dyn codeyard_database::ci_logs::LogObjects>,
    pool: codeyard_database::DbPool,
    run: i64,
    fault: ObjectFault,
}
impl codeyard_database::ci_logs::LogObjects for FaultingObjects {
    fn put<'a>(
        &'a self,
        key: &'a str,
        digest: &'a str,
        bytes: Vec<u8>,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if matches!(self.fault, ObjectFault::Put) {
                anyhow::bail!("owned object failure");
            }
            self.inner.put(key, digest, bytes).await?;
            if matches!(self.fault, ObjectFault::Revoke | ObjectFault::Deleted) {
                let statement = if matches!(self.fault, ObjectFault::Revoke) {
                    "UPDATE ci_runs SET status='failed' WHERE id=$1"
                } else {
                    "UPDATE ci_runs SET log_version=log_version+1 WHERE id=$1"
                };
                match &self.pool {
                    codeyard_database::DbPool::Postgres(pool) => {
                        sqlx::query(statement).bind(self.run).execute(pool).await?;
                    }
                    codeyard_database::DbPool::Sqlite(pool) => {
                        sqlx::query(statement).bind(self.run).execute(pool).await?;
                    }
                }
            }
            Ok(())
        })
    }
    fn get<'a>(
        &'a self,
        key: &'a str,
        length: usize,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async move {
            let mut bytes = self.inner.get(key, length).await?;
            if matches!(self.fault, ObjectFault::Corrupt) {
                bytes[0] ^= 1;
            }
            Ok(bytes)
        })
    }
}

async fn object_failure(pg: bool, fault: ObjectFault) {
    let mut fixture = Fixture::new(pg).await;
    let original = fixture.db.pool().log_objects().unwrap();
    let objects = std::sync::Arc::new(FaultingObjects {
        inner: original.clone(),
        pool: fixture.db.pool().clone(),
        run: fixture.run.id,
        fault,
    });
    fixture.db.pool().set_log_objects(objects.clone());
    let result = codeyard_db_actions::ci::append_ci_logs(
        fixture.db.pool(),
        fixture.run.id,
        WORKER,
        0,
        &[fixture.line(1)],
    )
    .await;
    if matches!(fault, ObjectFault::Put) {
        // Retries cannot grow the pending queue or replace an unacknowledged batch.
        assert!(codeyard_db_actions::ci::append_ci_logs(
            fixture.db.pool(),
            fixture.run.id,
            WORKER,
            0,
            &[fixture.line(1)]
        )
        .await
        .is_err());
        assert!(codeyard_db_actions::ci::append_ci_logs(
            fixture.db.pool(),
            fixture.run.id,
            WORKER,
            0,
            &[fixture.line(2)]
        )
        .await
        .is_err());
    }
    if matches!(fault, ObjectFault::Corrupt) {
        assert!(result.unwrap());
        assert!(
            codeyard_db_actions::ci::list_ci_logs(fixture.db.pool(), fixture.run.id, 100)
                .await
                .is_err()
        );
        fixture.db.pool().set_log_objects(original.clone());
        assert_eq!(
            codeyard_db_actions::ci::list_ci_logs(fixture.db.pool(), fixture.run.id, 100)
                .await
                .unwrap()
                .len(),
            1
        );
        fixture
            .db
            .execute(
                "DELETE FROM ci_log_chunks WHERE run_id=$1",
                &[fixture.run.id],
            )
            .await;
    } else {
        if matches!(fault, ObjectFault::Deleted) {
            assert!(!result.unwrap());
            assert!(!fixture.append(0, &[fixture.line(1)]).await);
        } else {
            assert!(result.is_err());
        }
        assert_eq!(
            fixture
                .db
                .scalar(
                    "SELECT log_lines FROM ci_runs WHERE id=$1",
                    &[fixture.run.id]
                )
                .await,
            0
        );
    }
    assert_eq!(
        fixture
            .db
            .scalar(
                "SELECT COUNT(*) FROM ci_log_chunks WHERE run_id=$1",
                &[fixture.run.id]
            )
            .await,
        0
    );
    assert_eq!(
        fixture
            .db
            .scalar(
                "SELECT COUNT(*) FROM pending_object_deletions WHERE object_key LIKE 'ci-logs/%'",
                &[]
            )
            .await,
        1
    );
    fixture.db.pool().set_log_objects(original);
    fixture.db.finish().await;
}

#[tokio::test]
async fn sqlite_log_object_failure_never_acknowledges_or_increments_totals() {
    object_failure(false, ObjectFault::Put).await;
}
#[tokio::test]
async fn postgres_log_object_failure_never_acknowledges_or_increments_totals() {
    object_failure(true, ObjectFault::Put).await;
}
#[tokio::test]
async fn sqlite_worker_revoked_during_object_upload_cannot_commit() {
    object_failure(false, ObjectFault::Revoke).await;
}
#[tokio::test]
async fn postgres_worker_revoked_during_object_upload_cannot_commit() {
    object_failure(true, ObjectFault::Revoke).await;
}
#[tokio::test]
async fn sqlite_corrupt_log_objects_are_refused_and_deleted_chunks_schedule_cleanup() {
    object_failure(false, ObjectFault::Corrupt).await;
}
#[tokio::test]
async fn postgres_corrupt_log_objects_are_refused_and_deleted_chunks_schedule_cleanup() {
    object_failure(true, ObjectFault::Corrupt).await;
}

#[tokio::test]
async fn sqlite_logs_deleted_during_object_upload_stay_closed() {
    object_failure(false, ObjectFault::Deleted).await;
}
#[tokio::test]
async fn postgres_logs_deleted_during_object_upload_stay_closed() {
    object_failure(true, ObjectFault::Deleted).await;
}
