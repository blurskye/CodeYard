//! Collaborator access changes and PostgreSQL bans while CI is finishing.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/native_database.rs"]
mod native;

use codeyard::backend::db::{
    self,
    models::{AccessLevel, CiRun, OrgRole},
    organizations::{self, OrganizationOutcome},
    users::transitions::BanChange,
};
use std::time::Duration;
use uuid::Uuid;

const WORKER: &str = "access-change-worker";

async fn repository_with_writer(pool: &db::DbPool, name: &str) -> (i64, i64, i64) {
    let owner = db::users::accounts::create_user(pool, &format!("{name}_owner"), None)
        .await
        .unwrap()
        .id;
    let writer = db::users::accounts::create_user(pool, &format!("{name}_writer"), None)
        .await
        .unwrap()
        .id;
    let repo = db::repositories::records::create_repository(
        pool,
        owner,
        name,
        None,
        true,
        "/fixture/not-executed.git",
    )
    .await
    .unwrap()
    .id;
    db::repositories::access::grant_repo_access(pool, writer, repo, AccessLevel::Write)
        .await
        .unwrap();
    (owner, writer, repo)
}

async fn enqueue(
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
            workflow_path: ".github/workflows/ci.yml",
            workflow_blobs: &blobs,
            workflow_inputs: None,
            environment: None,
        },
        trusted,
    )
    .await
}

async fn run(pool: &db::DbPool, id: Uuid) -> CiRun {
    db::ci::get_ci_run_by_run_id(pool, id)
        .await
        .unwrap()
        .unwrap()
}

async fn grant_changes(pool: &db::DbPool) {
    for (name, level, reduced) in [
        ("downgrade", AccessLevel::Read, true),
        ("unchanged", AccessLevel::Write, false),
        ("upgrade", AccessLevel::Admin, false),
    ] {
        let (owner, writer, repo) = repository_with_writer(pool, name).await;
        let trusted = [owner, writer];
        let running = enqueue(pool, repo, writer, &trusted).await.unwrap();
        assert_eq!(
            db::ci::claim_ci_run(pool, WORKER, "podman", &trusted)
                .await
                .unwrap()
                .unwrap()
                .id,
            running.id
        );
        let queued = enqueue(pool, repo, writer, &trusted).await.unwrap();

        let grant = db::repositories::access::grant_repo_access(pool, writer, repo, level)
            .await
            .unwrap();
        assert_eq!(grant.level(), level);
        assert_eq!(
            db::repositories::access::get_user_repo_access(pool, writer, repo)
                .await
                .unwrap(),
            level
        );
        let running_now = run(pool, running.run_id).await;
        assert_eq!(running_now.status, "running", "{name}");
        assert_eq!(running_now.cancel_requested, reduced, "{name}");
        assert_eq!(
            db::ci::heartbeat_ci_run(pool, running.id, WORKER)
                .await
                .unwrap(),
            Some(reduced),
            "{name}"
        );

        let queued_now = run(pool, queued.run_id).await;
        let claimed = db::ci::claim_ci_run(pool, WORKER, "podman", &trusted)
            .await
            .unwrap();
        if reduced {
            assert_eq!(queued_now.status, "cancelled");
            assert_eq!(
                queued_now.error_message.as_deref(),
                Some(db::ci::ACCESS_LOST_MESSAGE)
            );
            assert!(queued_now.completed_at.is_some());
            assert!(queued_now.worker.is_none() && queued_now.started_at.is_none());
            assert!(
                claimed.is_none(),
                "a downgraded writer cannot start another run"
            );
            let denied = enqueue(pool, repo, writer, &trusted).await.unwrap_err();
            assert!(denied
                .to_string()
                .contains("CI repository execution permission denied"));
        } else {
            assert_eq!(queued_now.status, "queued", "{name}");
            assert!(!queued_now.cancel_requested, "{name}");
            assert!(queued_now.completed_at.is_none(), "{name}");
            assert_eq!(claimed.unwrap().id, queued.id, "{name}");
        }
    }
}

#[tokio::test]
async fn sqlite_grant_changes_stop_ci_only_on_reduction() {
    let fixture = native::NativeDatabase::new().await;
    grant_changes(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_grant_changes_stop_ci_only_on_reduction() {
    let fixture = native::PostgresDatabase::new().await;
    grant_changes(&fixture.pool).await;
}

async fn wait_until_blocked_by(raw: &sqlx::PgPool, holder: i32, waiters: i64) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
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
    })
    .await
    .expect("the operations must be waiting on the held row lock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_ban_allows_ci_finish_notification_foreign_keys() {
    let fixture = native::PostgresDatabase::new().await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture");
    };
    let (admin, writer, repo) = repository_with_writer(&fixture.pool, "ban_finish").await;
    sqlx::query("UPDATE users SET is_admin = TRUE WHERE id = $1")
        .bind(admin)
        .execute(raw)
        .await
        .unwrap();
    let trusted = [admin, writer];
    let writer_run = enqueue(&fixture.pool, repo, writer, &trusted)
        .await
        .unwrap();
    assert_eq!(
        db::ci::claim_ci_run(&fixture.pool, WORKER, "podman", &trusted)
            .await
            .unwrap()
            .unwrap()
            .id,
        writer_run.id
    );
    let admin_run = enqueue(&fixture.pool, repo, admin, &trusted).await.unwrap();
    assert_eq!(
        db::ci::claim_ci_run(&fixture.pool, WORKER, "podman", &trusted)
            .await
            .unwrap()
            .unwrap()
            .id,
        admin_run.id
    );

    // Hold the status row so the real finish owns its run before the ban starts.
    let mut gate = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM commit_statuses WHERE ci_run_id = $1 FOR UPDATE")
        .bind(writer_run.id)
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    let pool = fixture.pool.clone();
    let writer_run_id = writer_run.id;
    let finishing = tokio::spawn(async move {
        db::ci::finish_ci_run(&pool, writer_run_id, WORKER, "success", None).await
    });
    wait_until_blocked_by(raw, holder, 1).await;
    let pool = fixture.pool.clone();
    let banning = tokio::spawn(async move {
        db::users::transitions::change_ban(&pool, admin, writer, true).await
    });
    wait_until_blocked_by(raw, holder, 2).await;

    // The ban holds the admin and target user locks while waiting for the writer's
    // run. Notifications for either user must still be able to take FK key-share.
    let (finished_admin, admin_notifications) = tokio::time::timeout(
        Duration::from_secs(30),
        db::ci::finish_ci_run(&fixture.pool, admin_run.id, WORKER, "success", None),
    )
    .await
    .expect("the admin lock must not block CI finish")
    .unwrap()
    .unwrap();
    assert_eq!(finished_admin.status, "success");
    assert_eq!(admin_notifications.len(), 1);
    assert_eq!(admin_notifications[0].0, admin);
    gate.commit().await.unwrap();

    let (finished_writer, writer_notifications) =
        tokio::time::timeout(Duration::from_secs(30), finishing)
            .await
            .expect("the target lock must not deadlock CI finish")
            .unwrap()
            .unwrap()
            .unwrap();
    assert_eq!(finished_writer.status, "success");
    assert_eq!(writer_notifications.len(), 1);
    assert_eq!(writer_notifications[0].0, writer);
    let ban = tokio::time::timeout(Duration::from_secs(30), banning)
        .await
        .expect("the ban must finish after CI")
        .unwrap()
        .unwrap();
    assert!(matches!(ban, BanChange::Updated(user) if user.is_banned));
}

/// Claims one run in a fresh repository, then holds that repository's row lock the
/// way revocation and deletion do. Returns the run id, the holding transaction and
/// its backend id.
async fn claimed_run_behind_repository_lock(
    pool: &db::DbPool,
    raw: &sqlx::PgPool,
    name: &str,
) -> (i64, sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let (owner, writer, repo) = repository_with_writer(pool, name).await;
    let trusted = [owner, writer];
    let run = enqueue(pool, repo, writer, &trusted).await.unwrap();
    assert_eq!(
        db::ci::claim_ci_run(pool, WORKER, "podman", &trusted)
            .await
            .unwrap()
            .unwrap()
            .id,
        run.id
    );
    let mut gate = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM repositories WHERE id = $1 FOR UPDATE")
        .bind(repo)
        .execute(&mut *gate)
        .await
        .unwrap();
    (run.id, gate, holder)
}

/// Revocation and deletion hold the repository lock and then need its runs, so a
/// finish waiting for that lock must not already hold the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_repository_lock_holder_takes_run_while_ci_finish_waits() {
    let fixture = native::PostgresDatabase::new().await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture");
    };
    let (run_id, mut gate, holder) =
        claimed_run_behind_repository_lock(&fixture.pool, raw, "finish_repo_lock").await;
    let pool = fixture.pool.clone();
    let finishing =
        tokio::spawn(
            async move { db::ci::finish_ci_run(&pool, run_id, WORKER, "success", None).await },
        );
    wait_until_blocked_by(raw, holder, 1).await;
    sqlx::query("SELECT id FROM ci_runs WHERE id = $1 FOR UPDATE")
        .bind(run_id)
        .execute(&mut *gate)
        .await
        .expect("the run lock must not deadlock with CI finish");
    gate.commit().await.unwrap();
    let (finished, _) = tokio::time::timeout(Duration::from_secs(30), finishing)
        .await
        .expect("CI finish must complete")
        .unwrap()
        .unwrap()
        .expect("the running run finishes");
    assert_eq!(finished.status, "success");
}

/// The stale sweep locks each run's repository before the run, as finish does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_repository_lock_holder_takes_run_while_stale_sweep_waits() {
    let fixture = native::PostgresDatabase::new().await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture");
    };
    let (run_id, mut gate, holder) =
        claimed_run_behind_repository_lock(&fixture.pool, raw, "sweep_repo_lock").await;
    let pool = fixture.pool.clone();
    // Zero seconds: the claimed run's heartbeat is already older than now.
    let sweeping = tokio::spawn(async move { db::ci::fail_stale_ci_runs(&pool, 0).await });
    wait_until_blocked_by(raw, holder, 1).await;
    sqlx::query("SELECT id FROM ci_runs WHERE id = $1 FOR UPDATE")
        .bind(run_id)
        .execute(&mut *gate)
        .await
        .expect("the run lock must not deadlock with the stale sweep");
    gate.commit().await.unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(30), sweeping)
        .await
        .expect("the stale sweep must complete")
        .unwrap()
        .unwrap();
    assert_eq!(failed, 1);
    let status: String = sqlx::query_scalar("SELECT status FROM ci_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(raw)
        .await
        .unwrap();
    assert_eq!(status, "failed");
}

/// Cancelling a queued run takes the repository lock before the run, as finish does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_repository_lock_holder_takes_run_while_cancellation_waits() {
    let fixture = native::PostgresDatabase::new().await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture");
    };
    let (owner, writer, repo) = repository_with_writer(&fixture.pool, "cancel_repo_lock").await;
    let queued = enqueue(&fixture.pool, repo, writer, &[owner, writer])
        .await
        .unwrap();
    let mut gate = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM repositories WHERE id = $1 FOR UPDATE")
        .bind(repo)
        .execute(&mut *gate)
        .await
        .unwrap();
    let pool = fixture.pool.clone();
    let run_uuid = queued.run_id;
    let cancelling =
        tokio::spawn(async move { db::ci::request_ci_cancellation(&pool, run_uuid, writer).await });
    wait_until_blocked_by(raw, holder, 1).await;
    sqlx::query("SELECT id FROM ci_runs WHERE id = $1 FOR UPDATE")
        .bind(queued.id)
        .execute(&mut *gate)
        .await
        .expect("the run lock must not deadlock with the cancellation");
    gate.commit().await.unwrap();
    let (cancelled, _) = tokio::time::timeout(Duration::from_secs(30), cancelling)
        .await
        .expect("the cancellation must complete")
        .unwrap()
        .unwrap();
    assert!(cancelled);
    let status: String = sqlx::query_scalar("SELECT status FROM ci_runs WHERE id = $1")
        .bind(queued.id)
        .fetch_one(raw)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_cancellation_retains_admin_authority_while_waiting() {
    let fixture = native::PostgresDatabase::new().await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture");
    };
    for lock in ["repository", "run"] {
        let (owner, admin, repo) = repository_with_writer(&fixture.pool, lock).await;
        sqlx::query("UPDATE users SET is_admin=TRUE WHERE id IN ($1,$2)")
            .bind(owner)
            .bind(admin)
            .execute(raw)
            .await
            .unwrap();
        let queued = enqueue(&fixture.pool, repo, owner, &[owner]).await.unwrap();
        let mut gate = raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        let (sql, id) = if lock == "repository" {
            ("SELECT id FROM repositories WHERE id=$1 FOR UPDATE", repo)
        } else {
            ("SELECT id FROM ci_runs WHERE id=$1 FOR UPDATE", queued.id)
        };
        sqlx::query(sql).bind(id).execute(&mut *gate).await.unwrap();
        let run_id = queued.run_id;
        let pool = fixture.pool.clone();
        let cancelling =
            tokio::spawn(
                async move { db::ci::request_ci_cancellation(&pool, run_id, admin).await },
            );
        wait_until_blocked_by(raw, holder, 1).await;
        let pool = fixture.pool.clone();
        let banning = tokio::spawn(async move {
            db::users::transitions::change_ban(&pool, owner, admin, true).await
        });
        // A ban cannot commit while the cancellation retains that actor's authority.
        wait_until_blocked_by(raw, holder, 2).await;
        assert!(!banning.is_finished());
        gate.commit().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(30), cancelling)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .0
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(30), banning)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BanChange::Updated(_)
        ));
        assert_eq!(run(&fixture.pool, run_id).await.status, "cancelled");
        let next = enqueue(&fixture.pool, repo, owner, &[owner]).await.unwrap();
        assert!(
            db::ci::request_ci_cancellation(&fixture.pool, next.run_id, admin)
                .await
                .is_err()
        );
        assert_eq!(run(&fixture.pool, next.run_id).await.status, "queued");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_cancellation_rechecks_admin_after_waiting() {
    let fixture = native::PostgresDatabase::new().await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture");
    };
    for restriction in [
        "is_banned=TRUE",
        "must_change_password=TRUE",
        "is_admin=FALSE",
    ] {
        let name = format!("recheck_{}", restriction.split('=').next().unwrap());
        let (owner, admin, repo) = repository_with_writer(&fixture.pool, &name).await;
        sqlx::query("UPDATE users SET is_admin=TRUE WHERE id=$1")
            .bind(admin)
            .execute(raw)
            .await
            .unwrap();
        let queued = enqueue(&fixture.pool, repo, owner, &[owner]).await.unwrap();
        let mut gate = raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        // Keep the old snapshot readable, but make both old and new cancellation
        // paths wait until the account restriction commits.
        sqlx::query(&format!("UPDATE users SET {restriction} WHERE id=$1"))
            .bind(admin)
            .execute(&mut *gate)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(repo)
            .execute(&mut *gate)
            .await
            .unwrap();
        let run_id = queued.run_id;
        let pool = fixture.pool.clone();
        let cancelling =
            tokio::spawn(
                async move { db::ci::request_ci_cancellation(&pool, run_id, admin).await },
            );
        wait_until_blocked_by(raw, holder, 1).await;
        gate.commit().await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(30), cancelling)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<db::ci::CiAdmissionRefusal>(),
                Some(db::ci::CiAdmissionRefusal::Denied(_))
            ),
            "{error:?}"
        );
        let unchanged = run(&fixture.pool, run_id).await;
        assert_eq!(unchanged.status, "queued");
        assert!(!unchanged.cancel_requested);
    }
}

async fn cancellation_after_access_loss(pool: &db::DbPool) {
    let (owner, writer, repo) = repository_with_writer(pool, "starter_cancel").await;
    let running = enqueue(pool, repo, writer, &[writer]).await.unwrap();
    assert_eq!(
        db::ci::claim_ci_run(pool, WORKER, "local", &[writer])
            .await
            .unwrap()
            .unwrap()
            .id,
        running.id
    );
    db::repositories::access::revoke_repo_access(pool, writer, repo)
        .await
        .unwrap();
    let archive = "UPDATE repositories SET is_archived=TRUE WHERE id=$1";
    match pool {
        db::DbPool::Postgres(raw) => {
            sqlx::query(archive).bind(repo).execute(raw).await.unwrap();
        }
        db::DbPool::Sqlite(raw) => {
            sqlx::query(archive).bind(repo).execute(raw).await.unwrap();
        }
    };
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, writer, repo)
            .await
            .unwrap(),
        AccessLevel::None
    );
    assert!(
        db::ci::request_ci_cancellation(pool, running.run_id, writer)
            .await
            .unwrap()
            .0
    );
    assert!(run(pool, running.run_id).await.cancel_requested);
    // Even the starter exception requires an unrestricted account.
    db::users::accounts::update_user_password(pool, writer, "unused", true)
        .await
        .unwrap();
    assert!(
        db::ci::request_ci_cancellation(pool, running.run_id, writer)
            .await
            .is_err()
    );
    assert!(
        db::ci::request_ci_cancellation(pool, running.run_id, owner)
            .await
            .unwrap()
            .0
    );
}

#[tokio::test]
async fn sqlite_starter_can_cancel_after_access_loss() {
    let fixture = native::NativeDatabase::new().await;
    cancellation_after_access_loss(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_starter_can_cancel_after_access_loss() {
    let fixture = native::PostgresDatabase::new().await;
    cancellation_after_access_loss(&fixture.pool).await;
}

fn applied<T: std::fmt::Debug>(outcome: OrganizationOutcome<T>) -> T {
    match outcome {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("expected applied, got {other:?}"),
    }
}

/// An organization repository that a team member can write only through the team's
/// grant. Returns the owner, the member, the organization, the team and the repository.
async fn team_writer_repository(
    pool: &db::DbPool,
    raw: &sqlx::PgPool,
    name: &str,
) -> (i64, i64, i64, i64, i64) {
    let owner = db::users::accounts::create_user(pool, &format!("{name}_owner"), None)
        .await
        .unwrap()
        .id;
    let member = db::users::accounts::create_user(pool, &format!("{name}_member"), None)
        .await
        .unwrap()
        .id;
    let org = applied(
        organizations::create_organization(
            pool,
            owner,
            &organizations::NewOrganization::new(
                name,
                organizations::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        organizations::add_organization_member(pool, owner, org, member, OrgRole::Member)
            .await
            .unwrap(),
    );
    let team = applied(
        organizations::create_organization_team(
            pool,
            owner,
            org,
            &organizations::NewTeam::new(
                name,
                organizations::TeamProfile::new(name, None, organizations::TeamPrivacy::Visible)
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        organizations::add_organization_team_member(
            pool,
            owner,
            org,
            team,
            member,
            organizations::TeamRole::Member,
        )
        .await
        .unwrap(),
    );
    let repo = db::repositories::records::create_repository(
        pool,
        owner,
        name,
        None,
        true,
        "/fixture/not-executed.git",
    )
    .await
    .unwrap()
    .id;
    sqlx::query(
        "UPDATE repositories SET owner_kind='org', owner_user_id=NULL, owner_org_id=$1 WHERE id=$2",
    )
    .bind(org)
    .bind(repo)
    .execute(raw)
    .await
    .unwrap();
    applied(
        organizations::grant_organization_team_repository(
            pool,
            owner,
            org,
            team,
            repo,
            organizations::TeamRepositoryPermission::new(3).unwrap(),
        )
        .await
        .unwrap(),
    );
    (owner, member, org, team, repo)
}

/// A team grant revocation locks the organization and then the repository. A claim
/// that kept an earlier candidate's repository locked while it waited for the
/// organization of a later candidate would deadlock with it. The claim waits for the
/// organization alone and then starts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_team_grant_revocation_and_claim_do_not_deadlock() {
    let fixture = native::PostgresDatabase::new().await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture");
    };
    let (owner, member, org, team, repo) =
        team_writer_repository(&fixture.pool, raw, "team_grant_claim").await;
    let trusted = [owner, member];
    // The flagged run comes first. The claim finishes it without a permission check
    // and must release its repository lock before it waits for the organization of
    // the run behind it.
    let flagged = enqueue(&fixture.pool, repo, member, &trusted)
        .await
        .unwrap();
    sqlx::query("UPDATE ci_runs SET cancel_requested = TRUE WHERE id = $1")
        .bind(flagged.id)
        .execute(raw)
        .await
        .unwrap();
    let queued = enqueue(&fixture.pool, repo, member, &trusted)
        .await
        .unwrap();

    // The revocation takes the organization, then waits for the team row this gate
    // holds. The claim then waits for the organization the revocation holds.
    let mut gate = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM teams WHERE id = $1 FOR UPDATE")
        .bind(team)
        .execute(&mut *gate)
        .await
        .unwrap();
    let pool = fixture.pool.clone();
    let revoking = tokio::spawn(async move {
        organizations::revoke_organization_team_repository(&pool, owner, org, team, repo).await
    });
    wait_until_blocked_by(raw, holder, 1).await;
    let pool = fixture.pool.clone();
    let claiming =
        tokio::spawn(async move { db::ci::claim_ci_run(&pool, WORKER, "local", &trusted).await });
    wait_until_blocked_by(raw, holder, 2).await;
    gate.commit().await.unwrap();

    let revoked = tokio::time::timeout(Duration::from_secs(30), revoking)
        .await
        .expect("the revocation must not deadlock with the claim")
        .unwrap()
        .unwrap();
    assert!(matches!(revoked, OrganizationOutcome::Applied(())));
    let claimed = tokio::time::timeout(Duration::from_secs(30), claiming)
        .await
        .expect("the claim must complete")
        .unwrap()
        .unwrap();
    assert!(
        claimed.is_none(),
        "a starter who lost access must not start a run"
    );
    let finished = run(&fixture.pool, flagged.run_id).await;
    assert_eq!(finished.status, "cancelled");
    assert_eq!(
        finished.error_message.as_deref(),
        Some("Cancelled before it started")
    );
    let cancelled = run(&fixture.pool, queued.run_id).await;
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(
        cancelled.error_message.as_deref(),
        Some(db::ci::ACCESS_LOST_MESSAGE)
    );
}

/// A ban by a site admin holds the admin's own row and then asks for the target. A
/// collaborator's write on the admin's personal repository holds its own row while
/// it checks the admin's row, so the two must not wait on each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_owner_ban_and_collaborator_write_do_not_deadlock() {
    let fixture = native::PostgresDatabase::new().await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture");
    };
    let (owner, writer, repo) = repository_with_writer(&fixture.pool, "owner_ban_write").await;
    sqlx::query("UPDATE users SET is_admin = TRUE WHERE id = $1")
        .bind(owner)
        .execute(raw)
        .await
        .unwrap();
    let mut ban = raw.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *ban)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR NO KEY UPDATE")
        .bind(owner)
        .execute(&mut *ban)
        .await
        .unwrap();
    let pool = fixture.pool.clone();
    let writing = tokio::spawn(async move {
        db::issues::create_issue(&pool, repo, writer, "Owner ban race", "body").await
    });
    // The write either waits for the owner's row or has already finished.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
            )
            .bind(holder)
            .fetch_one(raw)
            .await
            .unwrap();
            if waiting > 0 || writing.is_finished() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the write must wait for the owner's row or finish");
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR NO KEY UPDATE")
        .bind(writer)
        .execute(&mut *ban)
        .await
        .expect("the ban must not deadlock with the collaborator's write");
    ban.commit().await.unwrap();
    let issue = tokio::time::timeout(Duration::from_secs(30), writing)
        .await
        .expect("the write must complete")
        .unwrap()
        .unwrap();
    assert_eq!(issue.title, "Owner ban race");
}
