//! The CI queue while authority changes, on disposable SQLite and PostgreSQL
//! databases: a claim checks again that the person who started a run may still
//! run it, access revocations stop queued and running runs, the ways a run can
//! end cannot overwrite each other, and the global run list follows the
//! repository read rule.
//!
//! The PostgreSQL tests need GITSERV_TEST_DATABASE_URL and fail without it. The
//! tests in `interleaved` hold a row lock in a second transaction to put two
//! operations in a fixed order; PostgreSQL's own wait information says when the
//! operation under test is really waiting, so the order never depends on sleeping.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::native_database as native;
use crate::support::production_ci as production;

use codeyard::backend::db::{
    self,
    models::{AccessLevel, CiRun, OrgRole},
    organizations::{
        self as orgs, NewOrganization, NewTeam, OrganizationOutcome, OrganizationProfile,
        TeamPrivacy, TeamProfile, TeamRepositoryPermission, TeamRole,
    },
};
use std::{
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use uuid::Uuid;

const WORKER: &str = "worker-1";
const ACCESS_LOST: &str = db::ci::ACCESS_LOST_MESSAGE;
const WORKFLOW: &str = ".github/workflows/ci.yml";

enum Database {
    Postgres(Box<production::Fixture>),
    Sqlite(native::NativeDatabase),
}

impl Database {
    async fn new(pg: bool) -> Self {
        if pg {
            Self::Postgres(Box::new(production::Fixture::new().await))
        } else {
            Self::Sqlite(native::NativeDatabase::new().await)
        }
    }

    fn pool(&self) -> &db::DbPool {
        match self {
            Self::Postgres(f) => &f.pool,
            Self::Sqlite(f) => &f.pool,
        }
    }

    /// Fixture SQL that is the same on both engines, with numbered id parameters.
    async fn execute(&self, sql: &str, ids: &[i64]) {
        match self {
            Self::Postgres(f) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(&f.raw)
                    .await
                    .expect("owned PG fixture mutation");
            }
            Self::Sqlite(f) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(&f.raw)
                    .await
                    .expect("owned SQLite fixture mutation");
            }
        }
    }

    async fn count(&self, sql: &str, ids: &[i64]) -> i64 {
        match self {
            Self::Postgres(f) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query.fetch_one(&f.raw).await.expect("owned PG observation")
            }
            Self::Sqlite(f) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .fetch_one(&f.raw)
                    .await
                    .expect("owned SQLite observation")
            }
        }
    }

    async fn close(&mut self) {
        match self {
            Self::Postgres(f) => f.finish().await,
            Self::Sqlite(f) => f.close().await,
        }
    }

    async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(self.pool(), name, None)
            .await
            .unwrap()
            .id
    }

    async fn site_admin(&self, name: &str) -> i64 {
        let id = self.actor(name).await;
        self.execute("UPDATE users SET is_admin = TRUE WHERE id = $1", &[id])
            .await;
        id
    }

    /// A private repository owned by `owner`. Nothing here touches its files.
    async fn repo(&self, owner: i64, name: &str) -> i64 {
        db::repositories::records::create_repository(
            self.pool(),
            owner,
            name,
            None,
            true,
            "/fixture/not-executed.git",
        )
        .await
        .unwrap()
        .id
    }

    /// Queues a run through the real, permission-checked path. Every run gets
    /// its own commit, so commit statuses of different runs never meet.
    async fn enqueue(&self, repo: i64, actor: i64, deploy: bool, trusted: &[i64]) -> CiRun {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let sha = format!("{:040x}", NEXT.fetch_add(1, Ordering::Relaxed));
        let blobs = serde_json::json!({ ".github/workflows/ci.yml": sha });
        db::ci::enqueue_ci_run(
            self.pool(),
            db::ci::NewCiRun {
                repo_id: repo,
                triggered_by_id: actor,
                pipeline: if deploy { "full" } else { "ci" },
                event: "push",
                pull_request_id: None,
                branch: "main",
                head_sha: &sha,
                workflow_path: WORKFLOW,
                workflow_blobs: &blobs,
                workflow_inputs: None,
                environment: deploy.then_some("production"),
            },
            trusted,
        )
        .await
        .expect("the actor may queue this run")
    }

    /// A finished-looking run row written straight into the table, for tests of
    /// who may see it. Returns the row id.
    async fn history(&self, repo: i64, actor: i64) -> i64 {
        let sql = "INSERT INTO ci_runs(run_id,repo_id,triggered_by_id,mode,pipeline,event,branch,workflow_path,status) VALUES($1,$2,$3,'local','ci','push','main','.github/workflows/ci.yml','success') RETURNING id";
        match self {
            Self::Postgres(f) => sqlx::query_scalar(sql)
                .bind(Uuid::new_v4())
                .bind(repo)
                .bind(actor)
                .fetch_one(&f.raw)
                .await
                .unwrap(),
            Self::Sqlite(f) => sqlx::query_scalar(sql)
                .bind(Uuid::new_v4())
                .bind(repo)
                .bind(actor)
                .fetch_one(&f.raw)
                .await
                .unwrap(),
        }
    }

    async fn run(&self, run_id: Uuid) -> CiRun {
        db::ci::get_ci_run_by_run_id(self.pool(), run_id)
            .await
            .unwrap()
            .expect("the run exists")
    }

    /// Makes the run look abandoned: its worker last sent a heartbeat an hour ago.
    async fn stop_heartbeat(&self, id: i64) {
        let sql = match self {
            Self::Postgres(_) => {
                "UPDATE ci_runs SET heartbeat_at = NOW() - INTERVAL '1 hour' WHERE id = $1"
            }
            Self::Sqlite(_) => {
                "UPDATE ci_runs SET heartbeat_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour') WHERE id = $1"
            }
        };
        self.execute(sql, &[id]).await;
    }

    /// Gives a node a new public key, the way a re-keyed node looks in the table.
    async fn set_node_key(&self, node: i64, key: &str) {
        match self {
            Self::Postgres(f) => {
                sqlx::query("UPDATE nodes SET public_key = $1 WHERE id = $2")
                    .bind(key)
                    .bind(node)
                    .execute(&f.raw)
                    .await
                    .expect("owned PG fixture mutation");
            }
            Self::Sqlite(f) => {
                sqlx::query("UPDATE nodes SET public_key = ?1 WHERE id = ?2")
                    .bind(key)
                    .bind(node)
                    .execute(&f.raw)
                    .await
                    .expect("owned SQLite fixture mutation");
            }
        }
    }

    async fn notifications(&self, user: i64) -> i64 {
        self.count(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND kind = 'ci_completed'",
            &[user],
        )
        .await
    }
}

/// A site administrator who owns a private repository, and a collaborator with
/// write access who may also queue runs.
struct World {
    owner: i64,
    collab: i64,
    repo: i64,
}

async fn world(db: &Database, prefix: &str) -> World {
    let owner = db.site_admin(&format!("{prefix}_owner")).await;
    let collab = db.actor(&format!("{prefix}_collab")).await;
    let repo = db.repo(owner, &format!("{prefix}_repo")).await;
    db::repositories::access::grant_repo_access(db.pool(), collab, repo, AccessLevel::Write)
        .await
        .unwrap();
    World {
        owner,
        collab,
        repo,
    }
}

fn assert_cancelled(run: &CiRun, message: &str) {
    assert_eq!(run.status, "cancelled", "{run:?}");
    assert_eq!(run.error_message.as_deref(), Some(message), "{run:?}");
    assert!(run.completed_at.is_some(), "{run:?}");
}

/// Runs `call`, failing the test instead of hanging if it waits for something the
/// test is holding.
async fn promptly<T>(what: &str, call: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), call)
        .await
        .unwrap_or_else(|_| panic!("{what} waited for a lock it should not wait for"))
}

// ---- A claim checks the starter's access again ----

async fn claim_checks_the_starters_access_again(db: &Database) {
    let w = world(db, "claim").await;
    let trusted = [w.collab];
    let lost = db.enqueue(w.repo, w.collab, false, &trusted).await;
    let deploy = db.enqueue(w.repo, w.owner, true, &trusted).await;
    let later = db.enqueue(w.repo, w.collab, false, &trusted).await;
    // Remove the access without the revocation code that cancels runs. The claim must
    // recheck current authority on its own.
    db.execute(
        "DELETE FROM repo_access WHERE user_id = $1 AND repo_id = $2",
        &[w.collab, w.repo],
    )
    .await;

    // The oldest run belongs to the collaborator, who is gone: it is cancelled and the
    // claim moves on to the owner's deployment.
    let claimed = db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .expect("the owner's run may still start");
    assert_eq!(claimed.id, deploy.id);
    assert_eq!(claimed.status, "running");
    assert_eq!(claimed.worker.as_deref(), Some(WORKER));
    let deployment = db::ci::get_ci_deployment_by_run_id(db.pool(), deploy.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deployment.status, "in_progress");

    let lost = db.run(lost.run_id).await;
    assert_cancelled(&lost, ACCESS_LOST);
    assert!(lost.worker.is_none() && lost.started_at.is_none());
    let statuses = db::repositories::protection::list_commit_statuses(
        db.pool(),
        w.repo,
        lost.head_sha.as_deref().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(
        (
            statuses[0].state.as_str(),
            statuses[0].description.as_deref()
        ),
        ("failure", Some(ACCESS_LOST))
    );
    assert_eq!(db.notifications(w.collab).await, 1);

    // The run queued behind it is dealt with by the next claim.
    assert!(db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .is_none());
    assert_cancelled(&db.run(later.run_id).await, ACCESS_LOST);
    let untouched = db.run(deploy.run_id).await;
    assert_eq!(untouched.status, "running");
    assert!(!untouched.cancel_requested);

    // Other ways to lose the right to run workflows. Each is seen only by the claim.
    db::repositories::access::grant_repo_access(db.pool(), w.collab, w.repo, AccessLevel::Write)
        .await
        .unwrap();
    for (what, apply, undo, id) in [
        (
            "banned",
            "UPDATE users SET is_banned = TRUE WHERE id = $1",
            "UPDATE users SET is_banned = FALSE WHERE id = $1",
            w.collab,
        ),
        (
            "password change required",
            "UPDATE users SET must_change_password = TRUE WHERE id = $1",
            "UPDATE users SET must_change_password = FALSE WHERE id = $1",
            w.collab,
        ),
        (
            "repository archived",
            "UPDATE repositories SET is_archived = TRUE WHERE id = $1",
            "UPDATE repositories SET is_archived = FALSE WHERE id = $1",
            w.repo,
        ),
    ] {
        let run = db.enqueue(w.repo, w.collab, false, &trusted).await;
        db.execute(apply, &[id]).await;
        assert!(
            db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
                .await
                .unwrap()
                .is_none(),
            "{what}"
        );
        assert_cancelled(&db.run(run.run_id).await, ACCESS_LOST);
        db.execute(undo, &[id]).await;
    }

    // Being trusted to run workflows is part of the permission, and is read from the
    // configuration of the node that claims.
    let run = db.enqueue(w.repo, w.collab, false, &trusted).await;
    assert!(db::ci::claim_ci_run(db.pool(), WORKER, "local", &[])
        .await
        .unwrap()
        .is_none());
    assert_cancelled(&db.run(run.run_id).await, ACCESS_LOST);

    // A run whose starter's account no longer exists has nobody to run as.
    let run = db.enqueue(w.repo, w.collab, false, &trusted).await;
    db.execute(
        "UPDATE ci_runs SET triggered_by_id = NULL WHERE id = $1",
        &[run.id],
    )
    .await;
    assert!(db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .is_none());
    assert_cancelled(&db.run(run.run_id).await, ACCESS_LOST);

    // A queued run that was flagged for cancellation before runs were cancelled on
    // the spot is finished instead of waiting forever.
    let flagged = db.enqueue(w.repo, w.owner, false, &trusted).await;
    db.execute(
        "UPDATE ci_runs SET cancel_requested = TRUE WHERE id = $1",
        &[flagged.id],
    )
    .await;
    assert!(db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .is_none());
    assert_cancelled(&db.run(flagged.run_id).await, "Cancelled before it started");

    // A person who still has access gets the run.
    let fine = db.enqueue(w.repo, w.collab, false, &trusted).await;
    let claimed = db::ci::claim_ci_run(db.pool(), "worker-2", "local", &trusted)
        .await
        .unwrap()
        .expect("access is intact");
    assert_eq!(
        (claimed.id, claimed.worker.as_deref()),
        (fine.id, Some("worker-2"))
    );
}

async fn a_long_line_of_denied_runs_is_cancelled_in_batches(db: &Database) {
    let w = world(db, "batch").await;
    let trusted = [w.collab];
    let mut denied = Vec::new();
    for _ in 0..=db::ci::CLAIM_SCAN_LIMIT {
        denied.push(db.enqueue(w.repo, w.collab, false, &trusted).await);
    }
    let allowed = db.enqueue(w.repo, w.owner, false, &trusted).await;
    db.execute(
        "DELETE FROM repo_access WHERE user_id = $1 AND repo_id = $2",
        &[w.collab, w.repo],
    )
    .await;

    // One claim looks at a bounded number of runs, so it cancels that many and stops.
    assert!(db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .is_none());
    let mut cancelled = 0;
    for run in &denied {
        if db.run(run.run_id).await.status == "cancelled" {
            cancelled += 1;
        }
    }
    assert_eq!(cancelled, db::ci::CLAIM_SCAN_LIMIT);
    // The next one finishes the line and reaches the run that may start.
    let claimed = db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .expect("the owner's run is next");
    assert_eq!(claimed.id, allowed.id);
    assert_cancelled(&db.run(denied.last().unwrap().run_id).await, ACCESS_LOST);
}

// ---- Revocations stop queued and running runs ----

async fn revocations_stop_queued_and_running_runs(db: &Database) {
    let w = world(db, "stop").await;
    let trusted = [w.collab];
    let other = db.repo(w.owner, "stop_other").await;
    db::repositories::access::grant_repo_access(db.pool(), w.collab, other, AccessLevel::Write)
        .await
        .unwrap();

    for case in ["ban", "forced password change", "collaborator removal"] {
        // A run a worker has started, with more of the collaborator's runs and the
        // owner's own run waiting.
        let running = db.enqueue(w.repo, w.collab, false, &trusted).await;
        let started = db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
            .await
            .unwrap()
            .expect("claim");
        assert_eq!(started.id, running.id, "{case}");
        let queued = db.enqueue(w.repo, w.collab, false, &trusted).await;
        let elsewhere = db.enqueue(other, w.collab, false, &trusted).await;
        let theirs = db.enqueue(w.repo, w.owner, false, &trusted).await;

        match case {
            "ban" => {
                let change = db::users::transitions::change_ban(db.pool(), w.owner, w.collab, true)
                    .await
                    .unwrap();
                assert!(matches!(
                    change,
                    db::users::transitions::BanChange::Updated(_)
                ));
            }
            "forced password change" => {
                db::users::accounts::update_user_password(
                    db.pool(),
                    w.collab,
                    "temporary-hash",
                    true,
                )
                .await
                .unwrap();
            }
            _ => assert!(
                db::repositories::access::revoke_repo_access(db.pool(), w.collab, w.repo)
                    .await
                    .unwrap()
            ),
        }

        // Queued work is cancelled on the spot, for the person and the notification.
        assert_cancelled(&db.run(queued.run_id).await, ACCESS_LOST);
        // The run a worker holds is asked to stop; nothing is finished underneath it.
        assert_eq!(
            db::ci::heartbeat_ci_run(db.pool(), running.id, WORKER)
                .await
                .unwrap(),
            Some(true),
            "{case}"
        );
        assert_eq!(db.run(running.run_id).await.status, "running", "{case}");
        // Losing the account reaches every repository; losing one repository does not.
        let expected = if case == "collaborator removal" {
            "queued"
        } else {
            "cancelled"
        };
        assert_eq!(db.run(elsewhere.run_id).await.status, expected, "{case}");
        // What other people queued is theirs.
        let theirs_now = db.run(theirs.run_id).await;
        assert_eq!(theirs_now.status, "queued", "{case}");
        assert!(!theirs_now.cancel_requested, "{case}");

        // Put the world back and empty the queue for the next case.
        match case {
            "ban" => {
                db::users::transitions::change_ban(db.pool(), w.owner, w.collab, false)
                    .await
                    .unwrap();
            }
            "forced password change" => {
                db.execute(
                    "UPDATE users SET must_change_password = FALSE WHERE id = $1",
                    &[w.collab],
                )
                .await;
            }
            _ => {
                db::repositories::access::grant_repo_access(
                    db.pool(),
                    w.collab,
                    w.repo,
                    AccessLevel::Write,
                )
                .await
                .unwrap();
            }
        }
        db::ci::finish_ci_run(db.pool(), running.id, WORKER, "failed", Some("stopped"))
            .await
            .unwrap();
        for run in [&elsewhere, &theirs] {
            if db.run(run.run_id).await.status == "queued" {
                assert!(
                    db::ci::request_ci_cancellation(db.pool(), run.run_id, w.owner)
                        .await
                        .unwrap()
                        .0
                );
            }
        }
    }

    // Lifting a ban, and removing a grant that is not there, change nothing.
    let waiting = db.enqueue(w.repo, w.owner, false, &trusted).await;
    assert!(
        !db::repositories::access::revoke_repo_access(db.pool(), w.collab, other + 1000)
            .await
            .unwrap()
    );
    db::users::transitions::change_ban(db.pool(), w.owner, w.collab, false)
        .await
        .unwrap();
    assert_eq!(db.run(waiting.run_id).await.status, "queued");
}

// ---- Losing organization governance stops all tenant work ----

struct OrganizationWork {
    id: i64,
    repo: i64,
}

async fn organization_repository(db: &Database, owner: i64, prefix: &str) -> OrganizationWork {
    let organization = applied(
        orgs::create_organization(
            db.pool(),
            owner,
            &NewOrganization::new(
                &format!("{prefix}_org"),
                OrganizationProfile::new(Some("Fixture"), None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let repo = db.repo(owner, &format!("{prefix}_repo")).await;
    db.execute(
        "UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2",
        &[organization.id, repo],
    )
    .await;
    OrganizationWork {
        id: organization.id,
        repo,
    }
}

async fn deployment_status(db: &Database, run: &CiRun) -> String {
    db::ci::get_ci_deployment_by_run_id(db.pool(), run.id)
        .await
        .unwrap()
        .expect("deployment row")
        .status
}

async fn last_owner_restriction_stops_organization_work(db: &Database, forced_password: bool) {
    let kind = if forced_password { "forced" } else { "banned" };
    let site = db.site_admin(&format!("{kind}_site")).await;
    let owner = db.actor(&format!("{kind}_owner")).await;
    let runner = db.actor(&format!("{kind}_runner")).await;
    let trusted = [runner];

    let affected = organization_repository(db, owner, &format!("{kind}_affected")).await;
    db::repositories::access::grant_repo_access(
        db.pool(),
        runner,
        affected.repo,
        AccessLevel::Admin,
    )
    .await
    .unwrap();

    let running = db.enqueue(affected.repo, runner, false, &trusted).await;
    assert_eq!(
        db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
            .await
            .unwrap()
            .unwrap()
            .id,
        running.id
    );
    let running_deploy = db.enqueue(affected.repo, runner, true, &trusted).await;
    assert_eq!(
        db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
            .await
            .unwrap()
            .unwrap()
            .id,
        running_deploy.id
    );
    let queued = db.enqueue(affected.repo, runner, false, &trusted).await;
    let queued_deploy = db.enqueue(affected.repo, runner, true, &trusted).await;

    // This organization retains usable governance. The target's own run still
    // follows account-wide cancellation, while another actor's run does not.
    let survivor = db.actor(&format!("{kind}_survivor")).await;
    let retained = organization_repository(db, owner, &format!("{kind}_retained")).await;
    applied(
        orgs::add_organization_member(db.pool(), owner, retained.id, survivor, OrgRole::Owner)
            .await
            .unwrap(),
    );
    db::repositories::access::grant_repo_access(
        db.pool(),
        runner,
        retained.repo,
        AccessLevel::Admin,
    )
    .await
    .unwrap();
    let retained_runner = db.enqueue(retained.repo, runner, true, &trusted).await;
    let retained_target = db.enqueue(retained.repo, owner, false, &[owner]).await;

    // Work outside the affected tenant is not swept up by governance loss.
    let unrelated_owner = db.actor(&format!("{kind}_unrelated_owner")).await;
    let unrelated =
        organization_repository(db, unrelated_owner, &format!("{kind}_unrelated")).await;
    db::repositories::access::grant_repo_access(
        db.pool(),
        runner,
        unrelated.repo,
        AccessLevel::Admin,
    )
    .await
    .unwrap();
    let unrelated_run = db.enqueue(unrelated.repo, runner, true, &trusted).await;
    let personal_repo = db.repo(runner, &format!("{kind}_personal")).await;
    let personal_run = db.enqueue(personal_repo, runner, false, &trusted).await;
    let target_repo = db.repo(owner, &format!("{kind}_target_personal")).await;
    let target_run = db.enqueue(target_repo, owner, false, &[owner]).await;

    if forced_password {
        db::users::accounts::update_user_password(db.pool(), owner, "temporary-hash", true)
            .await
            .unwrap();
    } else {
        assert!(matches!(
            db::users::transitions::change_ban(db.pool(), site, owner, true)
                .await
                .unwrap(),
            db::users::transitions::BanChange::Updated(user) if user.is_banned
        ));
    }

    assert_cancelled(&db.run(queued.run_id).await, ACCESS_LOST);
    assert_cancelled(&db.run(queued_deploy.run_id).await, ACCESS_LOST);
    assert_eq!(deployment_status(db, &queued_deploy).await, "cancelled");

    for run in [&running, &running_deploy] {
        let current = db.run(run.run_id).await;
        assert_eq!(current.status, "running");
        assert!(current.cancel_requested);
        assert_eq!(
            db::ci::heartbeat_ci_run(db.pool(), run.id, WORKER)
                .await
                .unwrap(),
            Some(true)
        );
    }
    assert_eq!(deployment_status(db, &running_deploy).await, "in_progress");
    db::ci::finish_ci_run(
        db.pool(),
        running_deploy.id,
        WORKER,
        "cancelled",
        Some(ACCESS_LOST),
    )
    .await
    .unwrap()
    .expect("the deployment worker owns its run");
    assert_eq!(deployment_status(db, &running_deploy).await, "cancelled");

    for run in [&retained_runner, &unrelated_run, &personal_run] {
        let current = db.run(run.run_id).await;
        assert_eq!(current.status, "queued", "{kind}: {current:?}");
        assert!(!current.cancel_requested, "{kind}: {current:?}");
    }
    assert_eq!(deployment_status(db, &retained_runner).await, "pending");
    assert_eq!(deployment_status(db, &unrelated_run).await, "pending");
    assert_cancelled(&db.run(retained_target.run_id).await, ACCESS_LOST);
    assert_cancelled(&db.run(target_run.run_id).await, ACCESS_LOST);
}

async fn banning_the_last_owner_stops_organization_work(db: &Database) {
    last_owner_restriction_stops_organization_work(db, false).await;
}

async fn forcing_the_last_owner_password_stops_organization_work(db: &Database) {
    last_owner_restriction_stops_organization_work(db, true).await;
}

async fn ordinary_password_changes_leave_organization_work_running(db: &Database) {
    let owner = db.actor("password_owner").await;
    let runner = db.actor("password_runner").await;
    let organization = organization_repository(db, owner, "password_ordinary").await;
    db::repositories::access::grant_repo_access(
        db.pool(),
        runner,
        organization.repo,
        AccessLevel::Admin,
    )
    .await
    .unwrap();
    let first = db.enqueue(organization.repo, runner, true, &[runner]).await;

    db::users::accounts::update_user_password(db.pool(), owner, "ordinary-one", false)
        .await
        .unwrap();
    assert_eq!(db.run(first.run_id).await.status, "queued");

    let second = db
        .enqueue(organization.repo, runner, false, &[runner])
        .await;
    assert!(db::users::transitions::compare_exchange_password(
        db.pool(),
        owner,
        "ordinary-one",
        "ordinary-two",
        true,
        true,
    )
    .await
    .unwrap());
    for run in [&first, &second] {
        let current = db.run(run.run_id).await;
        assert_eq!(current.status, "queued");
        assert!(!current.cancel_requested);
    }
    assert_eq!(deployment_status(db, &first).await, "pending");
}

// ---- The ways a run can end do not overwrite each other ----

async fn finish_sweep_and_cancel_do_not_overwrite_each_other(db: &Database) {
    let w = world(db, "ends").await;
    let trusted = [w.collab];
    let notified = || db.notifications(w.owner);

    // Finished by its worker, then found by a sweep: the first outcome stands.
    let run = db.enqueue(w.repo, w.owner, true, &trusted).await;
    db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .expect("claim");
    db.stop_heartbeat(run.id).await;
    let (finished, _) = db::ci::finish_ci_run(db.pool(), run.id, WORKER, "success", None)
        .await
        .unwrap()
        .expect("the worker owns the run");
    assert_eq!(finished.status, "success");
    assert_eq!(db::ci::fail_stale_ci_runs(db.pool(), 90).await.unwrap(), 0);
    let after = db.run(run.run_id).await;
    assert_eq!(
        (after.status.as_str(), after.error_message.as_deref()),
        ("success", None)
    );
    let deployment = db::ci::get_ci_deployment_by_run_id(db.pool(), run.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deployment.status, "success");
    let statuses = db::repositories::protection::list_commit_statuses(
        db.pool(),
        w.repo,
        run.head_sha.as_deref().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(statuses[0].state, "success");
    assert_eq!(notified().await, 1);

    // Found by a sweep, then finished by the worker that came back: still the sweep's.
    let run = db.enqueue(w.repo, w.owner, true, &trusted).await;
    db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .expect("claim");
    db.stop_heartbeat(run.id).await;
    assert_eq!(db::ci::fail_stale_ci_runs(db.pool(), 90).await.unwrap(), 1);
    assert!(
        db::ci::finish_ci_run(db.pool(), run.id, WORKER, "success", None)
            .await
            .unwrap()
            .is_none()
    );
    let after = db.run(run.run_id).await;
    assert_eq!(after.status, "failed");
    assert_eq!(
        after.error_message.as_deref(),
        Some("The CI worker stopped before the run finished")
    );
    let deployment = db::ci::get_ci_deployment_by_run_id(db.pool(), run.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deployment.status, "failed");
    let statuses = db::repositories::protection::list_commit_statuses(
        db.pool(),
        w.repo,
        run.head_sha.as_deref().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(statuses[0].state, "failure");
    assert_eq!(notified().await, 2, "one outcome, one notification");
    assert_eq!(db::ci::fail_stale_ci_runs(db.pool(), 90).await.unwrap(), 0);

    // Cancelling something that already ended does nothing.
    assert!(
        !db::ci::request_ci_cancellation(db.pool(), run.run_id, w.owner)
            .await
            .unwrap()
            .0
    );
    assert_eq!(db.run(run.run_id).await.status, "failed");

    // Cancelled while queued: no worker brings it back.
    let queued = db.enqueue(w.repo, w.owner, false, &trusted).await;
    assert!(
        db::ci::request_ci_cancellation(db.pool(), queued.run_id, w.owner)
            .await
            .unwrap()
            .0
    );
    assert!(db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .is_none());
    assert_cancelled(&db.run(queued.run_id).await, "Cancelled before it started");
    assert!(
        !db::ci::request_ci_cancellation(db.pool(), queued.run_id, w.owner)
            .await
            .unwrap()
            .0
    );

    // Cancelled while running: the worker is asked, ends it as cancelled, and a later
    // sweep leaves it alone.
    let running = db.enqueue(w.repo, w.owner, false, &trusted).await;
    db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
        .await
        .unwrap()
        .expect("claim");
    assert!(
        db::ci::request_ci_cancellation(db.pool(), running.run_id, w.owner)
            .await
            .unwrap()
            .0
    );
    assert_eq!(db.run(running.run_id).await.status, "running");
    assert_eq!(
        db::ci::heartbeat_ci_run(db.pool(), running.id, WORKER)
            .await
            .unwrap(),
        Some(true)
    );
    db::ci::finish_ci_run(
        db.pool(),
        running.id,
        WORKER,
        "cancelled",
        Some("Cancelled"),
    )
    .await
    .unwrap()
    .expect("the worker owns the run");
    db.stop_heartbeat(running.id).await;
    assert_eq!(db::ci::fail_stale_ci_runs(db.pool(), 90).await.unwrap(), 0);
    assert_eq!(db.run(running.run_id).await.status, "cancelled");
}

// ---- Workflow statuses and live completion notifications ----

async fn workflows_keep_separate_statuses_and_newer_attempts_win(db: &Database) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use codeyard::backend::{
        api::create_router, app::AppState, ci::CiConfig, storage::ObjectStore,
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    let w = world(db, "statuses").await;
    db.execute(
        "UPDATE repositories SET is_private = FALSE WHERE id = $1",
        &[w.repo],
    )
    .await;
    db.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let root = tempfile::tempdir().unwrap();
    let app = create_router(AppState::new(
        db.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));

    async fn enqueue(db: &Database, w: &World, sha: &str, workflow: &str) -> CiRun {
        db::ci::enqueue_ci_run(
            db.pool(),
            db::ci::NewCiRun {
                repo_id: w.repo,
                triggered_by_id: w.owner,
                pipeline: "full",
                event: "push",
                pull_request_id: None,
                branch: "main",
                head_sha: sha,
                workflow_path: workflow,
                workflow_blobs: &serde_json::json!({workflow: sha}),
                workflow_inputs: None,
                environment: None,
            },
            &[],
        )
        .await
        .unwrap()
    }

    // The late success must not hide a failure, and neither may a late failure
    // erase the other workflow's success. Exercise the public combined-status API.
    for (index, order) in [["failed", "success"], ["success", "failed"]]
        .into_iter()
        .enumerate()
    {
        let sha = format!("{:040x}", 1000 + index);
        let failed = enqueue(db, &w, &sha, ".github/workflows/test.yml").await;
        let passed = enqueue(db, &w, &sha, ".github/workflows/lint.yaml").await;
        for _ in 0..2 {
            db::ci::claim_ci_run(db.pool(), WORKER, "local", &[])
                .await
                .unwrap()
                .unwrap();
        }
        for status in order {
            let id = if status == "failed" {
                failed.id
            } else {
                passed.id
            };
            db::ci::finish_ci_run(db.pool(), id, WORKER, status, None)
                .await
                .unwrap()
                .unwrap();
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/repos/statuses_owner/statuses_repo/commits/{sha}/status"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["state"], "failure");
        assert_eq!(body["total_count"], 2);
        assert_eq!(
            body["statuses"][0]["context"],
            "gitserv/workflow/.github/workflows/lint.yaml"
        );
        assert_eq!(body["statuses"][0]["state"], "success");
        assert_eq!(
            body["statuses"][1]["context"],
            "gitserv/workflow/.github/workflows/test.yml"
        );
        assert_eq!(body["statuses"][1]["state"], "failure");
    }

    let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let older = enqueue(db, &w, sha, ".github/workflows/test.yml").await;
    db::ci::claim_ci_run(db.pool(), WORKER, "local", &[])
        .await
        .unwrap()
        .unwrap();
    let newer = enqueue(db, &w, sha, ".github/workflows/test.yml").await;
    // Even an older finish cannot replace the newer pending status.
    db::ci::finish_ci_run(db.pool(), older.id, WORKER, "success", None)
        .await
        .unwrap()
        .unwrap();
    let statuses = db::repositories::protection::list_commit_statuses(db.pool(), w.repo, sha)
        .await
        .unwrap();
    assert_eq!(statuses[0].state, "pending");
    assert_eq!(db.count("SELECT COUNT(*) FROM commit_statuses WHERE ci_run_id = $1 AND context = 'gitserv/workflow/.github/workflows/test.yml'", &[newer.id]).await, 1);
    db::ci::claim_ci_run(db.pool(), WORKER, "local", &[])
        .await
        .unwrap()
        .unwrap();
    db::ci::finish_ci_run(db.pool(), newer.id, WORKER, "failed", None)
        .await
        .unwrap()
        .unwrap();
    let statuses = db::repositories::protection::list_commit_statuses(db.pool(), w.repo, sha)
        .await
        .unwrap();
    assert_eq!(statuses[0].state, "failure");
    assert_eq!(db.count("SELECT COUNT(*) FROM commit_statuses WHERE ci_run_id = $1 AND context = 'gitserv/workflow/.github/workflows/test.yml'", &[newer.id]).await, 1);

    let multi = enqueue(
        db,
        &w,
        sha,
        ".github/workflows/test.yml,.github/workflows/lint.yaml",
    )
    .await;
    assert_eq!(multi.commit_status_context(), "gitserv/pipeline/full");
    let statuses = db::repositories::protection::list_commit_statuses(db.pool(), w.repo, sha)
        .await
        .unwrap();
    assert!(statuses
        .iter()
        .any(|s| s.context == "gitserv/pipeline/full" && s.state == "pending"));
    db::ci::request_ci_cancellation(db.pool(), multi.run_id, w.owner)
        .await
        .unwrap();
}

async fn finishing_and_cancelling_broadcast_only_the_created_notification(db: &Database) {
    use codeyard::backend::{
        ci::{queue::RunQueue, CiConfig, CiRuns},
        notifications::{current_event, NotificationBroadcaster, NotificationEvent},
    };
    use std::sync::Arc;
    let w = world(db, "live").await;
    let broadcaster = Arc::new(NotificationBroadcaster::new(db.pool()));
    let mut subscriber = broadcaster.subscribe();
    let config = Arc::new(CiConfig::default());
    let service = CiRuns::new(
        db.pool().clone(),
        config.clone(),
        Arc::new(tokio::sync::Notify::new()),
        None,
        broadcaster.clone(),
    );
    let queue = RunQueue::Database {
        pool: db.pool().clone(),
        config,
        secrets: None,
        context: None,
        notifications: broadcaster,
    };
    let session = Uuid::new_v4().simple().to_string().repeat(2);
    db::users::sessions::create_session(
        db.pool(),
        &session,
        w.owner,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();

    for cancel in [false, true] {
        let run = db.enqueue(w.repo, w.owner, false, &[]).await;
        if cancel {
            assert!(service.cancel_run(run.run_id, w.owner).await.unwrap());
        } else {
            db::ci::claim_ci_run(db.pool(), WORKER, "local", &[])
                .await
                .unwrap()
                .unwrap();
            assert!(!queue
                .finish("foreign-worker", run.id, "success", None)
                .await
                .unwrap());
            assert!(
                subscriber.try_recv().is_err(),
                "an unowned finish sends nothing"
            );
            assert!(queue.finish(WORKER, run.id, "success", None).await.unwrap());
        }
        let event = promptly("the live CI notification", subscriber.recv())
            .await
            .unwrap();
        let NotificationEvent::Created {
            user_id,
            notification_id,
            ..
        } = &event
        else {
            panic!("expected a created notification: {event:?}");
        };
        assert_eq!(*user_id, w.owner);
        // Projection is the same recipient/session filter connected browsers use.
        let projected = current_event(db.pool(), &session, w.owner, &event)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(projected, NotificationEvent::Created { notification_id: id, kind, title, .. }
            if id == *notification_id && kind == "ci_completed"
                && title == if cancel { "CI run cancelled" } else { "CI run success" })
        );
        assert!(current_event(db.pool(), &session, w.collab, &event)
            .await
            .unwrap()
            .is_none());
        assert!(!service.cancel_run(run.run_id, w.owner).await.unwrap());
        assert!(!queue.finish(WORKER, run.id, "success", None).await.unwrap());
        assert!(
            subscriber.try_recv().is_err(),
            "one completed run sends one event"
        );
    }
    assert_eq!(db.notifications(w.owner).await, 2);
    assert_eq!(db.notifications(w.collab).await, 0);
}

/// The collaborator's own run is queued, claimed and finished by the worker.
async fn finish_collaborator_run(db: &Database, w: &World) {
    let run = db.enqueue(w.repo, w.collab, false, &[w.collab]).await;
    let claimed = db::ci::claim_ci_run(db.pool(), WORKER, "local", &[w.collab])
        .await
        .unwrap()
        .expect("claim");
    assert_eq!(claimed.id, run.id);
    db::ci::finish_ci_run(db.pool(), claimed.id, WORKER, "success", None)
        .await
        .unwrap()
        .expect("finish");
}

/// The run result reaches its author, unless the author ignores the repository, as it does
/// every other notice from the repository.
async fn ignoring_the_repository_silences_run_results(db: &Database) {
    let w = world(db, "quiet").await;
    finish_collaborator_run(db, &w).await;
    assert_eq!(db.notifications(w.collab).await, 1);
    db::repositories::engagement::set_watch_level(db.pool(), w.collab, w.repo, "ignoring", "")
        .await
        .unwrap();
    finish_collaborator_run(db, &w).await;
    assert_eq!(
        db.notifications(w.collab).await,
        1,
        "an ignoring author hears nothing about the run"
    );
}

// ---- The global run list follows the repository read rule ----

fn applied<T: std::fmt::Debug>(outcome: OrganizationOutcome<T>) -> T {
    match outcome {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("expected the organization change to apply: {other:?}"),
    }
}

async fn visible_repositories(db: &Database, user: i64, limit: i64) -> Vec<i64> {
    db::ci::list_ci_runs_for_user(db.pool(), user, limit)
        .await
        .unwrap()
        .into_iter()
        .map(|run| run.repo_id)
        .collect()
}

async fn the_global_run_list_follows_the_repository_read_rule(db: &Database) {
    let site = db.site_admin("list_site").await;
    let owner = db.actor("list_owner").await;
    let member = db.actor("list_member").await;
    let org = applied(
        orgs::create_organization(
            db.pool(),
            owner,
            &NewOrganization::new(
                "list_org",
                OrganizationProfile::new(Some("Fixture"), Some("Safe description")).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        orgs::add_organization_member(db.pool(), owner, org.id, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    applied(
        orgs::add_organization_member(db.pool(), owner, org.id, site, OrgRole::Member)
            .await
            .unwrap(),
    );
    let team = applied(
        orgs::create_organization_team(
            db.pool(),
            owner,
            org.id,
            &NewTeam::new(
                "list_team",
                TeamProfile::new("Fixture team", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    applied(
        orgs::add_organization_team_member(
            db.pool(),
            owner,
            org.id,
            team.id,
            member,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    let org_repo = db.repo(owner, "list_org_repo").await;
    db.execute(
        "UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2",
        &[org.id, org_repo],
    )
    .await;
    applied(
        orgs::grant_organization_team_repository(
            db.pool(),
            owner,
            org.id,
            team.id,
            org_repo,
            TeamRepositoryPermission::new(3).unwrap(),
        )
        .await
        .unwrap(),
    );
    let personal = db.repo(owner, "list_personal").await;
    let direct = db.repo(owner, "list_direct").await;
    db::repositories::access::grant_repo_access(db.pool(), member, direct, AccessLevel::Read)
        .await
        .unwrap();
    let public = db.repo(owner, "list_public").await;
    db.execute(
        "UPDATE repositories SET is_private = FALSE WHERE id = $1",
        &[public],
    )
    .await;

    // The member and the site administrator each started a run in every repository.
    for repo in [org_repo, personal, direct, public] {
        db.history(repo, member).await;
        db.history(repo, site).await;
    }

    // A team writer sees the organization's runs through the team grant. They do not
    // see a private repository of someone else, and a read grant or a public
    // repository is enough for the rest.
    let mut seen = visible_repositories(db, member, 50).await;
    seen.sort();
    assert_eq!(seen, [org_repo, direct, public]);

    // A site administrator reads every personal repository, but gets no special reach
    // into an organization: a plain member without a team grant sees none of its runs.
    let mut seen = visible_repositories(db, site, 50).await;
    seen.sort();
    assert_eq!(seen, [personal, direct, public]);

    // With a team grant they see them, and once removed from the organization they
    // lose them again, administrator or not.
    applied(
        orgs::add_organization_team_member(
            db.pool(),
            owner,
            org.id,
            team.id,
            site,
            TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    let mut seen = visible_repositories(db, site, 50).await;
    seen.sort();
    assert_eq!(seen, [org_repo, personal, direct, public]);
    applied(
        orgs::remove_organization_member(db.pool(), owner, org.id, site)
            .await
            .unwrap(),
    );
    let mut seen = visible_repositories(db, site, 50).await;
    seen.sort();
    assert_eq!(seen, [personal, direct, public]);

    // Runs are hidden before the limit, so they do not use up the page: the newest
    // runs here are in the organization, which the administrator can no longer read.
    for _ in 0..3 {
        db.history(org_repo, site).await;
    }
    assert_eq!(visible_repositories(db, site, 2).await, [public, direct]);

    // Access taken away takes the runs out of the list.
    db.execute(
        "DELETE FROM team_memberships WHERE team_id = $1 AND user_id = $2",
        &[team.id, member],
    )
    .await;
    assert!(
        db::repositories::access::revoke_repo_access(db.pool(), member, direct)
            .await
            .unwrap()
    );
    assert_eq!(visible_repositories(db, member, 50).await, [public]);

    // An account that cannot use the site sees nothing.
    db::users::transitions::change_ban(db.pool(), site, member, true)
        .await
        .unwrap();
    assert!(visible_repositories(db, member, 50).await.is_empty());
}

async fn global_run_list_applies_visibility_before_the_limit(db: &Database) {
    let viewer = db.actor("visibility_viewer").await;
    let other = db.actor("visibility_other").await;
    // The viewer's only readable run is the oldest. Thirty-two newer runs sit in
    // repositories the viewer cannot read, so a limit applied first would hide it.
    let visible = db.repo(viewer, "visible").await;
    db.history(visible, viewer).await;
    for index in 0..32 {
        let hidden = db.repo(other, &format!("hidden_{index}")).await;
        db.history(hidden, viewer).await;
    }
    let runs = db::ci::list_ci_runs_for_user(db.pool(), viewer, 1)
        .await
        .unwrap();
    assert_eq!(
        runs.iter().map(|run| run.repo_id).collect::<Vec<_>>(),
        [visible]
    );
    let runs = db::ci::list_ci_runs_for_user(db.pool(), viewer, 100)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
}

async fn actions_models_bound_current_logs_and_keep_refreshing_status(db: &Database) {
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
        Router,
    };
    use codeyard::{
        backend::{
            api::create_router,
            app::AppState,
            ci::{queue::LogLine, CiConfig},
            storage::ObjectStore,
        },
        webui::{
            app::PageEnvelope,
            bootstrap::{MAX_PAGE_MODEL_BYTES, MODEL_CONTENT_TYPE},
            pages::ci,
        },
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn get(router: &Router, session: &str, path: &str) -> Vec<u8> {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::COOKIE, format!("session_id={session}"))
                    .header(header::ACCEPT, MODEL_CONTENT_TYPE)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_PAGE_MODEL_BYTES)
            .await
            .expect("Actions responses must fit interactive navigation");
        body.to_vec()
    }

    let w = world(db, "log_page").await;
    db::node_config::upsert_node_configuration(db.pool(), "public", true)
        .await
        .unwrap();
    let session = native::token();
    db::users::sessions::create_session(
        db.pool(),
        &session,
        w.owner,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    // Both the scoped Actions page and the global page load the same selected run.
    let path = root.path().join("repo.git");
    git2::Repository::init_bare(&path).unwrap();
    let repo = db::repositories::records::create_repository(
        db.pool(),
        w.owner,
        "logs",
        None,
        true,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let router = create_router(AppState::new(
        db.pool().clone(),
        root.path().join("repos"),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));

    // Ordinary current lines exceed the page limit. Each quote doubles in the JSON
    // page, so the second run exceeds it at a much smaller raw size. Both runs stay
    // below ingestion's 32 MiB cap.
    for (character, count) in [('x', 1100), ('"', 600)] {
        let run = db.enqueue(repo.id, w.owner, false, &[]).await;
        assert_eq!(
            db::ci::claim_ci_run(db.pool(), WORKER, "local", &[])
                .await
                .unwrap()
                .unwrap()
                .id,
            run.id
        );
        let lines: Vec<_> = (0..count)
            .map(|index| LogLine {
                stage: "ci".into(),
                line: format!("{index:04} {}", character.to_string().repeat(15_995)),
                level: "info".into(),
            })
            .collect();
        for batch in lines.chunks(16) {
            db::ci::append_ci_logs(db.pool(), run.id, WORKER, batch)
                .await
                .unwrap();
        }
        let stored = db::ci::list_ci_logs(db.pool(), run.id, 2000).await.unwrap();
        assert_eq!(stored.len(), count);
        for (base, status) in [
            ("/ci", "running"),
            ("/log_page_owner/logs/actions", "success"),
        ] {
            if status == "success" {
                assert!(
                    db::ci::finish_ci_run(db.pool(), run.id, WORKER, status, None)
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            let bytes = get(
                &router,
                &session,
                &format!("{base}?job={}&log_q=%22%5C&view=logs", run.run_id),
            )
            .await;
            let envelope: PageEnvelope = serde_json::from_slice(&bytes).unwrap();
            let codeyard::webui::app::Page::Actions(ci::Page::Actions(model)) = envelope.page
            else {
                panic!("expected Actions model");
            };
            let selected = model.selected.unwrap();
            assert_eq!(selected.run.status, status);
            assert!(selected.logs_truncated);
            assert_eq!(selected.fetched_lines, count);
            assert!(!selected.logs.is_empty());
            assert!(selected.logs.len() < count);
            let suffix = &stored[count - selected.logs.len()..];
            for (actual, expected) in selected.logs.iter().zip(suffix) {
                assert_eq!(actual.id, expected.id);
                assert_eq!(actual.line, expected.line);
            }
            // The next complete line, not an arbitrary row count, fills the envelope.
            assert!(
                MAX_PAGE_MODEL_BYTES - bytes.len()
                    < serde_json::to_vec(&selected.logs[0]).unwrap().len() + 2
            );
        }
        let mut cursor = 0;
        let mut seen = Vec::new();
        loop {
            let bytes = get(
                &router,
                &session,
                &format!("/api/ci/jobs/{}/history?after={cursor}", run.run_id),
            )
            .await;
            let history: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            seen.extend(
                history["logs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|line| line["id"].as_i64().unwrap()),
            );
            let next = history["next_cursor"].as_i64().unwrap();
            assert!(next > cursor);
            cursor = next;
            if history["has_more"] == false {
                break;
            }
        }
        assert_eq!(seen, stored.iter().map(|line| line.id).collect::<Vec<_>>());
    }
}

async fn pull_request_runs_use_exact_approval_identity(db: &Database) {
    let w = world(db, "pr_identity").await;
    let fork_repo = db::repositories::discovery::create_forked_repository(
        db.pool(),
        db::repositories::discovery::NewForkedRepository {
            owner_id: w.owner,
            name: "pr_identity_fork",
            description: None,
            is_private: true,
            disk_path: "/fixture/not-executed-fork.git",
            forked_from: w.repo,
            default_branch: "main",
        },
    )
    .await
    .unwrap()
    .id;
    let sha = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
    let same_repo = db::pull_requests::records::create_pull_request(
        db.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: w.repo,
            head_repo_id: Some(w.repo),
            author_id: w.owner,
            title: "Same repository",
            body: "",
            head_branch: "topic",
            head_sha: sha,
            base_branch: "main",
            base_sha: "1111111111111111111111111111111111111111",
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let fork = db::pull_requests::records::create_pull_request(
        db.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: w.repo,
            head_repo_id: Some(fork_repo),
            author_id: w.owner,
            title: "Fork",
            body: "",
            head_branch: "topic",
            head_sha: sha,
            base_branch: "main",
            base_sha: "1111111111111111111111111111111111111111",
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let blobs = serde_json::json!({WORKFLOW: sha});
    let input = |pull_request_id| db::ci::NewCiRun {
        repo_id: w.repo,
        triggered_by_id: w.owner,
        pipeline: "ci",
        event: "pull_request",
        pull_request_id,
        branch: "main",
        head_sha: sha,
        workflow_path: WORKFLOW,
        workflow_blobs: &blobs,
        workflow_inputs: None,
        environment: None,
    };

    let denied = db::ci::enqueue_ci_run(db.pool(), input(Some(fork.id)), &[])
        .await
        .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<db::ci::CiAdmissionRefusal>(),
        Some(db::ci::CiAdmissionRefusal::Denied(
            "Fork workflows require approval for this head commit"
        ))
    ));

    let same_run = db::ci::enqueue_ci_run(db.pool(), input(Some(same_repo.id)), &[])
        .await
        .unwrap();
    assert_eq!(same_run.pull_request_id, Some(same_repo.id));
    assert!(!same_run.is_fork);
    assert_eq!(
        db::ci::enqueue_ci_run(db.pool(), input(Some(same_repo.id)), &[])
            .await
            .unwrap()
            .id,
        same_run.id
    );

    assert!(db::pull_requests::records::approve_pull_request_workflows(
        db.pool(),
        fork.id,
        w.owner,
        sha,
    )
    .await
    .unwrap());
    let fork_run = db::ci::enqueue_ci_run(db.pool(), input(Some(fork.id)), &[])
        .await
        .unwrap();
    assert_eq!(fork_run.pull_request_id, Some(fork.id));
    assert!(fork_run.is_fork);
    assert_ne!(fork_run.id, same_run.id);

    let ambiguous = db::ci::enqueue_ci_run(db.pool(), input(None), &[])
        .await
        .unwrap_err();
    assert!(matches!(
        ambiguous.downcast_ref::<db::ci::CiAdmissionRefusal>(),
        Some(db::ci::CiAdmissionRefusal::Conflict(
            "CI pull request revision is not open"
        ))
    ));
}

async fn queued_pull_request_inputs_survive_base_and_title_changes(db: &Database) {
    let w = world(db, "pr_snapshot").await;
    let pull = db::pull_requests::records::create_pull_request(
        db.pool(),
        db::pull_requests::records::NewPullRequest {
            repo_id: w.repo,
            head_repo_id: Some(w.repo),
            author_id: w.owner,
            title: "Original title",
            body: "",
            head_branch: "topic",
            head_sha: &"a".repeat(40),
            base_branch: "main",
            base_sha: &"1".repeat(40),
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let blobs = serde_json::json!({WORKFLOW: "b".repeat(40)});
    let input = || db::ci::NewCiRun {
        repo_id: w.repo,
        triggered_by_id: w.owner,
        pipeline: "ci",
        event: "pull_request",
        pull_request_id: Some(pull.id),
        branch: "main",
        head_sha: &pull.head_sha,
        workflow_path: WORKFLOW,
        workflow_blobs: &blobs,
        workflow_inputs: None,
        environment: None,
    };
    let queued = db::ci::enqueue_ci_run(db.pool(), input(), &[])
        .await
        .unwrap();
    assert_eq!(queued.mode, "unassigned");
    assert!(db::pull_requests::records::move_open_pull_request_base(
        db.pool(),
        pull.id,
        &pull.base_sha,
        &"2".repeat(40),
        "main",
    )
    .await
    .unwrap());
    db.execute(
        "UPDATE pull_requests SET title = 'Later title' WHERE id = $1",
        &[pull.id],
    )
    .await;
    let duplicate = db::ci::enqueue_ci_run(db.pool(), input(), &[])
        .await
        .unwrap();
    assert_eq!(duplicate.id, queued.id);
    assert_eq!(duplicate.pull_request_event, queued.pull_request_event);
    let claimed = db::ci::claim_ci_run(db.pool(), WORKER, "local", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.mode, "local");
    assert_eq!(claimed.head_sha, Some(pull.head_sha));
    assert_eq!(
        claimed.pull_request_event,
        Some(serde_json::json!({
            "number": pull.number, "title": "Original title", "head_ref": "topic",
            "base_ref": "main", "base_sha": "1".repeat(40),
        }))
    );
    assert_eq!(db.run(queued.run_id).await.mode, "local");
    let next = db.enqueue(w.repo, w.owner, false, &[]).await;
    assert_eq!(next.mode, "unassigned");
    assert!(db::ci::claim_ci_run(db.pool(), "worker-2", "invalid", &[])
        .await
        .is_err());
    assert_eq!(db.run(next.run_id).await.status, "queued");
    let claimed = db::ci::claim_ci_run(db.pool(), "worker-2", "podman", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, next.id);
    assert_eq!(claimed.mode, "podman");
    assert_eq!(db.run(next.run_id).await.mode, "podman");
}

// ---- A cluster claim needs its node to be an active actions worker ----

/// A claim as `node_key` must fail and leave the run queued with no worker.
async fn refused_claim(db: &Database, node_key: &str, run: i64, trusted: &[i64]) {
    let worker = format!("{node_key}/slot-1");
    let claimed = db::ci::claim_ci_run_on_node(db.pool(), node_key, &worker, "local", trusted)
        .await
        .unwrap();
    assert!(claimed.is_none(), "{node_key} claimed a run it may not run");
    let queued = db
        .count(
            "SELECT COUNT(*) FROM ci_runs WHERE id = $1 AND status = 'queued' AND worker IS NULL",
            &[run],
        )
        .await;
    assert_eq!(queued, 1, "the refused claim changed run {run}");
}

async fn cluster_claim_needs_an_active_actions_worker_node(db: &Database) {
    let w = world(db, "node_claim").await;
    let trusted = [w.owner];
    let queued = db.enqueue(w.repo, w.owner, false, &trusted).await;
    let key = "a".repeat(64);
    let node = db::cluster::register_own_node(
        db.pool(),
        &key,
        "claim-node",
        &["actions-worker".into()],
        "test",
    )
    .await
    .unwrap();

    db::cluster::set_node_status(
        db.pool(),
        db::cluster::Authority::Operator,
        &node.name,
        "revoked",
    )
    .await
    .unwrap();
    refused_claim(db, &key, queued.id, &trusted).await;

    db::cluster::set_node_status(
        db.pool(),
        db::cluster::Authority::Operator,
        &node.name,
        "pending",
    )
    .await
    .unwrap();
    refused_claim(db, &key, queued.id, &trusted).await;

    // Active again, but no longer an actions worker.
    db::cluster::upsert_declared_nodes(
        db.pool(),
        &[db::cluster::DeclaredMember {
            public_key: key.clone(),
            name: node.name.clone(),
            roles: vec!["storage".into()],
        }],
    )
    .await
    .unwrap();
    refused_claim(db, &key, queued.id, &trusted).await;

    // An actions worker again, but under a new key: the old key belongs to no node.
    db::cluster::upsert_declared_nodes(
        db.pool(),
        &[db::cluster::DeclaredMember {
            public_key: key.clone(),
            name: node.name.clone(),
            roles: vec!["actions-worker".into()],
        }],
    )
    .await
    .unwrap();
    let rekeyed = "b".repeat(64);
    db.set_node_key(node.id, &rekeyed).await;
    refused_claim(db, &key, queued.id, &trusted).await;

    let worker = format!("{rekeyed}/slot-1");
    let claimed = db::ci::claim_ci_run_on_node(db.pool(), &rekeyed, &worker, "local", &trusted)
        .await
        .unwrap()
        .expect("the active actions worker claims the queued run");
    assert_eq!(claimed.id, queued.id);
    assert_eq!(claimed.status, "running");
    assert_eq!(claimed.worker.as_deref(), Some(worker.as_str()));
}

macro_rules! on_both_engines {
    ($($name:ident),+ $(,)?) => {
        mod sqlite {
            $(
                #[tokio::test]
                async fn $name() {
                    let mut database = super::Database::new(false).await;
                    super::$name(&database).await;
                    database.close().await;
                }
            )+
        }
        mod postgres {
            $(
                #[tokio::test]
                async fn $name() {
                    let mut database = super::Database::new(true).await;
                    super::$name(&database).await;
                    database.close().await;
                }
            )+
        }
    };
}

on_both_engines!(
    claim_checks_the_starters_access_again,
    a_long_line_of_denied_runs_is_cancelled_in_batches,
    revocations_stop_queued_and_running_runs,
    banning_the_last_owner_stops_organization_work,
    forcing_the_last_owner_password_stops_organization_work,
    ordinary_password_changes_leave_organization_work_running,
    finish_sweep_and_cancel_do_not_overwrite_each_other,
    the_global_run_list_follows_the_repository_read_rule,
    global_run_list_applies_visibility_before_the_limit,
    actions_models_bound_current_logs_and_keep_refreshing_status,
    workflows_keep_separate_statuses_and_newer_attempts_win,
    finishing_and_cancelling_broadcast_only_the_created_notification,
    ignoring_the_repository_silences_run_results,
    pull_request_runs_use_exact_approval_identity,
    queued_pull_request_inputs_survive_base_and_title_changes,
    cluster_claim_needs_an_active_actions_worker_node,
);

// ---- PostgreSQL: operations in a fixed order ----
//
// A second transaction holds a row lock where the real competitor would be, so the
// operation under test meets it in a known state. SQLite has one writer and runs
// whole transactions one after another, which the tests above already cover.

mod interleaved {
    use super::*;
    use sqlx::{PgPool, Postgres, Transaction};

    /// A transaction that has locked the run row, and its backend process id.
    async fn lock_run(raw: &PgPool, id: i64) -> (Transaction<'static, Postgres>, i32) {
        let mut tx = raw.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM ci_runs WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        (tx, pid)
    }

    /// Waits until `waiters` statements are stuck behind the transaction of process
    /// `holder`, directly or through each other.
    async fn wait_until_blocked_by(raw: &PgPool, holder: i32, waiters: i64) {
        for _ in 0..800 {
            let blocked: i64 = sqlx::query_scalar(
                "WITH RECURSIVE waiting(pid) AS (
                     SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))
                     UNION
                     SELECT a.pid FROM pg_stat_activity a JOIN waiting w ON w.pid = ANY(pg_blocking_pids(a.pid))
                 ) SELECT COUNT(*) FROM waiting",
            )
            .bind(holder)
            .fetch_one(raw)
            .await
            .unwrap();
            if blocked >= waiters {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("nothing is waiting on the lock the test holds");
    }

    fn fixture(db: &Database) -> &production::Fixture {
        match db {
            Database::Postgres(fixture) => fixture,
            Database::Sqlite(_) => unreachable!("PostgreSQL test"),
        }
    }

    /// A claimed run: queued by the owner and started by `WORKER`.
    async fn running_run(db: &Database, w: &World) -> CiRun {
        let run = db.enqueue(w.repo, w.owner, false, &[w.collab]).await;
        let claimed = db::ci::claim_ci_run(db.pool(), WORKER, "local", &[w.collab])
            .await
            .unwrap()
            .expect("claim");
        assert_eq!(claimed.id, run.id);
        claimed
    }

    async fn enqueue_once(
        pool: &db::DbPool,
        repo: i64,
        actor: i64,
        trusted: &[i64],
    ) -> anyhow::Result<CiRun> {
        let sha = format!("{:040x}", Uuid::new_v4().as_u128());
        let blobs = serde_json::json!({ ".github/workflows/ci.yml": sha });
        db::ci::enqueue_ci_run(
            pool,
            db::ci::NewCiRun {
                repo_id: repo,
                triggered_by_id: actor,
                pipeline: "ci",
                event: "push",
                pull_request_id: None,
                branch: "main",
                head_sha: &sha,
                workflow_path: WORKFLOW,
                workflow_blobs: &blobs,
                workflow_inputs: None,
                environment: None,
            },
            trusted,
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_sweep_leaves_a_run_while_its_worker_is_finishing_it() {
        let mut db = Database::new(true).await;
        let w = world(&db, "sweep").await;
        let run = running_run(&db, &w).await;
        db.stop_heartbeat(run.id).await;

        // The worker holds the run while it writes its outcome. The sweep sees a
        // stale heartbeat but must not wait for it or write over it.
        let (mut worker, _) = lock_run(&fixture(&db).raw, run.id).await;
        let swept = promptly("the stale sweep", db::ci::fail_stale_ci_runs(db.pool(), 90))
            .await
            .unwrap();
        assert_eq!(swept, 0);
        sqlx::query("UPDATE ci_runs SET status = 'success', completed_at = NOW() WHERE id = $1")
            .bind(run.id)
            .execute(&mut *worker)
            .await
            .unwrap();
        worker.commit().await.unwrap();

        let after = db.run(run.run_id).await;
        assert_eq!(after.status, "success");
        assert!(after.error_message.is_none());
        assert_eq!(
            db.notifications(w.owner).await,
            0,
            "the sweep announced nothing"
        );
        // Once the worker is done, the sweep still has nothing to fail.
        assert_eq!(db::ci::fail_stale_ci_runs(db.pool(), 90).await.unwrap(), 0);
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finishing_waits_for_a_sweep_in_progress_and_then_changes_nothing() {
        let mut db = Database::new(true).await;
        let w = world(&db, "finish").await;
        let run = running_run(&db, &w).await;

        // The sweep has the run and is writing the failure.
        let (mut sweep, holder) = lock_run(&fixture(&db).raw, run.id).await;
        sqlx::query("UPDATE ci_runs SET status = 'failed', error_message = 'swept', completed_at = NOW() WHERE id = $1")
            .bind(run.id)
            .execute(&mut *sweep)
            .await
            .unwrap();
        let pool = db.pool().clone();
        let finishing = tokio::spawn(async move {
            db::ci::finish_ci_run(&pool, run.id, WORKER, "success", None).await
        });
        wait_until_blocked_by(&fixture(&db).raw, holder, 1).await;
        sweep.commit().await.unwrap();

        let result = promptly("the finish", finishing).await.unwrap().unwrap();
        assert!(result.is_none(), "the sweep decided first");
        let after = db.run(run.run_id).await;
        assert_eq!(after.status, "failed");
        assert_eq!(after.error_message.as_deref(), Some("swept"));
        assert_eq!(
            db.notifications(w.owner).await,
            0,
            "the lost finish announced nothing"
        );
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_waits_for_a_claim_in_progress_and_then_asks_the_worker() {
        let mut db = Database::new(true).await;
        let w = world(&db, "cancel").await;
        let run = db.enqueue(w.repo, w.owner, false, &[w.collab]).await;

        // A worker is claiming the run: it has written "running" and not committed.
        let (mut claim, holder) = lock_run(&fixture(&db).raw, run.id).await;
        sqlx::query("UPDATE ci_runs SET status = 'running', worker = $2, started_at = NOW(), heartbeat_at = NOW() WHERE id = $1")
            .bind(run.id)
            .bind(WORKER)
            .execute(&mut *claim)
            .await
            .unwrap();
        let pool = db.pool().clone();
        let owner = w.owner;
        let cancelling =
            tokio::spawn(
                async move { db::ci::request_ci_cancellation(&pool, run.run_id, owner).await },
            );
        wait_until_blocked_by(&fixture(&db).raw, holder, 1).await;
        claim.commit().await.unwrap();

        // The cancellation saw a running run and asked its worker to stop; it did not
        // mark a run cancelled that a worker is executing.
        assert!(
            promptly("the cancellation", cancelling)
                .await
                .unwrap()
                .unwrap()
                .0
        );
        let after = db.run(run.run_id).await;
        assert_eq!(after.status, "running");
        assert!(after.cancel_requested);
        assert_eq!(db.notifications(w.owner).await, 0);
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_claim_skips_a_run_someone_else_holds() {
        let mut db = Database::new(true).await;
        let w = world(&db, "skip").await;
        let first = db.enqueue(w.repo, w.owner, false, &[w.collab]).await;
        let second = db.enqueue(w.repo, w.owner, false, &[w.collab]).await;

        let (held, _) = lock_run(&fixture(&db).raw, first.id).await;
        let claimed = promptly(
            "the claim",
            db::ci::claim_ci_run(db.pool(), WORKER, "local", &[w.collab]),
        )
        .await
        .unwrap()
        .expect("the second run is free");
        assert_eq!(claimed.id, second.id);
        held.rollback().await.unwrap();
        let claimed = db::ci::claim_ci_run(db.pool(), "worker-2", "local", &[w.collab])
            .await
            .unwrap()
            .expect("the first run is free again");
        assert_eq!(claimed.id, first.id);
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claims_each_get_a_different_run() {
        let mut db = Database::new(true).await;
        let w = world(&db, "race").await;
        let mut queued = Vec::new();
        for _ in 0..3 {
            queued.push(db.enqueue(w.repo, w.owner, false, &[w.collab]).await.id);
        }
        let mut claims = Vec::new();
        for index in 0..5 {
            let pool = db.pool().clone();
            let trusted = [w.collab];
            claims.push(tokio::spawn(async move {
                db::ci::claim_ci_run(&pool, &format!("worker-{index}"), "local", &trusted).await
            }));
        }
        let mut claimed = Vec::new();
        for claim in claims {
            if let Some(run) = promptly("a claim", claim).await.unwrap().unwrap() {
                claimed.push(run.id);
            }
        }
        claimed.sort();
        assert_eq!(claimed, queued, "every run went to exactly one worker");
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_owner_restrictions_converge_without_deadlock() {
        let mut db = Database::new(true).await;
        let site = db.site_admin("concurrent_site").await;
        let first_owner = db.actor("concurrent_first_owner").await;
        let second_owner = db.actor("concurrent_second_owner").await;
        let runner = db.actor("concurrent_runner").await;
        let organization = organization_repository(&db, first_owner, "concurrent_governance").await;
        applied(
            orgs::add_organization_member(
                db.pool(),
                first_owner,
                organization.id,
                second_owner,
                OrgRole::Owner,
            )
            .await
            .unwrap(),
        );
        db::repositories::access::grant_repo_access(
            db.pool(),
            runner,
            organization.repo,
            AccessLevel::Admin,
        )
        .await
        .unwrap();
        let trusted = [runner];
        let running = db.enqueue(organization.repo, runner, false, &trusted).await;
        assert_eq!(
            db::ci::claim_ci_run(db.pool(), WORKER, "local", &trusted)
                .await
                .unwrap()
                .unwrap()
                .id,
            running.id
        );
        let queued_deploy = db.enqueue(organization.repo, runner, true, &trusted).await;

        // Hold the organization parent so both account transitions reach the same
        // lock edge. Once released, one sees the other's committed restriction.
        let mut gate = fixture(&db).raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(organization.id)
            .fetch_one(&mut *gate)
            .await
            .unwrap();

        let pool = db.pool().clone();
        let banning = tokio::spawn(async move {
            db::users::transitions::change_ban(&pool, site, first_owner, true).await
        });
        let pool = db.pool().clone();
        let forcing = tokio::spawn(async move {
            db::users::accounts::update_user_password(&pool, second_owner, "temporary-hash", true)
                .await
        });
        wait_until_blocked_by(&fixture(&db).raw, holder, 2).await;
        gate.commit().await.unwrap();

        assert!(matches!(
            promptly("the concurrent ban", banning)
                .await
                .unwrap()
                .unwrap(),
            db::users::transitions::BanChange::Updated(user) if user.is_banned
        ));
        promptly("the concurrent forced password change", forcing)
            .await
            .unwrap()
            .unwrap();
        assert!(
            db::users::accounts::get_user_by_id(db.pool(), second_owner)
                .await
                .unwrap()
                .unwrap()
                .must_change_password
        );

        assert_cancelled(&db.run(queued_deploy.run_id).await, ACCESS_LOST);
        assert_eq!(deployment_status(&db, &queued_deploy).await, "cancelled");
        assert_eq!(
            db::ci::heartbeat_ci_run(db.pool(), running.id, WORKER)
                .await
                .unwrap(),
            Some(true)
        );
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enqueue_waits_for_a_direct_grant_revocation_and_then_refuses() {
        let mut db = Database::new(true).await;
        let w = world(&db, "enqueue_direct_revoke").await;
        let mut revocation = fixture(&db).raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revocation)
            .await
            .unwrap();
        sqlx::query("DELETE FROM repo_access WHERE user_id=$1 AND repo_id=$2")
            .bind(w.collab)
            .bind(w.repo)
            .execute(&mut *revocation)
            .await
            .unwrap();

        let pool = db.pool().clone();
        let trusted = [w.collab];
        let enqueueing =
            tokio::spawn(async move { enqueue_once(&pool, w.repo, w.collab, &trusted).await });
        wait_until_blocked_by(&fixture(&db).raw, holder, 1).await;
        revocation.commit().await.unwrap();

        let error = promptly("the waiting enqueue", enqueueing)
            .await
            .unwrap()
            .expect_err("the committed revocation must deny admission");
        assert!(matches!(
            error.downcast_ref::<db::ci::CiAdmissionRefusal>(),
            Some(db::ci::CiAdmissionRefusal::Denied(
                "CI repository execution permission denied"
            ))
        ));
        assert_eq!(
            db.count(
                "SELECT COUNT(*) FROM ci_runs WHERE repo_id=$1 AND triggered_by_id=$2",
                &[w.repo, w.collab]
            )
            .await,
            0
        );
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_waits_for_a_direct_grant_revocation_and_then_cancels() {
        let mut db = Database::new(true).await;
        let w = world(&db, "claim_direct_revoke").await;
        let trusted = [w.collab];
        let run = db.enqueue(w.repo, w.collab, false, &trusted).await;
        let mut revocation = fixture(&db).raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revocation)
            .await
            .unwrap();
        sqlx::query("DELETE FROM repo_access WHERE user_id=$1 AND repo_id=$2")
            .bind(w.collab)
            .bind(w.repo)
            .execute(&mut *revocation)
            .await
            .unwrap();

        let pool = db.pool().clone();
        let claiming =
            tokio::spawn(
                async move { db::ci::claim_ci_run(&pool, WORKER, "local", &trusted).await },
            );
        wait_until_blocked_by(&fixture(&db).raw, holder, 1).await;
        revocation.commit().await.unwrap();

        assert!(promptly("the waiting claim", claiming)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        assert_cancelled(&db.run(run.run_id).await, ACCESS_LOST);
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enqueue_waits_for_an_organization_membership_revocation() {
        let mut db = Database::new(true).await;
        let owner = db.actor("enqueue_org_revoke_owner").await;
        let actor = db.actor("enqueue_org_revoke_actor").await;
        let organization = organization_repository(&db, owner, "enqueue_org_revoke").await;
        applied(
            orgs::add_organization_member(db.pool(), owner, organization.id, actor, OrgRole::Admin)
                .await
                .unwrap(),
        );
        let mut revocation = fixture(&db).raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *revocation)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(organization.id)
            .fetch_one(&mut *revocation)
            .await
            .unwrap();
        sqlx::query("DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2")
            .bind(organization.id)
            .bind(actor)
            .execute(&mut *revocation)
            .await
            .unwrap();

        let pool = db.pool().clone();
        let trusted = [actor];
        let enqueueing =
            tokio::spawn(
                async move { enqueue_once(&pool, organization.repo, actor, &trusted).await },
            );
        wait_until_blocked_by(&fixture(&db).raw, holder, 1).await;
        revocation.commit().await.unwrap();

        let error = promptly("the organization enqueue", enqueueing)
            .await
            .unwrap()
            .expect_err("the committed membership revocation must deny admission");
        assert!(matches!(
            error.downcast_ref::<db::ci::CiAdmissionRefusal>(),
            Some(db::ci::CiAdmissionRefusal::Denied(
                "CI repository execution permission denied"
            ))
        ));
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_claim_waits_for_a_ban_in_progress_and_then_cancels_the_run() {
        let mut db = Database::new(true).await;
        let w = world(&db, "ban").await;
        let trusted = [w.collab];
        let run = db.enqueue(w.repo, w.collab, false, &trusted).await;

        // The ban is being written: the account row is changed and not yet committed.
        let mut ban = fixture(&db).raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *ban)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET is_banned = TRUE WHERE id = $1")
            .bind(w.collab)
            .execute(&mut *ban)
            .await
            .unwrap();
        let pool = db.pool().clone();
        let claiming =
            tokio::spawn(
                async move { db::ci::claim_ci_run(&pool, WORKER, "local", &trusted).await },
            );
        wait_until_blocked_by(&fixture(&db).raw, holder, 1).await;
        ban.commit().await.unwrap();

        // The claim did not start the run on the account as it was before the ban.
        let claimed = promptly("the claim", claiming).await.unwrap().unwrap();
        assert!(claimed.is_none());
        assert_cancelled(&db.run(run.run_id).await, ACCESS_LOST);
        db.close().await;
    }
}
