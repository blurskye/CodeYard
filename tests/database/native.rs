//! Real native SQLite contracts and populated PostgreSQL parity.
#![cfg(not(target_arch = "wasm32"))]
use crate::support::native_database as support;
use chrono::{DateTime, Duration, NaiveDate, SecondsFormat, Utc};
use codeyard::backend::db::discussions::ListFilter;
use codeyard::backend::{
    crypto::SecretEncryptor,
    db::{
        self,
        discussions::DiscussionEdit,
        files::{self, AdmissionError, ReserveUpload},
        models::{FileReason, FileVisibility},
        repositories,
        users::{
            admission, sessions,
            transitions::{self, AccountAdmission, BanChange, SetupClaim},
        },
    },
};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, sqlite::SqlitePoolOptions, Connection, PgConnection, Row};
use std::sync::Arc;
use support::{token, NativeDatabase, PostgresDatabase};
use uuid::Uuid;

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}
async fn setup(pool: &db::DbPool, mode: &str) -> db::models::User {
    match transitions::complete_setup(pool, "admin", None, "fixture-hash", mode, None)
        .await
        .unwrap()
    {
        SetupClaim::Completed(user) => *user,
        other => panic!("unexpected setup outcome: {other:?}"),
    }
}
async fn repository(pool: &db::DbPool, owner: i64, name: &str) -> db::models::Repository {
    db::repositories::records::create_repository(
        pool,
        owner,
        name,
        None,
        true,
        "/owned/native-fixture.git",
    )
    .await
    .unwrap()
}
fn upload(user_id: i64, reason: FileReason, visibility: FileVisibility) -> ReserveUpload<'static> {
    ReserveUpload {
        user_id,
        filename: "fixture.bin",
        content_type: if reason == FileReason::Avatar {
            "image/png"
        } else {
            "application/octet-stream"
        },
        file_size: 10,
        reason,
        visibility,
        sha256_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    }
}
async fn new_pr(
    pool: &db::DbPool,
    repo: i64,
    actor: i64,
    head_branch: &str,
) -> db::models::PullRequest {
    db::pull_requests::records::create_pull_request(
        pool,
        db::pull_requests::records::NewPullRequest {
            head_repo_id: None,
            repo_id: repo,
            author_id: actor,
            title: "PR",
            body: "body",
            head_branch,
            head_sha: "1111111111111111111111111111111111111111",
            base_branch: "main",
            base_sha: "0000000000000000000000000000000000000000",
            is_draft: false,
        },
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_setup_has_one_winner_and_constraint_failure_rolls_back() {
    let fixture = NativeDatabase::new().await;
    assert!(
        transitions::complete_setup(&fixture.pool, "bad", None, "hash", "invalid-mode", None)
            .await
            .is_err()
    );
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        0
    );
    assert!(db::node_config::get_node_configuration(&fixture.pool)
        .await
        .unwrap()
        .is_none());
    let other = fixture.second_handle().await;
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..8 {
        let pool = if index % 2 == 0 {
            fixture.pool.clone()
        } else {
            other.clone()
        };
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            transitions::complete_setup(
                &pool,
                &format!("admin_{index}"),
                None,
                "hash",
                if index % 2 == 0 { "public" } else { "private" },
                None,
            )
            .await
            .unwrap()
        });
    }
    barrier.wait().await;
    let mut completed = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            SetupClaim::Completed(user) => {
                assert!(user.is_admin);
                completed += 1;
            }
            SetupClaim::AlreadyCompleted => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(completed, 1);
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        1
    );
    assert!(
        db::node_config::get_node_configuration(&fixture.pool)
            .await
            .unwrap()
            .unwrap()
            .setup_completed
    );
    other.close().await;
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_enrollment_review_bans_and_password_cas_are_atomic() {
    let fixture = NativeDatabase::new().await;
    let admin = setup(&fixture.pool, "private").await;
    assert!(matches!(
        transitions::register_password(&fixture.pool, "requested", None, "hash", None)
            .await
            .unwrap(),
        AccountAdmission::Requested
    ));
    assert!(matches!(
        transitions::register_password(&fixture.pool, "REQUESTED", None, "hash", None)
            .await
            .unwrap(),
        AccountAdmission::Conflict
    ));
    let request = db::users::signup_requests::list_signup_requests(&fixture.pool, "pending")
        .await
        .unwrap()
        .remove(0);
    let (a, b) = tokio::join!(
        db::users::signup_requests::approve_signup_request(&fixture.pool, request.id, admin.id),
        db::users::signup_requests::approve_signup_request(&fixture.pool, request.id, admin.id)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let user = db::users::accounts::get_user_by_username(&fixture.pool, "requested")
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        transitions::admit_oauth_account(&fixture.pool, "github", "subject", " outsider", None)
            .await
            .unwrap(),
        AccountAdmission::ApprovalRequired
    ));
    assert!(transitions::enroll_oauth_identity(
        &fixture.pool,
        admin.id,
        user.id,
        "github",
        "subject"
    )
    .await
    .unwrap());
    assert!(matches!(
        transitions::admit_oauth_account(&fixture.pool, "github", "subject", "ignored", None)
            .await
            .unwrap(),
        AccountAdmission::User(_)
    ));
    let session = token();
    db::users::sessions::create_session(
        &fixture.pool,
        &session,
        user.id,
        None,
        Some("192.0.2.10"),
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap();
    assert!(!transitions::compare_exchange_password(
        &fixture.pool,
        user.id,
        "stale",
        "new",
        true,
        true
    )
    .await
    .unwrap());
    assert!(db::users::sessions::get_session(&fixture.pool, &session)
        .await
        .unwrap()
        .is_some());
    assert!(transitions::compare_exchange_password(
        &fixture.pool,
        user.id,
        "hash",
        "new",
        true,
        true
    )
    .await
    .unwrap());
    assert!(db::users::sessions::get_session(&fixture.pool, &session)
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        transitions::change_ban(&fixture.pool, user.id, admin.id, true)
            .await
            .unwrap(),
        BanChange::Denied
    ));
    assert!(matches!(
        transitions::change_ban(&fixture.pool, admin.id, admin.id, true)
            .await
            .unwrap(),
        BanChange::SelfBan
    ));
    assert!(matches!(
        transitions::change_ban(&fixture.pool, admin.id, user.id, true)
            .await
            .unwrap(),
        BanChange::Updated(_)
    ));
    assert!(matches!(
        transitions::admit_oauth_account(&fixture.pool, "github", "subject", "ignored", None)
            .await
            .unwrap(),
        AccountAdmission::Denied
    ));
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        2,
        "suspension does not delete data"
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_and_global_password_budgets_are_shared_by_independent_handles() {
    let fixture = NativeDatabase::new().await;
    let other = fixture.second_handle().await;
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..16 {
        let pool = if index % 2 == 0 {
            fixture.pool.clone()
        } else {
            other.clone()
        };
        tasks.spawn(async move {
            admission::admit_password_attempt(&pool, "user:42")
                .await
                .unwrap()
        });
    }
    let mut admitted = 0;
    while let Some(result) = tasks.join_next().await {
        admitted += usize::from(result.unwrap());
    }
    assert_eq!(admitted, 10);
    assert!(!admission::admit_password_attempt(&other, "user:42")
        .await
        .unwrap());
    // Denied account attempts spent global budget too: only 103 remain.
    for index in 0..103 {
        assert!(
            admission::admit_password_attempt(&fixture.pool, &format!("unknown:{index}"))
                .await
                .unwrap()
        );
    }
    assert!(!admission::admit_password_attempt(&other, "unknown:denied")
        .await
        .unwrap());
    let allocated: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_login_limits WHERE account_key='unknown:denied'",
    )
    .fetch_one(&fixture.raw)
    .await
    .unwrap();
    assert_eq!(allocated, 0);
    assert!(admission::admit_password_attempt(&other, "global")
        .await
        .is_err());
    other.close().await;
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_csrf_oauth_consumption_and_rotation_survive_reopen() {
    let mut fixture = NativeDatabase::new().await;
    let user = setup(&fixture.pool, "public").await;
    let old = token();
    sessions::create_browser_session(
        &fixture.pool,
        &old,
        Some("fixture"),
        Some("2001:db8::1"),
        Utc::now() + Duration::days(2),
    )
    .await
    .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let pool = fixture.pool.clone();
        let id = old.clone();
        tasks.spawn(async move {
            sessions::get_or_create_csrf_token(&pool, &id, None, &token(), 7200)
                .await
                .unwrap()
                .unwrap()
        });
    }
    let first = tasks.join_next().await.unwrap().unwrap();
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap(), first);
    }
    assert!(
        sessions::get_or_create_csrf_token(&fixture.pool, &old, Some(user.id), &token(), 7200)
            .await
            .unwrap()
            .is_none()
    );
    let collision = token();
    sessions::create_browser_session(
        &fixture.pool,
        &collision,
        None,
        None,
        Utc::now() + Duration::days(1),
    )
    .await
    .unwrap();
    assert!(sessions::rotate_authenticated_session(
        &fixture.pool,
        Some(&old),
        &collision,
        user.id,
        Some("fixture-hash"),
        None,
        Utc::now() + Duration::days(1)
    )
    .await
    .is_err());
    assert!(
        sessions::get_browser_session(&fixture.pool, &old)
            .await
            .unwrap()
            .is_some(),
        "collision rollback retains old session/CSRF"
    );
    let fresh = token();
    assert!(sessions::rotate_authenticated_session(
        &fixture.pool,
        Some(&old),
        &fresh,
        user.id,
        Some("stale"),
        None,
        Utc::now() + Duration::days(1)
    )
    .await
    .unwrap()
    .is_none());
    assert!(sessions::rotate_authenticated_session(
        &fixture.pool,
        Some(&old),
        &fresh,
        user.id,
        Some("fixture-hash"),
        None,
        Utc::now() + Duration::days(1)
    )
    .await
    .unwrap()
    .is_some());
    assert!(sessions::get_browser_session(&fixture.pool, &old)
        .await
        .unwrap()
        .is_none());
    let csrf =
        sessions::get_or_create_csrf_token(&fixture.pool, &fresh, Some(user.id), &token(), 7200)
            .await
            .unwrap()
            .unwrap();
    let state = token();
    let binding = token();
    sessions::save_bound_oauth_state(
        &fixture.pool,
        &state,
        "verifier",
        "github",
        "https://fixture.invalid/callback",
        &binding,
    )
    .await
    .unwrap();
    assert!(sessions::consume_bound_oauth_state(
        &fixture.pool,
        &state,
        "github",
        &token(),
        "https://fixture.invalid/callback"
    )
    .await
    .unwrap()
    .is_none());
    assert!(sessions::get_oauth_state(&fixture.pool, &state)
        .await
        .unwrap()
        .is_some());
    let (a, b) = tokio::join!(
        sessions::consume_bound_oauth_state(
            &fixture.pool,
            &state,
            "github",
            &binding,
            "https://fixture.invalid/callback"
        ),
        sessions::consume_bound_oauth_state(
            &fixture.pool,
            &state,
            "github",
            &binding,
            "https://fixture.invalid/callback"
        )
    );
    assert_eq!(
        usize::from(a.unwrap().is_some()) + usize::from(b.unwrap().is_some()),
        1
    );
    fixture.reopen().await;
    assert_eq!(
        sessions::get_browser_session(&fixture.pool, &fresh)
            .await
            .unwrap()
            .unwrap()
            .user_id,
        Some(user.id)
    );
    assert_eq!(
        sessions::get_or_create_csrf_token(&fixture.pool, &fresh, Some(user.id), &token(), 7200)
            .await
            .unwrap(),
        Some(csrf)
    );
    assert!(sessions::get_oauth_state(&fixture.pool, &state)
        .await
        .unwrap()
        .is_none());
    assert!(transitions::compare_exchange_password(
        &fixture.pool,
        user.id,
        "fixture-hash",
        "replacement",
        true,
        true
    )
    .await
    .unwrap());
    assert!(sessions::get_browser_session(&fixture.pool, &fresh)
        .await
        .unwrap()
        .is_none());
    fixture.close().await;
}

/// The stored CSRF token of a session, creating one when the session has none yet.
async fn stored_csrf(pool: &db::DbPool, id: &str, user: Option<i64>) -> String {
    sessions::get_or_create_csrf_token(pool, id, user, &token(), 7200)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_csrf_reads_do_not_wait_for_the_writer() {
    let fixture = NativeDatabase::new().await;
    let user = setup(&fixture.pool, "public").await;
    let anonymous = token();
    let member = token();
    sessions::create_browser_session(
        &fixture.pool,
        &anonymous,
        None,
        None,
        Utc::now() + Duration::days(2),
    )
    .await
    .unwrap();
    sessions::rotate_authenticated_session(
        &fixture.pool,
        None,
        &member,
        user.id,
        Some("fixture-hash"),
        None,
        Utc::now() + Duration::days(2),
    )
    .await
    .unwrap()
    .unwrap();
    let anonymous_csrf = stored_csrf(&fixture.pool, &anonymous, None).await;
    let member_csrf = stored_csrf(&fixture.pool, &member, Some(user.id)).await;

    // Another connection owns the SQLite writer. Current tokens still answer.
    let writer = fixture.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert_eq!(
        stored_csrf(&fixture.pool, &anonymous, None).await,
        anonymous_csrf
    );
    assert_eq!(
        stored_csrf(&fixture.pool, &member, Some(user.id)).await,
        member_csrf
    );
    writer.rollback().await.unwrap();

    // An expired token is replaced through the writer, then answers as current again.
    sqlx::query("UPDATE sessions SET csrf_expires_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 minute') WHERE id=?1")
        .bind(&member)
        .execute(&fixture.raw)
        .await
        .unwrap();
    let replaced = stored_csrf(&fixture.pool, &member, Some(user.id)).await;
    assert_ne!(replaced, member_csrf);
    assert_eq!(
        stored_csrf(&fixture.pool, &member, Some(user.id)).await,
        replaced
    );

    // A revoked session has no token, and that refusal does not wait for the writer.
    sessions::delete_session(&fixture.pool, &anonymous)
        .await
        .unwrap();
    sessions::delete_session(&fixture.pool, &member)
        .await
        .unwrap();
    let writer = fixture.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert_eq!(
        sessions::get_or_create_csrf_token(&fixture.pool, &anonymous, None, &token(), 7200)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        sessions::get_or_create_csrf_token(&fixture.pool, &member, Some(user.id), &token(), 7200)
            .await
            .unwrap(),
        None
    );
    writer.rollback().await.unwrap();
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_csrf_reads_do_not_wait_for_session_row_locks() {
    let fixture = PostgresDatabase::new().await;
    let user = setup(&fixture.pool, "public").await;
    let anonymous = token();
    let member = token();
    sessions::create_browser_session(
        &fixture.pool,
        &anonymous,
        None,
        None,
        Utc::now() + Duration::days(2),
    )
    .await
    .unwrap();
    sessions::rotate_authenticated_session(
        &fixture.pool,
        None,
        &member,
        user.id,
        Some("fixture-hash"),
        None,
        Utc::now() + Duration::days(2),
    )
    .await
    .unwrap()
    .unwrap();
    let anonymous_csrf = stored_csrf(&fixture.pool, &anonymous, None).await;
    let member_csrf = stored_csrf(&fixture.pool, &member, Some(user.id)).await;

    // Another connection locks both session rows. A current token is a plain read, so it
    // still answers; the locked path would wait here until the lock is released.
    let mut connection = PgConnection::connect(&fixture.connection_url())
        .await
        .unwrap();
    let mut locks = connection.begin().await.unwrap();
    sqlx::query("SELECT id FROM anonymous_sessions WHERE id=$1 FOR UPDATE")
        .bind(&anonymous)
        .fetch_one(&mut *locks)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE")
        .bind(&member)
        .fetch_one(&mut *locks)
        .await
        .unwrap();
    let within = std::time::Duration::from_secs(5);
    let answered = tokio::time::timeout(
        within,
        sessions::get_or_create_csrf_token(&fixture.pool, &anonymous, None, &token(), 7200),
    )
    .await
    .expect("a current token must not wait for the session row lock")
    .unwrap();
    assert_eq!(answered, Some(anonymous_csrf));
    let answered = tokio::time::timeout(
        within,
        sessions::get_or_create_csrf_token(&fixture.pool, &member, Some(user.id), &token(), 7200),
    )
    .await
    .expect("a current token must not wait for the session row lock")
    .unwrap();
    assert_eq!(answered, Some(member_csrf.clone()));
    locks.rollback().await.unwrap();

    sqlx::query(
        "UPDATE sessions SET csrf_expires_at=clock_timestamp()-INTERVAL '1 minute' WHERE id=$1",
    )
    .bind(&member)
    .execute(&mut connection)
    .await
    .unwrap();
    let replaced = stored_csrf(&fixture.pool, &member, Some(user.id)).await;
    assert_ne!(replaced, member_csrf);
    assert_eq!(
        stored_csrf(&fixture.pool, &member, Some(user.id)).await,
        replaced
    );

    sessions::delete_session(&fixture.pool, &anonymous)
        .await
        .unwrap();
    sessions::delete_session(&fixture.pool, &member)
        .await
        .unwrap();
    assert_eq!(
        sessions::get_or_create_csrf_token(&fixture.pool, &anonymous, None, &token(), 7200)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        sessions::get_or_create_csrf_token(&fixture.pool, &member, Some(user.id), &token(), 7200)
            .await
            .unwrap(),
        None
    );
    connection.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_pr_numbers_rollback_and_exact_future_microsecond_revision_cas() {
    let fixture = NativeDatabase::new().await;
    let user = setup(&fixture.pool, "public").await;
    let repo = repository(&fixture.pool, user.id, "numbered").await;
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..20 {
        let pool = fixture.pool.clone();
        let actor = user.id;
        let repo = repo.id;
        tasks.spawn(async move {
            if index % 2 == 0 {
                db::issues::create_issue(&pool, repo, actor, "issue", "")
                    .await
                    .unwrap()
                    .number
            } else {
                new_pr(&pool, repo, actor, &format!("feature-{index}"))
                    .await
                    .number
            }
        });
    }
    let mut numbers = vec![];
    while let Some(result) = tasks.join_next().await {
        numbers.push(result.unwrap());
    }
    numbers.sort_unstable();
    assert_eq!(numbers, (1..=20).collect::<Vec<_>>());
    assert!(
        db::issues::create_issue(&fixture.pool, repo.id, -1, "invalid", "")
            .await
            .is_err()
    );
    let issue = db::issues::create_issue(&fixture.pool, repo.id, user.id, "before", "body")
        .await
        .unwrap();
    assert_eq!(issue.number, 21);
    let previous = DateTime::parse_from_rfc3339("2099-01-02T03:04:05.123456Z")
        .unwrap()
        .with_timezone(&Utc);
    // Noncanonical but exact SQL-written representation must compare by decoded
    // microseconds and CAS the original raw string, not rounded julianday.
    sqlx::query("UPDATE issues SET updated_at=?2 WHERE id=?1")
        .bind(issue.id)
        .bind("2099-01-02 03:04:05.123456")
        .execute(&fixture.raw)
        .await
        .unwrap();
    let loaded = db::issues::get_issue_by_id(&fixture.pool, issue.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.updated_at, previous);
    let edit = || DiscussionEdit {
        actor_id: user.id,
        expected_state: "open",
        expected_updated: previous,
        title: Some("winner"),
        body: Some("same transaction"),
        state: Some("closed"),
    };
    let (a, b) = tokio::join!(
        db::issues::update_issue(&fixture.pool, issue.id, edit()),
        db::issues::update_issue(&fixture.pool, issue.id, edit())
    );
    assert_eq!(
        usize::from(a.as_ref().unwrap().is_some()) + usize::from(b.as_ref().unwrap().is_some()),
        1
    );
    let winner = a.unwrap().or(b.unwrap()).unwrap();
    assert_eq!(winner.updated_at, previous + Duration::microseconds(1));
    assert_eq!(winner.closed_by_id, Some(user.id));
    let closed = winner.closed_at;
    assert!(db::issues::update_issue(&fixture.pool, issue.id, edit())
        .await
        .unwrap()
        .is_none());
    db::issues::add_issue_comment(
        &fixture.pool,
        issue.id,
        user.id,
        "comment bumps revision monotonically",
    )
    .await
    .unwrap();
    let revised = db::issues::get_issue_by_id(&fixture.pool, issue.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        revised.updated_at,
        winner.updated_at + Duration::microseconds(1)
    );
    assert_eq!(revised.closed_at, closed);
    let stranger = db::users::accounts::create_user(&fixture.pool, "stranger", None)
        .await
        .unwrap();
    assert!(db::issues::update_issue(
        &fixture.pool,
        issue.id,
        DiscussionEdit {
            actor_id: stranger.id,
            expected_state: "closed",
            expected_updated: revised.updated_at,
            title: Some("forbidden"),
            body: None,
            state: None
        }
    )
    .await
    .unwrap()
    .is_none());
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_quota_is_integer_atomic_and_charged_per_reservation() {
    let fixture = NativeDatabase::new().await;
    let user = setup(&fixture.pool, "public").await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let pool = fixture.pool.clone();
        let id = user.id;
        tasks.spawn(async move {
            files::reserve_upload(&pool, upload(id, FileReason::Other, FileVisibility::Owner)).await
        });
    }
    let mut admitted = vec![];
    let mut denied = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(file) => admitted.push(file),
            Err(AdmissionError::Quota) => denied += 1,
            Err(error) => panic!("unexpected upload admission: {error}"),
        }
    }
    assert_eq!(admitted.len(), 5);
    assert_eq!(denied, 3);

    // A reservation is a pending record that expires after 15 minutes.
    let file = &admitted[0];
    assert_eq!(file.object_key, format!("uploads/{}", file.id));
    assert_eq!(file.user_id, Some(user.id));
    assert_eq!(file.reason.as_deref(), Some("other"));
    assert_eq!(file.visibility, "owner");
    assert!(!file.is_active && file.confirmed_at.is_none());
    let window = file.upload_expires_at.unwrap() - Utc::now();
    assert!(window > Duration::minutes(14) && window <= Duration::minutes(15));

    // Deleting a reservation or letting it expire does not refund the quota.
    let full = || upload(user.id, FileReason::Other, FileVisibility::Owner);
    let files::DeleteFileOutcome::Deleted(removed) =
        files::delete_file(&fixture.pool, file.id, user.id)
            .await
            .unwrap()
    else {
        panic!("owner did not delete the reservation")
    };
    assert_eq!(removed.id, file.id);
    assert!(matches!(
        files::reserve_upload(&fixture.pool, full()).await,
        Err(AdmissionError::Quota)
    ));
    sqlx::query("UPDATE files SET upload_expires_at=?1")
        .bind(timestamp(Utc::now() - Duration::minutes(1)))
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert!(matches!(
        files::reserve_upload(&fixture.pool, full()).await,
        Err(AdmissionError::Quota)
    ));
    // Each reason has its own budget.
    files::reserve_upload(
        &fixture.pool,
        upload(user.id, FileReason::Avatar, FileVisibility::Public),
    )
    .await
    .unwrap();
    // Charges leave the window as time passes.
    sqlx::query("UPDATE upload_rate_limit SET uploaded_at=?1")
        .bind(timestamp(Utc::now() - Duration::days(2)))
        .execute(&fixture.raw)
        .await
        .unwrap();
    files::reserve_upload(&fixture.pool, full()).await.unwrap();

    // Declarations the policy refuses are never recorded.
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_rate_limit")
        .fetch_one(&fixture.raw)
        .await
        .unwrap();
    let refused = [
        (
            "empty file",
            ReserveUpload {
                file_size: 0,
                ..full()
            },
        ),
        (
            "larger than any upload",
            ReserveUpload {
                file_size: files::MAX_UPLOAD_BYTES + 1,
                ..full()
            },
        ),
        (
            "NUL in the name",
            ReserveUpload {
                filename: "a\0b",
                ..full()
            },
        ),
        (
            "short checksum",
            ReserveUpload {
                sha256_hex: "abc",
                ..full()
            },
        ),
        (
            "public file that is not an avatar",
            ReserveUpload {
                visibility: FileVisibility::Public,
                ..full()
            },
        ),
        (
            "avatar that is not an image",
            ReserveUpload {
                content_type: "text/plain",
                ..upload(user.id, FileReason::Avatar, FileVisibility::Public)
            },
        ),
        (
            "unknown user",
            ReserveUpload {
                user_id: user.id + 1000,
                ..full()
            },
        ),
    ];
    for (name, declaration) in refused {
        assert!(
            matches!(
                files::reserve_upload(&fixture.pool, declaration).await,
                Err(AdmissionError::Invalid)
            ),
            "{name}"
        );
    }
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_rate_limit")
        .fetch_one(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(before, after);

    // Sums that do not fit in 64 bits are an error, not a wrapped or REAL total.
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO upload_rate_limit(user_id,file_size,reason) VALUES (?1,?2,'other')",
        )
        .bind(user.id)
        .bind(i64::MAX)
        .execute(&fixture.raw)
        .await
        .unwrap();
    }
    assert!(matches!(
        files::reserve_upload(&fixture.pool, full()).await,
        Err(AdmissionError::Database(_))
    ));
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_activation_is_atomic_happens_once_and_survives_reopen() {
    let mut fixture = NativeDatabase::new().await;
    let user = setup(&fixture.pool, "public").await;
    let other = db::users::accounts::create_user(&fixture.pool, "other", None)
        .await
        .unwrap();
    let file = files::reserve_upload(
        &fixture.pool,
        upload(user.id, FileReason::Avatar, FileVisibility::Public),
    )
    .await
    .unwrap();

    // A pending upload is not a file yet, and only its owner can activate it.
    for actor in [None, Some(user.id)] {
        assert!(files::get_downloadable_file(&fixture.pool, file.id, actor)
            .await
            .unwrap()
            .is_none());
    }
    assert!(files::get_user_file(&fixture.pool, file.id, user.id)
        .await
        .unwrap()
        .is_none());
    assert!(!files::activate_upload(&fixture.pool, file.id, other.id)
        .await
        .unwrap());

    // A failure while activating leaves the upload exactly as it was.
    sqlx::raw_sql("CREATE TRIGGER deny_activation BEFORE UPDATE ON files WHEN NEW.is_active BEGIN SELECT RAISE(ABORT,'fixture activation failure'); END;").execute(&fixture.raw).await.unwrap();
    assert!(files::activate_upload(&fixture.pool, file.id, user.id)
        .await
        .is_err());
    let pending = files::get_file_by_id(&fixture.pool, file.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!pending.is_active && pending.confirmed_at.is_none());
    assert!(pending.upload_expires_at.is_some());
    sqlx::query("DROP TRIGGER deny_activation")
        .execute(&fixture.raw)
        .await
        .unwrap();

    // Connections racing to activate: exactly one wins.
    let second = fixture.second_handle().await;
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..8 {
        let pool = if index % 2 == 0 {
            fixture.pool.clone()
        } else {
            second.clone()
        };
        let barrier = barrier.clone();
        let (id, owner) = (file.id, user.id);
        tasks.spawn(async move {
            barrier.wait().await;
            files::activate_upload(&pool, id, owner).await.unwrap()
        });
    }
    let mut winners = 0;
    while let Some(result) = tasks.join_next().await {
        winners += usize::from(result.unwrap());
    }
    assert_eq!(winners, 1);
    second.close().await;
    assert!(!files::activate_upload(&fixture.pool, file.id, user.id)
        .await
        .unwrap());
    let active = files::get_file_by_id(&fixture.pool, file.id)
        .await
        .unwrap()
        .unwrap();
    assert!(active.is_active && active.confirmed_at.is_some());
    assert!(active.upload_expires_at.is_none());
    assert!(
        files::get_downloadable_file(&fixture.pool, file.id, None)
            .await
            .unwrap()
            .is_some(),
        "a public avatar is visible to everyone"
    );
    assert!(files::get_user_file(&fixture.pool, file.id, user.id)
        .await
        .unwrap()
        .is_some());
    assert!(files::get_user_file(&fixture.pool, file.id, other.id)
        .await
        .unwrap()
        .is_none());

    // An upload whose window has ended cannot be activated and can be swept.
    let stale = files::reserve_upload(
        &fixture.pool,
        upload(user.id, FileReason::Other, FileVisibility::Owner),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE files SET upload_expires_at=?2 WHERE id=?1")
        .bind(stale.id)
        .bind(timestamp(Utc::now() - Duration::seconds(1)))
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert!(!files::activate_upload(&fixture.pool, stale.id, user.id)
        .await
        .unwrap());
    let expired = files::expired_uploads(&fixture.pool, 10).await.unwrap();
    assert_eq!(
        expired.iter().map(|file| file.id).collect::<Vec<_>>(),
        [stale.id]
    );
    assert!(
        files::delete_expired_upload(&fixture.pool, file.id)
            .await
            .unwrap()
            .is_none(),
        "an active file is never swept"
    );
    assert!(files::delete_expired_upload(&fixture.pool, stale.id)
        .await
        .unwrap()
        .is_some());
    assert!(files::delete_expired_upload(&fixture.pool, stale.id)
        .await
        .unwrap()
        .is_none());

    files::record_download(
        &fixture.pool,
        user.id,
        file.id,
        Some("avatar"),
        Some("2001:db8::4".parse().unwrap()),
    )
    .await
    .unwrap();
    let ip: String =
        sqlx::query_scalar("SELECT ip_address FROM download_rate_limit WHERE file_id=?1")
            .bind(file.id)
            .fetch_one(&fixture.raw)
            .await
            .unwrap();
    assert_eq!(ip, "2001:db8::4/128");

    fixture.reopen().await;
    let reopened = files::get_file_by_id(&fixture.pool, file.id)
        .await
        .unwrap()
        .unwrap();
    assert!(reopened.is_active);
    assert_eq!(reopened.object_key, file.object_key);
    let stats = files::get_user_file_stats(&fixture.pool, user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.total_files, 1);
    assert_eq!(stats.total_bytes, 10);
    assert_eq!(stats.avatar_count, 1);
    assert_eq!(stats.uploads_24h, 2, "swept uploads stay charged");
    assert_eq!(stats.bytes_24h, 20);
    assert_eq!(stats.downloads_24h, 1);

    // Only the owner can delete, and the record comes back for object cleanup.
    assert!(matches!(
        files::delete_file(&fixture.pool, file.id, other.id)
            .await
            .unwrap(),
        files::DeleteFileOutcome::NotFound
    ));
    let files::DeleteFileOutcome::Deleted(removed) =
        files::delete_file(&fixture.pool, file.id, user.id)
            .await
            .unwrap()
    else {
        panic!("owner did not delete the file")
    };
    assert_eq!(removed.object_key, file.object_key);
    assert!(files::get_file_by_id(&fixture.pool, file.id)
        .await
        .unwrap()
        .is_none());
    fixture.close().await;
}

async fn execute_sql(pool: &db::DbPool, sql: &str) {
    match pool {
        db::DbPool::Postgres(raw) => sqlx::raw_sql(sql)
            .execute(raw)
            .await
            .unwrap()
            .rows_affected(),
        db::DbPool::Sqlite(raw) => sqlx::raw_sql(sql)
            .execute(raw)
            .await
            .unwrap()
            .rows_affected(),
    };
}

async fn assert_indexed_plan(pool: &db::DbPool, sql: &str, indexes: &[&str]) {
    let plan = match pool {
        db::DbPool::Sqlite(raw) => {
            // EXPLAIN QUERY PLAN does not reload the schema; refresh this pooled connection first.
            let mut conn = raw.acquire().await.unwrap();
            sqlx::query("SELECT 1 FROM sqlite_schema LIMIT 1")
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}"))
                .fetch_all(&mut *conn)
                .await
                .unwrap()
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        db::DbPool::Postgres(raw) => {
            let mut tx = raw.begin().await.unwrap();
            // Small fixtures otherwise prefer sequential scans. Check that the
            // ordered/foreign-key index path is available without a sort.
            sqlx::raw_sql("SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off;")
                .execute(&mut *tx)
                .await
                .unwrap();
            let lines: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN (COSTS OFF) {sql}"))
                .fetch_all(&mut *tx)
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            lines.join("\n")
        }
    };
    assert!(
        !plan.contains("TEMP B-TREE") && !plan.contains("Sort"),
        "{plan}"
    );
    for index in indexes {
        assert!(plan.contains(index), "missing {index}: {plan}");
    }
}

async fn assert_watch_count(pool: &db::DbPool, repo_id: i64, expected: i32) {
    assert_eq!(
        repositories::records::get_repository_by_id(pool, repo_id)
            .await
            .unwrap()
            .unwrap()
            .watchers_count,
        expected
    );
}

async fn watch_count_transitions(pool: &db::DbPool) {
    let owner = setup(pool, "public").await;
    let ignored = db::users::accounts::create_user(pool, "ignored", None)
        .await
        .unwrap();
    let releases = db::users::accounts::create_user(pool, "releases", None)
        .await
        .unwrap();
    let repo = repository(pool, owner.id, "watched").await;
    let empty = repository(pool, owner.id, "empty").await;
    // A repository with no active watches is covered by the empty repository below.
    execute_sql(pool, &format!(
        "INSERT INTO watches(user_id,repo_id,level) VALUES ({},{},'watching'),({},{},'releases_only'),({},{},'ignoring');
         INSERT INTO watches(user_id,repo_id,level) VALUES ({},{},'ignoring');",
        owner.id, repo.id, releases.id, repo.id, ignored.id, repo.id,
        owner.id, empty.id
    )).await;
    assert_watch_count(pool, repo.id, 2).await;
    assert_watch_count(pool, empty.id, 0).await;
    assert!(
        !repositories::engagement::is_repo_watched(pool, ignored.id, repo.id)
            .await
            .unwrap()
    );
    assert!(
        repositories::engagement::is_repo_watched(pool, releases.id, repo.id)
            .await
            .unwrap()
    );

    assert!(
        repositories::engagement::watch_repo(pool, ignored.id, repo.id)
            .await
            .unwrap()
    );
    assert_watch_count(pool, repo.id, 3).await;
    assert!(
        !repositories::engagement::watch_repo(pool, ignored.id, repo.id)
            .await
            .unwrap()
    );
    assert_watch_count(pool, repo.id, 3).await;
    assert!(
        repositories::engagement::watch_repo(pool, releases.id, repo.id)
            .await
            .unwrap()
    );
    assert_watch_count(pool, repo.id, 3).await;
    execute_sql(
        pool,
        &format!(
            "UPDATE watches SET level='ignoring' WHERE user_id={} AND repo_id={}",
            owner.id, repo.id
        ),
    )
    .await;
    assert_watch_count(pool, repo.id, 2).await;
    assert!(
        repositories::engagement::unwatch_repo(pool, owner.id, repo.id)
            .await
            .unwrap()
    );
    assert_watch_count(pool, repo.id, 2).await;
    assert!(
        repositories::engagement::unwatch_repo(pool, ignored.id, repo.id)
            .await
            .unwrap()
    );
    assert_watch_count(pool, repo.id, 1).await;
    assert!(
        repositories::engagement::unwatch_repo(pool, releases.id, repo.id)
            .await
            .unwrap()
    );
    assert_watch_count(pool, repo.id, 0).await;
    execute_sql(
        pool,
        &format!(
            "INSERT INTO watches(user_id,repo_id,level) VALUES ({},{},'ignoring')",
            ignored.id, repo.id
        ),
    )
    .await;
    assert_watch_count(pool, repo.id, 0).await;
    assert!(
        repositories::engagement::unwatch_repo(pool, ignored.id, repo.id)
            .await
            .unwrap()
    );
    assert_watch_count(pool, repo.id, 0).await;
    execute_sql(
        pool,
        &format!(
            "INSERT INTO watches(user_id,repo_id,level) VALUES ({},{},'releases_only')",
            releases.id, repo.id
        ),
    )
    .await;
    assert_watch_count(pool, repo.id, 1).await;
    // A row moved to another repository must update both counters.
    execute_sql(
        pool,
        &format!(
            "UPDATE watches SET repo_id={} WHERE repo_id={}",
            empty.id, repo.id
        ),
    )
    .await;
    assert_watch_count(pool, repo.id, 0).await;
    assert_watch_count(pool, empty.id, 1).await;
    assert!(
        repositories::engagement::unwatch_repo(pool, releases.id, empty.id)
            .await
            .unwrap()
    );
    assert_watch_count(pool, empty.id, 0).await;
}

#[tokio::test]
async fn sqlite_watch_count_transitions() {
    let fixture = NativeDatabase::new().await;
    watch_count_transitions(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_watch_count_transitions() {
    let fixture = PostgresDatabase::new().await;
    watch_count_transitions(&fixture.pool).await;
}

#[tokio::test]
async fn postgres_watch_change_and_repository_deletion_do_not_deadlock() {
    let fixture = PostgresDatabase::new().await;
    let pool = &fixture.pool;
    let owner = setup(pool, "public").await;
    for round in 0..40 {
        let watcher = db::users::accounts::create_user(pool, &format!("racer{round}"), None)
            .await
            .unwrap();
        let repo = repository(pool, owner.id, &format!("race{round}")).await;
        execute_sql(
            pool,
            &format!(
                "INSERT INTO watches(user_id,repo_id,level) VALUES ({},{},'ignoring')",
                watcher.id, repo.id
            ),
        )
        .await;
        // Moving the ignoring row to watching runs the counter trigger on the repository.
        // Deletion locks the repository before the watch rows it cascades to, so the watch
        // change must lock the repository first too, or the two can deadlock.
        let (watched, deleted) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            tokio::join!(
                repositories::engagement::watch_repo(pool, watcher.id, repo.id),
                repositories::records::delete_repository(pool, owner.id, repo.id),
            )
        })
        .await
        .expect("watch change and repository deletion must not wait on each other");
        watched.expect("watch change must not fail");
        deleted.expect("repository deletion must not fail");
        assert!(repositories::records::get_repository_by_id(pool, repo.id)
            .await
            .unwrap()
            .is_none());
    }
}

/// A ban takes the membership lock, then the repository rows of an organization it leaves without
/// an owner. A milestone write must not hold a repository row while it waits for that lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_ban_and_milestone_write_do_not_deadlock() {
    let fixture = PostgresDatabase::new().await;
    let pool = &fixture.pool;
    let admin = setup(pool, "public").await;
    for round in 0..40 {
        let owner = db::users::accounts::create_user(pool, &format!("sole_owner{round}"), None)
            .await
            .unwrap();
        let writer = db::users::accounts::create_user(pool, &format!("writer{round}"), None)
            .await
            .unwrap();
        let organization = db::organizations::create_organization(
            pool,
            owner.id,
            &db::organizations::NewOrganization::new(
                &format!("ban_org_{round}"),
                db::organizations::OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let db::organizations::OrganizationOutcome::Applied(organization) = organization else {
            panic!("organization was not created");
        };
        let repo = db::repositories::creation::insert_repository(
            pool,
            db::repositories::creation::NewRepository {
                owner: db::repositories::creation::RepositoryCreationOwner {
                    actor_id: owner.id,
                    target: db::repositories::creation::RepositoryTarget::Organization(
                        organization.id,
                    ),
                },
                name: "proj",
                description: None,
                is_private: false,
                disk_path: "/owned/native-fixture.git",
                storage_node_id: None,
                source_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        repositories::access::grant_repo_access(
            pool,
            writer.id,
            repo.id,
            db::models::AccessLevel::Write,
        )
        .await
        .unwrap();
        let (banned, milestone) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            tokio::join!(
                transitions::change_ban(pool, admin.id, owner.id, true),
                db::milestones::create_milestone(pool, writer.id, repo.id, "v1", None, None),
            )
        })
        .await
        .expect("a ban and a milestone write must not wait on each other");
        assert!(matches!(banned.unwrap(), BanChange::Updated(_)));
        // The write lands before the ban, or is refused once the ban has committed.
        if let Err(error) = milestone {
            assert_eq!(
                error.downcast_ref::<db::milestones::MilestoneWriteRefusal>(),
                Some(&db::milestones::MilestoneWriteRefusal::Denied),
                "{error:#}"
            );
        }
    }
}

/// Editing or deleting a comment takes repository authority before the comment row, the order a
/// repository deletion uses for the comments it removes. Taking them the other way round deadlocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_comment_changes_and_repository_deletion_do_not_deadlock() {
    let fixture = PostgresDatabase::new().await;
    let pool = &fixture.pool;
    let owner = setup(pool, "public").await;
    for round in 0..40 {
        let repo = repository(pool, owner.id, &format!("comments{round}")).await;
        let issue = db::issues::create_issue(pool, repo.id, owner.id, "Issue", "")
            .await
            .unwrap();
        let comment = db::issues::add_issue_comment(pool, issue.id, owner.id, "first")
            .await
            .unwrap();
        let pr = new_pr(pool, repo.id, owner.id, &format!("comments{round}")).await;
        let pr_comment = db::pull_requests::reviews::add_pull_request_comment(
            pool,
            db::pull_requests::reviews::NewPullRequestComment {
                pr_id: pr.id,
                author_id: owner.id,
                body: "first",
                path: None,
                line: None,
                side: None,
                commit_sha: None,
                in_reply_to_comment_id: None,
            },
        )
        .await
        .unwrap();
        let (edited, deleted_comment, deleted) =
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                tokio::join!(
                    db::issues::edit_issue_comment(
                        pool, owner.id, repo.id, issue.id, comment.id, "second"
                    ),
                    db::pull_requests::reviews::delete_pull_request_comment(
                        pool,
                        owner.id,
                        repo.id,
                        pr.id,
                        pr_comment.id
                    ),
                    repositories::records::delete_repository(pool, owner.id, repo.id),
                )
            })
            .await
            .expect("comment changes and repository deletion must not wait on each other");
        edited.expect("comment edit must not fail");
        deleted_comment.expect("comment delete must not fail");
        deleted.expect("repository deletion must not fail");
    }
}

async fn accounting_and_explore_indexes(pool: &db::DbPool) {
    let owner = setup(pool, "public").await;
    let popular = repository(pool, owner.id, "popular").await;
    let newer = repository(pool, owner.id, "newer").await;
    let private = repository(pool, owner.id, "private").await;
    execute_sql(pool, &format!(
        "UPDATE repositories SET is_private=false, stars_count=9, created_at='2020-01-01T00:00:00.000000Z' WHERE id={};
         UPDATE repositories SET is_private=false, stars_count=1, created_at='2030-01-01T00:00:00.000000Z' WHERE id={};
         UPDATE repositories SET stars_count=100 WHERE id={};",
        popular.id, newer.id, private.id
    )).await;
    let expired = files::reserve_upload(
        pool,
        upload(owner.id, FileReason::Other, FileVisibility::Owner),
    )
    .await
    .unwrap();
    let kept = files::reserve_upload(
        pool,
        upload(owner.id, FileReason::Avatar, FileVisibility::Public),
    )
    .await
    .unwrap();
    files::record_download(pool, owner.id, kept.id, Some("avatar"), None)
        .await
        .unwrap();
    execute_sql(
        pool,
        "UPDATE files SET upload_expires_at='2020-01-01T00:00:00.000000Z' WHERE reason='other'",
    )
    .await;

    for table in ["upload_rate_limit", "download_rate_limit"] {
        // SQLite stores file ids as 16-byte blobs. A text literal would apply text affinity
        // and skip the index, so compare with a blob literal as the application does.
        let sql = match pool {
            db::DbPool::Postgres(_) => {
                format!("SELECT * FROM {table} WHERE file_id='{}'", expired.id)
            }
            db::DbPool::Sqlite(_) => {
                format!(
                    "SELECT * FROM {table} WHERE file_id=X'{}'",
                    expired.id.simple()
                )
            }
        };
        assert_indexed_plan(pool, &sql, &[&format!("idx_{table}_file_id")]).await;
    }
    let explore_sql = match pool {
        db::DbPool::Postgres(_) => "SELECT * FROM repositories WHERE is_private = false ORDER BY stars_count DESC, created_at DESC, id DESC LIMIT 1",
        db::DbPool::Sqlite(_) => "SELECT * FROM repositories INDEXED BY idx_repositories_public_stars WHERE is_private = false ORDER BY stars_count DESC, created_at DESC, id DESC LIMIT 1",
    };
    assert_indexed_plan(pool, explore_sql, &["idx_repositories_public_stars"]).await;
    let ranked = repositories::records::list_public_repositories(
        pool,
        repositories::records::RepositorySort::Stars,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        ranked.iter().map(|repo| repo.id).collect::<Vec<_>>(),
        [popular.id]
    );
    assert!(files::delete_expired_upload(pool, expired.id)
        .await
        .unwrap()
        .is_some());
    let stats = files::get_user_file_stats(pool, owner.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.uploads_24h, 2, "deletion retains upload charges");
    assert_eq!(stats.downloads_24h, 1, "unrelated downloads are retained");
    assert!(files::get_file_by_id(pool, kept.id)
        .await
        .unwrap()
        .is_some());
    assert!(matches!(
        files::delete_file(pool, kept.id, owner.id).await.unwrap(),
        files::DeleteFileOutcome::Deleted(_)
    ));
    let stats = files::get_user_file_stats(pool, owner.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.uploads_24h, 2);
    assert_eq!(
        stats.downloads_24h, 0,
        "the deleted file's downloads cascade"
    );
}

#[tokio::test]
async fn sqlite_accounting_and_explore_use_indexes() {
    let fixture = NativeDatabase::new().await;
    accounting_and_explore_indexes(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_accounting_and_explore_use_indexes() {
    let fixture = PostgresDatabase::new().await;
    accounting_and_explore_indexes(&fixture.pool).await;
}

#[tokio::test]
async fn sqlite_search_uses_ordered_scans_and_keeps_private_rows_hidden() {
    let fixture = NativeDatabase::new().await;
    search_uses_ordered_scans_and_keeps_private_rows_hidden(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_search_uses_ordered_scans_and_keeps_private_rows_hidden() {
    let fixture = PostgresDatabase::new().await;
    search_uses_ordered_scans_and_keeps_private_rows_hidden(&fixture.pool).await;
}

async fn search_uses_ordered_scans_and_keeps_private_rows_hidden(pool: &db::DbPool) {
    let owner = setup(pool, "public").await;
    let public = repository(pool, owner.id, "public-needle").await;
    let private = repository(pool, owner.id, "private-needle").await;
    execute_sql(
        pool,
        &format!(
            "UPDATE repositories SET is_private=false WHERE id={};
        UPDATE repositories SET stars_count=100 WHERE id={};",
            public.id, private.id
        ),
    )
    .await;
    let visible = db::issues::create_issue(pool, public.id, owner.id, "needle", "")
        .await
        .unwrap();
    let hidden = db::issues::create_issue(pool, private.id, owner.id, "needle", "")
        .await
        .unwrap();
    execute_sql(
        pool,
        &format!(
            "UPDATE issues SET updated_at='2099-01-01T00:00:00.000000Z' WHERE id={}",
            hidden.id
        ),
    )
    .await;
    // INDEXED BY is a SQLite hint. Postgres is checked on each ordered scan alone, because its
    // planner may choose a partial index or a hash join around the private filter and the join.
    let (repositories_sql, issues_sql) = match pool {
        db::DbPool::Postgres(_) => (
            "SELECT * FROM repositories ORDER BY stars_count DESC,updated_at DESC",
            "SELECT * FROM issues ORDER BY updated_at DESC",
        ),
        db::DbPool::Sqlite(_) => (
            "SELECT * FROM repositories INDEXED BY idx_repositories_search_order WHERE NOT is_private ORDER BY stars_count DESC,updated_at DESC",
            "SELECT i.* FROM issues i INDEXED BY idx_issues_updated_at CROSS JOIN repositories r ON r.id=i.repo_id WHERE NOT r.is_private ORDER BY i.updated_at DESC",
        ),
    };
    for (sql, index) in [
        (repositories_sql, "idx_repositories_search_order"),
        (issues_sql, "idx_issues_updated_at"),
    ] {
        assert_indexed_plan(pool, sql, &[index]).await;
    }
    let repos = repositories::discovery::search_repositories(
        pool,
        None,
        "needle",
        repositories::records::RepositorySort::Stars,
        0,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        repos.iter().map(|repo| repo.id).collect::<Vec<_>>(),
        [public.id]
    );
    let issues = repositories::discovery::search_issues(
        pool,
        None,
        "needle",
        repositories::discovery::ItemSort::Updated,
        0,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        issues.iter().map(|issue| issue.id).collect::<Vec<_>>(),
        [visible.id]
    );
}

async fn default_branch_accepts_full_ref_length(pool: &db::DbPool) {
    let owner = db::users::accounts::create_user(pool, "branch-migration", None)
        .await
        .unwrap();
    let repo = repository(pool, owner.id, "kept").await;
    repositories::records::update_repo_default_branch_and_size(pool, repo.id, "release/stable", 17)
        .await
        .unwrap();
    repositories::engagement::star_repo(pool, owner.id, repo.id)
        .await
        .unwrap();
    let branch = format!(
        "{}{}",
        format!("{}/", "a".repeat(126)).repeat(7),
        "b".repeat(124)
    );
    assert_eq!(format!("refs/heads/{branch}").len(), 1024);
    repositories::records::update_repo_default_branch_and_size(pool, repo.id, &branch, 88)
        .await
        .unwrap();
    let stored = repositories::records::get_repository_by_id(pool, repo.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.default_branch, branch);
    assert_eq!(stored.size_bytes, 88);
    assert_eq!(stored.stars_count, 1);
    assert!(
        repositories::engagement::is_repo_starred(pool, owner.id, repo.id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sqlite_default_branch_accepts_full_ref_length() {
    let fixture = NativeDatabase::new().await;
    default_branch_accepts_full_ref_length(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_default_branch_accepts_full_ref_length() {
    let fixture = PostgresDatabase::new().await;
    default_branch_accepts_full_ref_length(&fixture.pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_schema_history_foreign_keys_and_integer_overflow_fail_closed() {
    let fixture = NativeDatabase::new().await;
    let (a, b) = tokio::join!(
        db::run_migrations(&fixture.pool),
        db::run_migrations(&fixture.pool)
    );
    a.unwrap();
    b.unwrap();
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(journal, "wal");
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(foreign_keys, 1);
    let migrations: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _gitserv_sqlite_migrations ORDER BY version")
            .fetch_all(&fixture.raw)
            .await
            .unwrap();
    assert_eq!(migrations, [1]);
    let pending_table: String = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='pending_object_deletions'",
    )
    .fetch_one(&fixture.raw)
    .await
    .unwrap();
    assert_eq!(pending_table, "pending_object_deletions");
    assert!(sqlx::query("INSERT INTO sessions(id,user_id,expires_at) VALUES ('invalid',-1,'2099-01-01T00:00:00.000000Z')").execute(&fixture.raw).await.is_err());
    let user = setup(&fixture.pool, "public").await;
    for _ in 0..2 {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO files(id,user_id,filename,original_name,content_type,file_size,sha256_hex,object_key,is_active) VALUES (?1,?2,'big','big','text/plain',?3,?4,?5,1)").bind(id).bind(user.id).bind(i64::MAX).bind("a".repeat(64)).bind(format!("uploads/{id}")).execute(&fixture.raw).await.unwrap();
    }
    assert!(
        files::get_user_total_storage(&fixture.pool, user.id)
            .await
            .is_err(),
        "SUM cannot saturate or use REAL accounting"
    );
    sqlx::query("UPDATE _gitserv_sqlite_migrations SET sha256='tampered' WHERE version=1")
        .execute(&fixture.raw)
        .await
        .unwrap();
    assert!(db::run_migrations(&fixture.pool).await.is_err());
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        1
    );
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_current_ledger_skips_foreign_key_scan() {
    let fixture = NativeDatabase::new().await;
    let mut connection = fixture.raw.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions(id,user_id,expires_at) VALUES ('orphan',-1,'2099-01-01T00:00:00.000000Z')")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);

    // A current ledger needs no database-wide scan on ordinary CLI startup.
    db::run_migrations(&fixture.pool).await.unwrap();
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&fixture.raw)
        .await
        .unwrap();
    assert_eq!(violations.len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_baseline_orphan_row_fails_and_rolls_back() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}",
        directory.path().join("orphan.sqlite").display()
    );
    let pool = db::init_db(&url).await.unwrap();
    let db::DbPool::Sqlite(raw) = &pool else {
        panic!("SQLite fixture");
    };
    // A table outside the schema holds an orphan row. The baseline applies cleanly over it,
    // so the foreign key scan after it must refuse the migration and roll it back.
    let mut connection = raw.acquire().await.unwrap();
    sqlx::raw_sql(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE stray(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES stray(id));
         INSERT INTO stray(id, parent) VALUES (1, 99);
         PRAGMA foreign_keys = ON;",
    )
    .execute(&mut *connection)
    .await
    .unwrap();
    drop(connection);
    let error = db::run_migrations(&pool).await.unwrap_err();
    assert!(error.to_string().contains("foreign key validation failed"));
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .fetch_all(raw)
            .await
            .unwrap();
    assert_eq!(tables, ["stray"], "no baseline table or ledger survives");
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_sqlite_writer_contention_waits_and_closed_selected_database_fails() {
    let fixture = NativeDatabase::new().await;
    let other = fixture.second_handle().await;
    let mut writer = fixture.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query(
        "INSERT INTO node_configuration(id,node_mode,setup_completed) VALUES(1,'public',0)",
    )
    .execute(&mut *writer)
    .await
    .unwrap();
    let pool = other.clone();
    let (sent, received) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn(async move {
        sent.send(()).unwrap();
        db::users::accounts::create_user(&pool, "blocked-writer", None).await
    });
    received.await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(
        !waiting.is_finished(),
        "a SQLite write cannot bypass another connection's writer ownership"
    );
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        0,
        "WAL readers retain committed snapshot"
    );
    writer.commit().await.unwrap();
    assert!(waiting.await.unwrap().is_ok());
    other.close().await;
    assert!(
        db::node_config::get_node_configuration(&other)
            .await
            .is_err(),
        "unavailable is not uninitialized/setup"
    );
    assert_eq!(
        db::users::accounts::count_users(&fixture.pool)
            .await
            .unwrap(),
        1
    );
    assert!(
        db::repositories::records::list_public_repositories(
            &fixture.pool,
            db::repositories::records::RepositorySort::Recent,
            -1,
        )
        .await
        .is_err(),
        "SQLite LIMIT -1 must not become unbounded success"
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_concurrent_native_migrations_use_an_independent_atomic_ledger() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}",
        directory.path().join("fresh.sqlite").display()
    );
    let first = db::init_db(&url).await.unwrap();
    let second = db::init_db(&url).await.unwrap();
    let (a, b) = tokio::join!(db::run_migrations(&first), db::run_migrations(&second));
    a.unwrap();
    b.unwrap();
    let user = setup(&first, "public").await;
    assert_eq!(
        db::users::accounts::get_user_by_id(&second, user.id)
            .await
            .unwrap()
            .unwrap()
            .username,
        user.username
    );
    first.close().await;
    second.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_provisioning_visibility_floor_and_native_array_decoding_persist() {
    let mut fixture = NativeDatabase::new().await;
    let owner = setup(&fixture.pool, "public").await;
    let reader = db::users::accounts::create_user(&fixture.pool, "reader", None)
        .await
        .unwrap();
    let source = repositories::insert_repository(
        &fixture.pool,
        repositories::NewRepository {
            owner: repositories::RepositoryCreationOwner::personal(owner.id),
            name: "source",
            description: Some("immutable storage"),
            is_private: false,
            disk_path: "/owned/source.git",
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let fork = db::repositories::discovery::create_forked_repository(
        &fixture.pool,
        db::repositories::discovery::NewForkedRepository {
            owner_id: reader.id,
            name: "fork",
            description: None,
            is_private: false,
            disk_path: "/owned/fork.git",
            forked_from: source.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    assert!(!fork.is_private);
    let descendant = db::repositories::discovery::create_forked_repository(
        &fixture.pool,
        db::repositories::discovery::NewForkedRepository {
            owner_id: owner.id,
            name: "descendant",
            description: None,
            is_private: false,
            disk_path: "/owned/descendant.git",
            forked_from: fork.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    db::repositories::records::update_repository(
        &fixture.pool,
        owner.id,
        source.id,
        None,
        true,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await
    .unwrap();
    assert!(
        db::repositories::records::get_repository_by_id(&fixture.pool, fork.id)
            .await
            .unwrap()
            .unwrap()
            .is_private
    );
    assert!(
        repositories::repository_visibility_locked(&fixture.pool, descendant.id)
            .await
            .unwrap()
    );
    assert!(db::repositories::records::update_repository(
        &fixture.pool,
        owner.id,
        descendant.id,
        None,
        false,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None
        }
    )
    .await
    .is_err());
    db::repositories::records::rename_repository(&fixture.pool, owner.id, source.id, "renamed")
        .await
        .unwrap();
    assert_eq!(
        db::repositories::records::get_repository_by_id(&fixture.pool, source.id)
            .await
            .unwrap()
            .unwrap()
            .disk_path,
        source.disk_path
    );
    let rule = db::repositories::protection::upsert_branch_protection_rule(
        &fixture.pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: owner.id,
            repo_id: source.id,
            pattern: "main",
            require_pull_request: true,
            required_approving_reviews: 2,
            dismiss_stale_reviews: true,
            require_status_checks: true,
            required_status_checks: vec!["build".into()],
            require_up_to_date_branch: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE branch_protection_rules SET push_allowances=?2 WHERE id=?1")
        .bind(rule.id)
        .bind(format!("[{},{}]", owner.id, reader.id))
        .execute(&fixture.raw)
        .await
        .unwrap();
    fixture.reopen().await;
    let rules =
        db::repositories::protection::list_branch_protection_rules(&fixture.pool, source.id)
            .await
            .unwrap();
    assert_eq!(rules[0].push_allowances, vec![owner.id, reader.id]);
    assert_eq!(rules[0].required_status_checks, vec!["build"]);
    assert!(
        db::users::accounts::create_user(&fixture.pool, &"x".repeat(256), None)
            .await
            .is_err(),
        "VARCHAR bounds remain constraints in native SQLite"
    );
    db::users::accounts::update_user_profile(
        &fixture.pool,
        reader.id,
        Some("CAFÉ maintainer"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        db::repositories::discovery::search_users(&fixture.pool, "café", 0, 10)
            .await
            .unwrap()[0]
            .id,
        reader.id,
        "Unicode search is not SQLite ASCII-only LIKE"
    );
    assert!(
        db::repositories::discovery::search_repositories(
            &fixture.pool,
            None,
            "renamed",
            db::repositories::records::RepositorySort::Stars,
            0,
            10
        )
        .await
        .unwrap()
        .is_empty(),
        "private candidates are scoped before matching"
    );
    fixture.close().await;
}

async fn repository_search_ids(pool: &db::DbPool, query: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = repositories::discovery::search_repositories(
        pool,
        None,
        query,
        repositories::records::RepositorySort::Stars,
        0,
        50,
    )
    .await
    .unwrap()
    .iter()
    .map(|repo| repo.id)
    .collect();
    ids.sort_unstable();
    ids
}

async fn check_repository_search_matches_owner_names(pool: &db::DbPool) {
    let admin = setup(pool, "public").await;
    let alice = db::users::accounts::create_user(pool, "alice", None)
        .await
        .unwrap();
    let acme = match db::organizations::create_organization(
        pool,
        admin.id,
        &db::organizations::NewOrganization::new(
            "acme",
            db::organizations::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(metadata) => metadata,
        _ => panic!("the owner creates the organization"),
    };
    let create =
        |owner: i64, name: &'static str, private: bool, description: Option<&'static str>| {
            db::repositories::records::create_repository(
                pool,
                owner,
                name,
                description,
                private,
                "/owned/search-fixture.git",
            )
        };
    let alice_proj = create(alice.id, "proj", false, None).await.unwrap();
    let alice_tools = create(alice.id, "tools", false, Some("plain"))
        .await
        .unwrap();
    create(alice.id, "secret-proj", true, None).await.unwrap();
    let acme_proj = create(admin.id, "proj", false, None).await.unwrap();
    execute_sql(
        pool,
        &format!(
            "UPDATE repositories SET owner_kind='org', owner_user_id=NULL, owner_org_id={} WHERE id={}",
            acme.id, acme_proj.id
        ),
    )
    .await;

    let sorted = |mut ids: Vec<i64>| {
        ids.sort_unstable();
        ids
    };
    // A plain word matches the owner's name as well as the name and description.
    assert_eq!(
        repository_search_ids(pool, "alice").await,
        sorted(vec![alice_proj.id, alice_tools.id]),
    );
    assert_eq!(repository_search_ids(pool, "plain").await, [alice_tools.id],);
    assert_eq!(repository_search_ids(pool, "acme").await, [acme_proj.id],);
    assert_eq!(
        repository_search_ids(pool, "alice/proj").await,
        [alice_proj.id],
    );
    assert_eq!(
        repository_search_ids(pool, "user:alice").await,
        sorted(vec![alice_proj.id, alice_tools.id]),
    );
    assert_eq!(
        repository_search_ids(pool, "org:acme").await,
        [acme_proj.id],
    );
    assert_eq!(
        repository_search_ids(pool, "ORG:acme").await,
        [acme_proj.id],
    );
    // Owner names are exact, and private repositories stay hidden.
    assert!(repository_search_ids(pool, "org:acm").await.is_empty());
    assert!(repository_search_ids(pool, "alice/secret").await.is_empty());
}

#[tokio::test]
async fn sqlite_repository_search_matches_owner_names() {
    let fixture = NativeDatabase::new().await;
    check_repository_search_matches_owner_names(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_repository_search_matches_owner_names() {
    let fixture = PostgresDatabase::new().await;
    check_repository_search_matches_owner_names(&fixture.pool).await;
    fixture.pool.close().await;
}

#[tokio::test]
async fn search_batches_preserve_unicode_order_and_limits_on_both_databases() {
    let sqlite = NativeDatabase::new().await;
    let postgres = PostgresDatabase::new().await;
    for pool in [&sqlite.pool, &postgres.pool] {
        let mut expected = Vec::new();
        for index in (0..70).rev() {
            let user = db::users::accounts::create_user(pool, &format!("search_{index:02}"), None)
                .await
                .unwrap();
            if index % 2 == 0 {
                db::users::accounts::update_user_profile(
                    pool,
                    user.id,
                    Some("CAFé"),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
                expected.push(user.username);
            }
        }
        expected.reverse();
        for limit in [0, 1, 33, 80] {
            let found = db::repositories::discovery::search_users(pool, "café", 0, limit)
                .await
                .unwrap();
            assert_eq!(
                found
                    .into_iter()
                    .map(|user| user.username)
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .take(limit as usize)
                    .cloned()
                    .collect::<Vec<_>>()
            );
        }
    }
    sqlite.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_searches_share_the_pool_without_stalling() {
    let fixture = NativeDatabase::new().await;
    for index in 0..100 {
        db::users::accounts::create_user(&fixture.pool, &format!("pool_{index:03}"), None)
            .await
            .unwrap();
    }
    // More searches than pool connections, each spanning several matching batches.
    let searches: Vec<_> = (0..12)
        .map(|_| {
            let pool = fixture.pool.clone();
            tokio::spawn(async move {
                db::repositories::discovery::search_users(&pool, "pool_", 0, 100).await
            })
        })
        .collect();
    for search in searches {
        let users = tokio::time::timeout(std::time::Duration::from_secs(30), search)
            .await
            .expect("search stalled")
            .unwrap()
            .unwrap();
        assert_eq!(users.len(), 100);
    }
    fixture.close().await;
}

async fn populated_snapshot(pool: &db::DbPool) -> Value {
    let owner = setup(pool, "public").await;
    let reader = db::users::accounts::create_user_with_password(
        pool,
        "reader",
        Some("reader@fixture.invalid"),
        "reader-hash",
    )
    .await
    .unwrap();
    let repo = repository(pool, owner.id, "populated").await;
    db::repositories::access::grant_repo_access(
        pool,
        reader.id,
        repo.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let issue = db::issues::create_issue(pool, repo.id, reader.id, "issue", "body")
        .await
        .unwrap();
    let pr = new_pr(pool, repo.id, owner.id, "feature").await;
    let closed = db::issues::update_issue(
        pool,
        issue.id,
        DiscussionEdit {
            actor_id: reader.id,
            expected_state: "open",
            expected_updated: issue.updated_at,
            title: Some("closed title"),
            body: None,
            state: Some("closed"),
        },
    )
    .await
    .unwrap()
    .unwrap();
    db::issues::add_issue_comment(pool, issue.id, reader.id, "comment")
        .await
        .unwrap();
    db::pull_requests::reviews::add_pull_request_review(
        pool,
        pr.id,
        reader.id,
        "approved",
        Some("review"),
        Some(&pr.head_sha),
    )
    .await
    .unwrap();
    let label = db::labels::create_label(
        pool,
        owner.id,
        repo.id,
        "bug",
        "#123456",
        Some("description"),
    )
    .await
    .unwrap();
    db::labels::set_issue_labels(pool, owner.id, repo.id, issue.id, &[label.id])
        .await
        .unwrap();
    assert!(
        db::labels::set_issue_labels(pool, owner.id, repo.id, issue.id, &[label.id, -1])
            .await
            .is_err()
    );
    db::repository_stats::upsert_repo_languages(
        pool,
        repo.id,
        &[
            ("Rust".into(), 900, 90.25, "#123456".into()),
            ("Other".into(), 100, 9.75, "#654321".into()),
        ],
    )
    .await
    .unwrap();
    db::repository_stats::upsert_repo_contributors(
        pool,
        repo.id,
        &[(
            "Reader".into(),
            "reader@fixture.invalid".into(),
            7,
            Some(reader.id),
        )],
    )
    .await
    .unwrap();
    assert!(db::repository_stats::upsert_repo_contributors(
        pool,
        repo.id,
        &[(
            "Invalid".into(),
            "invalid@fixture.invalid".into(),
            1,
            Some(-1)
        )]
    )
    .await
    .is_err());
    db::repositories::engagement::star_repo(pool, reader.id, repo.id)
        .await
        .unwrap();
    assert!(
        !db::repositories::engagement::star_repo(pool, reader.id, repo.id)
            .await
            .unwrap()
    );
    db::repositories::engagement::watch_repo(pool, reader.id, repo.id)
        .await
        .unwrap();
    db::following::follow_user(pool, reader.id, owner.id)
        .await
        .unwrap();
    assert!(db::following::follow_user(pool, reader.id, reader.id)
        .await
        .is_err());
    let rule = db::repositories::protection::upsert_branch_protection_rule(
        pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: owner.id,
            repo_id: repo.id,
            pattern: "main",
            require_pull_request: true,
            required_approving_reviews: 1,
            dismiss_stale_reviews: true,
            require_status_checks: true,
            required_status_checks: vec!["build".into(), "test".into()],
            require_up_to_date_branch: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(db::repositories::protection::upsert_commit_status(
        pool,
        db::repositories::protection::CommitStatusUpsert {
            repo_id: repo.id,
            sha: &pr.head_sha,
            context: "gitserv/ci",
            state: "success",
            description: None,
            target_url: None,
            creator_id: Some(reader.id)
        }
    )
    .await
    .is_err());
    db::notifications::records::create_notification(
        pool,
        db::notifications::records::NewNotification {
            user_id: owner.id,
            actor_id: Some(reader.id),
            repo_id: Some(repo.id),
            kind: "mention",
            title: "title",
            body: Some("body"),
            link: Some("/fixture"),
            metadata: &json!({"array":[1,2],"bool":true}),
        },
    )
    .await
    .unwrap();
    let encryptor = SecretEncryptor::new(&[0x31; 32]);
    db::secrets::create_or_update_secret(
        pool,
        &encryptor,
        repo.id,
        "TOKEN",
        "fixture-secret",
        Some(owner.id),
    )
    .await
    .unwrap();
    let secrets = db::secrets::decrypt_repository_secrets(pool, &encryptor, repo.id)
        .await
        .unwrap();
    assert_eq!(secrets, vec![("TOKEN".into(), "fixture-secret".into())]);
    let avatar = files::reserve_upload(
        pool,
        upload(owner.id, FileReason::Avatar, FileVisibility::Public),
    )
    .await
    .unwrap();
    assert!(files::activate_upload(pool, avatar.id, owner.id)
        .await
        .unwrap());
    assert!(matches!(
        db::users::accounts::set_user_avatar(pool, owner.id, Some(avatar.id))
            .await
            .unwrap(),
        db::users::accounts::UserAvatarOutcome::Applied(None)
    ));
    files::record_download(
        pool,
        owner.id,
        avatar.id,
        Some("avatar"),
        Some("192.0.2.1".parse().unwrap()),
    )
    .await
    .unwrap();
    let fork = db::repositories::discovery::create_forked_repository(
        pool,
        db::repositories::discovery::NewForkedRepository {
            owner_id: reader.id,
            name: "fork",
            description: None,
            is_private: false,
            disk_path: "/owned/fork.git",
            forked_from: repo.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    assert!(fork.is_private);
    assert!(db::repositories::records::update_repository(
        pool,
        reader.id,
        fork.id,
        None,
        false,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None
        }
    )
    .await
    .is_err());
    let repo = db::repositories::records::get_repository_by_id(pool, repo.id)
        .await
        .unwrap()
        .unwrap();
    let stats = files::get_user_file_stats(pool, owner.id)
        .await
        .unwrap()
        .unwrap();
    let summaries = db::users::accounts::list_admin_account_summaries(pool)
        .await
        .unwrap();
    let activity = db::users::accounts::list_admin_activity_points(pool, 7)
        .await
        .unwrap();
    let emails = db::users::accounts::get_users_by_emails(
        pool,
        &[
            "reader@fixture.invalid".into(),
            "missing@fixture.invalid".into(),
        ],
    )
    .await
    .unwrap();
    let ids = db::users::accounts::get_users_by_ids(pool, &[reader.id, i64::MAX])
        .await
        .unwrap();
    json!({"issue":[closed.number,pr.number],"closed":[closed.state,closed.title,closed.closed_by_id],"labels":db::labels::get_issue_labels(pool,issue.id).await.unwrap().into_iter().map(|l|l.name).collect::<Vec<_>>(),
        "comments":db::issues::list_issue_comments(pool,issue.id,100,0).await.unwrap().len(),"reviews":db::pull_requests::reviews::list_pull_request_reviews(pool,pr.id,100,0).await.unwrap().into_iter().map(|entry|entry.review.state).collect::<Vec<_>>(),
        "languages":db::repository_stats::get_repo_languages(pool,repo.id).await.unwrap().into_iter().map(|l|(l.language,l.bytes,l.percentage)).collect::<Vec<_>>(),"contributors":db::repository_stats::get_repo_contributors(pool,repo.id,100).await.unwrap().into_iter().map(|c|(c.name,c.commits_count)).collect::<Vec<_>>(),
        "social":[repo.stars_count,repo.watchers_count,repo.forks_count],"followers":db::following::count_user_followers(pool,owner.id).await.unwrap(),"rule":[rule.required_status_checks,rule.push_allowances],
        "notification":db::notifications::records::list_notifications_for_user(pool,owner.id,10,false).await.unwrap()[0].metadata,"storage":[stats.total_files,stats.total_bytes,stats.avatar_count,stats.uploads_24h,stats.bytes_24h,stats.downloads_24h],
        "summaries":summaries.into_iter().map(|s|(s.username,s.total_files,s.total_bytes,s.commits_total,s.pushes_total,s.contributions_total)).collect::<Vec<_>>(),
        "activity":activity.into_iter().map(|a|(a.daily_contributions,a.cumulative_contributions,a.daily_storage_bytes,a.cumulative_storage_bytes)).collect::<Vec<_>>(),"emails":emails.into_iter().map(|u|u.username).collect::<Vec<_>>(),"ids":ids.into_iter().map(|u|u.username).collect::<Vec<_>>()})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn populated_sqlite_and_postgres_named_operations_have_semantic_parity() {
    let sqlite = NativeDatabase::new().await;
    let postgres = PostgresDatabase::new().await;
    let sqlite_snapshot = populated_snapshot(&sqlite.pool).await;
    let postgres_snapshot = populated_snapshot(&postgres.pool).await;
    assert_eq!(sqlite_snapshot,postgres_snapshot,"populated parity includes metadata rollback, native arrays/JSON/bytes, uploaded files and aggregate cardinalities");
    sqlite.close().await;
    postgres.pool.close().await;
}

async fn check_ci_admission_refusals(pool: &db::DbPool) {
    use db::ci::{self, CiAdmissionRefusal, NewCiRun};

    let owner = setup(pool, "public").await;
    let repo = repository(pool, owner.id, "ci-refusals").await;
    let stranger = db::users::accounts::create_user(pool, "ci-stranger", None)
        .await
        .unwrap();
    let sha = "1111111111111111111111111111111111111111";
    let blobs = json!({".github/workflows/ci.yml": sha});
    let input = |actor, event| NewCiRun {
        repo_id: repo.id,
        triggered_by_id: actor,
        pipeline: "ci",
        event,
        pull_request_id: None,
        branch: "main",
        head_sha: sha,
        workflow_path: ".github/workflows/ci.yml",
        workflow_blobs: &blobs,
        workflow_inputs: None,
        environment: None,
        manual: false,
    };

    let denied = ci::enqueue_ci_run(pool, input(stranger.id, "push"), &[])
        .await
        .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<CiAdmissionRefusal>(),
        Some(CiAdmissionRefusal::Denied(
            "CI repository execution permission denied"
        ))
    ));
    let stale = ci::enqueue_ci_run(pool, input(owner.id, "pull_request"), &[])
        .await
        .unwrap_err();
    assert!(matches!(
        stale.downcast_ref::<CiAdmissionRefusal>(),
        Some(CiAdmissionRefusal::Conflict(
            "CI pull request revision is not open"
        ))
    ));
    assert!(ci::list_ci_runs_for_repo(pool, repo.id, 10)
        .await
        .unwrap()
        .is_empty());
    assert!(
        db::repositories::protection::list_commit_statuses(pool, repo.id, sha)
            .await
            .unwrap()
            .is_empty()
    );

    let run = ci::enqueue_ci_run(pool, input(owner.id, "push"), &[])
        .await
        .unwrap();
    let missing_actor = ci::request_ci_cancellation(pool, run.run_id, -1)
        .await
        .unwrap_err();
    assert!(matches!(
        missing_actor.downcast_ref::<CiAdmissionRefusal>(),
        Some(CiAdmissionRefusal::Missing("CI actor unavailable"))
    ));
    let missing_run = ci::request_ci_cancellation(pool, Uuid::new_v4(), owner.id)
        .await
        .unwrap_err();
    assert!(matches!(
        missing_run.downcast_ref::<CiAdmissionRefusal>(),
        Some(CiAdmissionRefusal::Missing("CI run unavailable"))
    ));
    let denied = ci::request_ci_cancellation(pool, run.run_id, stranger.id)
        .await
        .unwrap_err();
    assert!(matches!(
        denied.downcast_ref::<CiAdmissionRefusal>(),
        Some(CiAdmissionRefusal::Denied(
            "CI cancellation permission denied"
        ))
    ));
    assert_eq!(
        ci::get_ci_run_by_run_id(pool, run.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "queued"
    );
    assert!(
        ci::request_ci_cancellation(pool, run.run_id, owner.id)
            .await
            .unwrap()
            .0
    );
}

#[tokio::test]
async fn sqlite_ci_admission_refusals_are_typed_and_leave_state_unchanged() {
    let fixture = NativeDatabase::new().await;
    check_ci_admission_refusals(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_ci_admission_refusals_are_typed_and_leave_state_unchanged() {
    let fixture = PostgresDatabase::new().await;
    check_ci_admission_refusals(&fixture.pool).await;
    fixture.pool.close().await;
}

async fn check_list_display_projections(pool: &db::DbPool) {
    use db::{
        issues, labels, organizations,
        pull_requests::{records, reviews},
        repositories,
        users::accounts,
    };

    let owner = setup(pool, "public").await;
    let writer = accounts::create_user_with_password(
        pool,
        "writer",
        Some("private@example.invalid"),
        "private-hash",
    )
    .await
    .unwrap();
    accounts::update_user_profile(
        pool,
        writer.id,
        Some("Writer name"),
        Some("Public bio"),
        Some("Location"),
        Some("https://example.invalid"),
    )
    .await
    .unwrap();
    let base = repository(pool, owner.id, "projection-base").await;
    repositories::access::grant_repo_access(
        pool,
        writer.id,
        base.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let older = issues::create_issue(pool, base.id, owner.id, "Older", "body")
        .await
        .unwrap();
    let newer = issues::create_issue(pool, base.id, writer.id, "Newer", "body")
        .await
        .unwrap();
    match pool {
        db::DbPool::Postgres(native) => {
            sqlx::query("UPDATE issues SET created_at='2020-01-01T00:00:00Z' WHERE id=$1")
                .bind(older.id)
                .execute(native)
                .await
                .unwrap();
            sqlx::query("UPDATE issues SET created_at='2021-01-01T00:00:00Z' WHERE id=$1")
                .bind(newer.id)
                .execute(native)
                .await
                .unwrap();
        }
        db::DbPool::Sqlite(native) => {
            sqlx::query("UPDATE issues SET created_at='2020-01-01T00:00:00.000000Z' WHERE id=?1")
                .bind(older.id)
                .execute(native)
                .await
                .unwrap();
            sqlx::query("UPDATE issues SET created_at='2021-01-01T00:00:00.000000Z' WHERE id=?1")
                .bind(newer.id)
                .execute(native)
                .await
                .unwrap();
        }
    }
    let entries = issues::list_repo_issues(
        pool,
        base.id,
        &ListFilter {
            state: Some("open"),
            ..Default::default()
        },
        1,
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].issue.id, newer.id);
    let author = &entries[0].author;
    assert_eq!(author.id, writer.id);
    assert_eq!(author.username, "writer");
    assert_eq!(author.display_name.as_deref(), Some("Writer name"));
    assert_eq!(author.bio.as_deref(), Some("Public bio"));
    assert_eq!(author.location.as_deref(), Some("Location"));
    assert_eq!(author.website.as_deref(), Some("https://example.invalid"));
    assert!(!author.is_admin);
    assert_eq!(author.created_at, writer.created_at);
    assert!(issues::list_repo_issues(
        pool,
        base.id,
        &ListFilter {
            state: Some("closed"),
            ..Default::default()
        },
        100
    )
    .await
    .unwrap()
    .is_empty());
    assert!(
        issues::list_repo_issues(pool, base.id, &ListFilter::default(), 0)
            .await
            .unwrap()
            .is_empty()
    );

    let zeta = labels::create_label(pool, owner.id, base.id, "zeta", "ff0000", None)
        .await
        .unwrap();
    let alpha = labels::create_label(pool, owner.id, base.id, "alpha", "00ff00", None)
        .await
        .unwrap();
    labels::set_issue_labels(pool, owner.id, base.id, newer.id, &[zeta.id, alpha.id])
        .await
        .unwrap();
    let mut issue_ids: Vec<i64> = (1_000_000..1_000_600).collect();
    issue_ids.extend([newer.id, older.id, newer.id]);
    let batched = labels::get_issues_labels(pool, &issue_ids).await.unwrap();
    assert_eq!(batched.len(), 1);
    assert_eq!(
        batched[&newer.id]
            .iter()
            .map(|label| label.id)
            .collect::<Vec<_>>(),
        vec![alpha.id, zeta.id]
    );
    assert_eq!(
        labels::get_issue_labels(pool, newer.id)
            .await
            .unwrap()
            .iter()
            .map(|label| label.id)
            .collect::<Vec<_>>(),
        vec![alpha.id, zeta.id]
    );
    assert!(labels::get_issues_labels(pool, &[])
        .await
        .unwrap()
        .is_empty());
    let issue_comment = issues::add_issue_comment(pool, newer.id, owner.id, "Discussion")
        .await
        .unwrap();
    let comments = issues::list_issue_comments(pool, newer.id, 100, 0)
        .await
        .unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].comment.id, issue_comment.id);
    assert_eq!(comments[0].author.username, owner.username);
    let second_issue_comment = issues::add_issue_comment(pool, newer.id, owner.id, "Second")
        .await
        .unwrap();
    let third_issue_comment = issues::add_issue_comment(pool, newer.id, owner.id, "Third")
        .await
        .unwrap();
    let issue_comment_ids = |entries: &[issues::IssueCommentEntry]| {
        entries
            .iter()
            .map(|entry| entry.comment.id)
            .collect::<Vec<_>>()
    };
    let listed = issues::list_issue_comments(pool, newer.id, 100, 0)
        .await
        .unwrap();
    let all_issue_comments = issue_comment_ids(&listed);
    assert_eq!(
        all_issue_comments,
        [
            issue_comment.id,
            second_issue_comment.id,
            third_issue_comment.id
        ]
    );
    let first_page = issues::list_issue_comments(pool, newer.id, 2, 0)
        .await
        .unwrap();
    let second_page = issues::list_issue_comments(pool, newer.id, 2, 2)
        .await
        .unwrap();
    assert_eq!(issue_comment_ids(&first_page), &all_issue_comments[..2]);
    assert_eq!(issue_comment_ids(&second_page), &all_issue_comments[2..]);

    let org = match organizations::profiles::create_organization(
        pool,
        owner.id,
        &organizations::NewOrganization::new(
            "display-org",
            organizations::OrganizationProfile::new(None, None).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        organizations::OrganizationOutcome::Applied(org) => org,
        other => panic!("organization creation failed: {other:?}"),
    };
    let org_repo = repositories::creation::insert_repository(
        pool,
        repositories::creation::NewRepository {
            owner: repositories::creation::RepositoryCreationOwner {
                actor_id: owner.id,
                target: repositories::creation::RepositoryTarget::Organization(org.id),
            },
            name: "projection-org",
            description: None,
            is_private: true,
            disk_path: "/owned/projection-org.git",
            storage_node_id: None,
            source_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let head = repositories::creation::insert_repository(
        pool,
        repositories::creation::NewRepository {
            owner: repositories::creation::RepositoryCreationOwner::personal(writer.id),
            name: "projection-head",
            description: None,
            is_private: true,
            disk_path: "/owned/projection-head.git",
            storage_node_id: None,
            source_id: Some(base.id),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let pull = records::create_pull_request(
        pool,
        records::NewPullRequest {
            head_repo_id: Some(head.id),
            repo_id: base.id,
            author_id: owner.id,
            title: "Head display",
            body: "body",
            head_branch: "feature",
            head_sha: "1111111111111111111111111111111111111111",
            base_branch: "main",
            base_sha: "0000000000000000000000000000000000000000",
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let pulls = records::list_repo_pull_requests(
        pool,
        base.id,
        &ListFilter {
            state: Some("open"),
            ..Default::default()
        },
        100,
    )
    .await
    .unwrap();
    assert_eq!(pulls.len(), 1);
    assert_eq!(pulls[0].pull_request.id, pull.id);
    assert_eq!(pulls[0].author.username, owner.username);
    let display = pulls[0].head_repository.as_ref().unwrap();
    assert_eq!(display.id, head.id);
    assert_eq!(display.name, head.name);
    assert_eq!(display.owner_name.as_deref(), Some("writer"));
    let mut repository_ids: Vec<i64> = (1_000_000..1_000_600).collect();
    repository_ids.extend([base.id, head.id, org_repo.id, base.id]);
    let owners = repositories::records::repository_owner_names(pool, &repository_ids)
        .await
        .unwrap();
    assert_eq!(owners.len(), 3);
    assert_eq!(owners[&base.id], owner.username);
    assert_eq!(owners[&head.id], "writer");
    assert_eq!(owners[&org_repo.id], "display-org");
    assert!(repositories::records::repository_owner_names(pool, &[])
        .await
        .unwrap()
        .is_empty());

    let comment = reviews::add_pull_request_comment(
        pool,
        reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: writer.id,
            body: "PR comment",
            path: None,
            line: None,
            side: None,
            commit_sha: None,
            in_reply_to_comment_id: None,
        },
    )
    .await
    .unwrap();
    let comments = reviews::list_pull_request_comments(pool, pull.id, 100, 0)
        .await
        .unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].comment.id, comment.id);
    assert_eq!(comments[0].author.username, "writer");
    let second_comment = reviews::add_pull_request_comment(
        pool,
        reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: writer.id,
            body: "PR second",
            path: None,
            line: None,
            side: None,
            commit_sha: None,
            in_reply_to_comment_id: None,
        },
    )
    .await
    .unwrap();
    let third_comment = reviews::add_pull_request_comment(
        pool,
        reviews::NewPullRequestComment {
            pr_id: pull.id,
            author_id: writer.id,
            body: "PR third",
            path: None,
            line: None,
            side: None,
            commit_sha: None,
            in_reply_to_comment_id: None,
        },
    )
    .await
    .unwrap();
    let comment_ids = |entries: &[reviews::PullRequestCommentEntry]| {
        entries
            .iter()
            .map(|entry| entry.comment.id)
            .collect::<Vec<_>>()
    };
    let listed = reviews::list_pull_request_comments(pool, pull.id, 100, 0)
        .await
        .unwrap();
    let all_comments = comment_ids(&listed);
    assert_eq!(
        all_comments,
        [comment.id, second_comment.id, third_comment.id]
    );
    let first_page = reviews::list_pull_request_comments(pool, pull.id, 2, 0)
        .await
        .unwrap();
    let second_page = reviews::list_pull_request_comments(pool, pull.id, 2, 2)
        .await
        .unwrap();
    assert_eq!(comment_ids(&first_page), &all_comments[..2]);
    assert_eq!(comment_ids(&second_page), &all_comments[2..]);
    let future = reviews::add_pull_request_review(
        pool,
        pull.id,
        writer.id,
        "approved",
        Some("Future"),
        Some("ffffffffffffffffffffffffffffffffffffffff"),
    )
    .await
    .unwrap_err();
    assert!(future.to_string().contains("discussion_review_invalid"));
    let unbound = reviews::add_pull_request_review(
        pool,
        pull.id,
        writer.id,
        "commented",
        Some("Unbound"),
        None,
    )
    .await
    .unwrap_err();
    assert!(unbound.to_string().contains("discussion_review_invalid"));
    let review = reviews::add_pull_request_review(
        pool,
        pull.id,
        writer.id,
        "commented",
        Some("Review"),
        Some(&pull.head_sha),
    )
    .await
    .unwrap();
    let reviews = reviews::list_pull_request_reviews(pool, pull.id, 100, 0)
        .await
        .unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].review.id, review.id);
    assert_eq!(reviews[0].author.username, "writer");
    let approved = reviews::add_pull_request_review(
        pool,
        pull.id,
        writer.id,
        "approved",
        Some("Ship it"),
        Some(&pull.head_sha),
    )
    .await
    .unwrap();
    // A comment after a vote keeps the vote.
    let still_reading = reviews::add_pull_request_review(
        pool,
        pull.id,
        writer.id,
        "commented",
        Some("Still reading"),
        Some(&pull.head_sha),
    )
    .await
    .unwrap();
    let decisions = reviews::list_pull_request_review_decisions(pool, pull.id)
        .await
        .unwrap();
    assert_eq!(
        decisions
            .iter()
            .map(|decision| (decision.author_id, decision.state.as_str()))
            .collect::<Vec<_>>(),
        [(writer.id, "approved")]
    );
    let changes = reviews::add_pull_request_review(
        pool,
        pull.id,
        writer.id,
        "changes_requested",
        Some("Fix it"),
        Some(&pull.head_sha),
    )
    .await
    .unwrap();
    reviews::dismiss_pull_request_review(pool, changes.id, owner.id, Some("Addressed"))
        .await
        .unwrap();
    // The dismissal is the reviewer's latest decision, so it replaces the approval.
    let decisions = reviews::list_pull_request_review_decisions(pool, pull.id)
        .await
        .unwrap();
    assert_eq!(
        decisions
            .iter()
            .map(|decision| (decision.author_id, decision.state.as_str()))
            .collect::<Vec<_>>(),
        [(writer.id, "dismissed")]
    );
    // The batch read used by the pull request list keeps the same latest decision per reviewer.
    let states = reviews::list_pull_request_review_states(pool, &[pull.id])
        .await
        .unwrap();
    assert_eq!(
        states
            .iter()
            .map(|(id, state, _)| (*id, state.as_str()))
            .collect::<Vec<_>>(),
        [(pull.id, "dismissed")]
    );
    // The author cannot request changes on their own pull request, so a second writer does.
    let second = accounts::create_user_with_password(pool, "second-writer", None, "second-hash")
        .await
        .unwrap();
    repositories::access::grant_repo_access(
        pool,
        second.id,
        base.id,
        db::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let owner_review = reviews::add_pull_request_review(
        pool,
        pull.id,
        second.id,
        "changes_requested",
        Some("Please change this"),
        Some(&pull.head_sha),
    )
    .await
    .unwrap();
    // One decision per reviewer, oldest decision first.
    let decisions = reviews::list_pull_request_review_decisions(pool, pull.id)
        .await
        .unwrap();
    assert_eq!(
        decisions
            .iter()
            .map(|decision| (decision.author_id, decision.state.as_str()))
            .collect::<Vec<_>>(),
        [(writer.id, "dismissed"), (second.id, "changes_requested")]
    );
    let review_ids = |entries: &[reviews::PullRequestReviewEntry]| {
        entries
            .iter()
            .map(|entry| entry.review.id)
            .collect::<Vec<_>>()
    };
    let listed = reviews::list_pull_request_reviews(pool, pull.id, 100, 0)
        .await
        .unwrap();
    let all_reviews = review_ids(&listed);
    assert_eq!(
        all_reviews,
        [
            review.id,
            approved.id,
            still_reading.id,
            changes.id,
            owner_review.id
        ]
    );
    for (offset, expected) in [
        (0, &all_reviews[..2]),
        (2, &all_reviews[2..4]),
        (4, &all_reviews[4..]),
    ] {
        let page = reviews::list_pull_request_reviews(pool, pull.id, 2, offset)
            .await
            .unwrap();
        assert_eq!(review_ids(&page), expected);
    }

    repositories::records::delete_repository(pool, writer.id, head.id)
        .await
        .unwrap();
    let pulls = records::list_repo_pull_requests(pool, base.id, &ListFilter::default(), 100)
        .await
        .unwrap();
    assert_eq!(pulls[0].pull_request.id, pull.id);
    assert!(pulls[0].pull_request.head_repo_id.is_none());
    assert!(pulls[0].head_repository.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_list_author_label_and_owner_projections() {
    let fixture = NativeDatabase::new().await;
    check_list_display_projections(&fixture.pool).await;
    assert!(
        db::notifications::events::NotificationListener::connect(&fixture.pool)
            .await
            .unwrap()
            .is_none()
    );
    db::notifications::events::announce(&fixture.pool, "local-only")
        .await
        .unwrap();
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_list_projections_and_owned_notification_listener() {
    let fixture = PostgresDatabase::new().await;
    check_list_display_projections(&fixture.pool).await;
    let mut listener = db::notifications::events::NotificationListener::connect(&fixture.pool)
        .await
        .unwrap()
        .unwrap();
    let payload =
        r#"{"origin":"projection-test","event":{"type":"read_all","user_id":1,"unread_count":0}}"#;
    db::notifications::events::announce(&fixture.pool, payload)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
            .await
            .unwrap()
            .unwrap(),
        payload
    );
    drop(listener);
    fixture.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_read_failures_propagate() {
    let fixture = NativeDatabase::new().await;
    assert!(!fixture.pool.is_closed());
    let owner = setup(&fixture.pool, "public").await;
    let repo = repository(&fixture.pool, owner.id, "closed-reads").await;
    let issue = db::issues::create_issue(&fixture.pool, repo.id, owner.id, "Issue", "body")
        .await
        .unwrap();

    fixture.pool.close().await;
    assert!(fixture.pool.is_closed());
    assert!(
        db::issues::list_repo_issues(&fixture.pool, repo.id, &ListFilter::default(), 100)
            .await
            .is_err()
    );
    assert!(
        db::issues::list_issue_comments(&fixture.pool, issue.id, 100, 0)
            .await
            .is_err()
    );
    assert!(db::labels::get_issues_labels(&fixture.pool, &[issue.id])
        .await
        .is_err());
    assert!(
        db::repositories::records::repository_owner_names(&fixture.pool, &[repo.id])
            .await
            .is_err()
    );
    fixture.close().await;
}

/// Backdates `count` rows in each table the identity sweep covers. The application
/// API cannot create expired rows, so the fixture writes them directly.
async fn insert_transfer_rows(
    pool: &db::DbPool,
    user_id: i64,
    file_id: Uuid,
    uploaded_at: DateTime<Utc>,
    downloaded_at: DateTime<Utc>,
    count: i64,
) {
    for (table, column, at, size) in [
        (
            "upload_rate_limit",
            "uploaded_at",
            uploaded_at,
            ", file_size",
        ),
        ("download_rate_limit", "downloaded_at", downloaded_at, ""),
    ] {
        let bytes = if size.is_empty() { "" } else { ", 10" };
        match pool {
            db::DbPool::Postgres(raw) => {
                sqlx::query(&format!(
                    "INSERT INTO {table} (user_id, file_id, reason, {column}{size})
                     SELECT $1, $2, 'other', $3::timestamptz{bytes} FROM generate_series(1::bigint, $4::bigint)"
                ))
                .bind(user_id)
                .bind(file_id)
                .bind(at)
                .bind(count)
                .execute(raw)
                .await
                .unwrap();
            }
            db::DbPool::Sqlite(raw) => {
                sqlx::query(&format!(
                    "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?4)
                     INSERT INTO {table} (user_id, file_id, reason, {column}{size})
                     SELECT ?1, ?2, 'other', ?3{bytes} FROM n"
                ))
                .bind(user_id)
                .bind(file_id)
                .bind(timestamp(at))
                .bind(count)
                .execute(raw)
                .await
                .unwrap();
            }
        }
    }
}

async fn transfer_row_counts(pool: &db::DbPool) -> (i64, i64) {
    let sql = "SELECT (SELECT COUNT(*) FROM upload_rate_limit), (SELECT COUNT(*) FROM download_rate_limit)";
    match pool {
        db::DbPool::Postgres(raw) => sqlx::query_as(sql).fetch_one(raw).await.unwrap(),
        db::DbPool::Sqlite(raw) => sqlx::query_as(sql).fetch_one(raw).await.unwrap(),
    }
}

async fn check_expired_transfer_rows_are_bounded_and_preserve_windows(pool: &db::DbPool) {
    let owner = setup(pool, "public").await;
    let live = files::reserve_upload(
        pool,
        upload(owner.id, FileReason::Other, FileVisibility::Owner),
    )
    .await
    .unwrap();
    assert!(files::activate_upload(pool, live.id, owner.id)
        .await
        .unwrap());
    files::record_download(pool, owner.id, live.id, None, None)
        .await
        .unwrap();
    let deleted = files::reserve_upload(
        pool,
        upload(owner.id, FileReason::Other, FileVisibility::Owner),
    )
    .await
    .unwrap();
    assert!(matches!(
        files::delete_file(pool, deleted.id, owner.id)
            .await
            .unwrap(),
        files::DeleteFileOutcome::Deleted(_)
    ));

    let now = Utc::now();
    let first_day = (now.date_naive() - Duration::days(729))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    insert_transfer_rows(
        pool,
        owner.id,
        live.id,
        first_day,
        now - Duration::days(29),
        1,
    )
    .await;
    insert_transfer_rows(
        pool,
        owner.id,
        live.id,
        now - Duration::hours(23),
        now - Duration::hours(23),
        1,
    )
    .await;
    insert_transfer_rows(
        pool,
        owner.id,
        live.id,
        now - Duration::days(731),
        now - Duration::days(31),
        501,
    )
    .await;
    // Expired charges with no surviving file must be reclaimed too.
    match pool {
        db::DbPool::Postgres(raw) => {
            sqlx::query("UPDATE upload_rate_limit SET file_id=NULL WHERE uploaded_at < NOW() - INTERVAL '730 days'").execute(raw).await.unwrap();
        }
        db::DbPool::Sqlite(raw) => {
            sqlx::query("UPDATE upload_rate_limit SET file_id=NULL WHERE uploaded_at < strftime('%Y-%m-%dT%H:%M:%f000Z','now','-730 days')").execute(raw).await.unwrap();
        }
    }
    let stats = files::get_user_file_stats(pool, owner.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            stats.total_files,
            stats.total_bytes,
            stats.uploads_24h,
            stats.bytes_24h,
            stats.downloads_24h
        ),
        (1, 10, 3, 30, 2)
    );
    let report = db::users::accounts::list_admin_activity_points(pool, 730)
        .await
        .unwrap();
    assert_eq!(report.len(), 730);
    assert_eq!(report[0].daily_storage_bytes, 10);
    assert_eq!(report.last().unwrap().cumulative_storage_bytes, 40);

    assert_eq!(transfer_row_counts(pool).await, (505, 504));
    assert_eq!(
        files::remove_expired_transfer_rows(pool).await.unwrap(),
        1002
    );
    assert_eq!(files::remove_expired_transfer_rows(pool).await.unwrap(), 0);
    assert_eq!(transfer_row_counts(pool).await, (4, 3));
    assert_eq!(
        json!(files::get_user_file_stats(pool, owner.id)
            .await
            .unwrap()
            .unwrap()),
        json!(stats)
    );
    assert_eq!(
        json!(db::users::accounts::list_admin_activity_points(pool, 730)
            .await
            .unwrap()),
        json!(report)
    );
    assert!(files::get_user_file(pool, live.id, owner.id)
        .await
        .unwrap()
        .is_some());
    assert!(files::get_file_by_id(pool, deleted.id)
        .await
        .unwrap()
        .is_none());

    // Independently exhaust the hour and day counts, including the deleted file's charge.
    for (hour, day) in [(2, 4), (3, 3)] {
        match pool {
            db::DbPool::Postgres(raw) => {
                sqlx::query("UPDATE file_reason_limits SET max_uploads_per_hour=$1,max_uploads_per_day=$2 WHERE reason='other'")
                    .bind(hour).bind(day).execute(raw).await.unwrap();
            }
            db::DbPool::Sqlite(raw) => {
                sqlx::query("UPDATE file_reason_limits SET max_uploads_per_hour=?1,max_uploads_per_day=?2 WHERE reason='other'")
                    .bind(hour).bind(day).execute(raw).await.unwrap();
            }
        }
        assert!(matches!(
            files::reserve_upload(
                pool,
                upload(owner.id, FileReason::Other, FileVisibility::Owner)
            )
            .await,
            Err(AdmissionError::Quota)
        ));
    }
    assert_eq!(transfer_row_counts(pool).await, (4, 3));

    // A larger backlog stops at 20 batches per table, then drains on the next sweep.
    insert_transfer_rows(
        pool,
        owner.id,
        live.id,
        now - Duration::days(731),
        now - Duration::days(31),
        10_001,
    )
    .await;
    assert_eq!(
        files::remove_expired_transfer_rows(pool).await.unwrap(),
        20_000
    );
    assert_eq!(transfer_row_counts(pool).await, (5, 4));
    assert_eq!(files::remove_expired_transfer_rows(pool).await.unwrap(), 2);
    assert_eq!(transfer_row_counts(pool).await, (4, 3));
}

#[tokio::test]
async fn sqlite_expired_transfer_rows_are_bounded_and_preserve_windows() {
    let fixture = NativeDatabase::new().await;
    check_expired_transfer_rows_are_bounded_and_preserve_windows(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_expired_transfer_rows_are_bounded_and_preserve_windows() {
    let fixture = PostgresDatabase::new().await;
    check_expired_transfer_rows_are_bounded_and_preserve_windows(&fixture.pool).await;
    fixture.pool.close().await;
}

async fn insert_expired_identity_rows(pool: &db::DbPool, user_id: i64, count: i64) {
    let past = Utc::now() - Duration::days(1);
    match pool {
        db::DbPool::Postgres(raw) => {
            sqlx::query("INSERT INTO anonymous_sessions (id, expires_at) SELECT 'expired-anon-' || i, $1::timestamptz FROM generate_series(1::bigint, $2::bigint) AS i")
                .bind(past)
                .bind(count)
                .execute(raw)
                .await
                .unwrap();
            sqlx::query("INSERT INTO sessions (id, user_id, expires_at) SELECT 'expired-session-' || i, $3::bigint, $1::timestamptz FROM generate_series(1::bigint, $2::bigint) AS i")
                .bind(past)
                .bind(count)
                .bind(user_id)
                .execute(raw)
                .await
                .unwrap();
            sqlx::query("INSERT INTO oauth_states (state, pkce_verifier, provider, expires_at) SELECT 'expired-state-' || i, 'verifier', 'github', $1::timestamptz FROM generate_series(1::bigint, $2::bigint) AS i")
                .bind(past)
                .bind(count)
                .execute(raw)
                .await
                .unwrap();
        }
        db::DbPool::Sqlite(raw) => {
            let past = timestamp(past);
            sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?2) INSERT INTO anonymous_sessions (id, expires_at) SELECT 'expired-anon-' || i, ?1 FROM n")
                .bind(past.clone())
                .bind(count)
                .execute(raw)
                .await
                .unwrap();
            sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?2) INSERT INTO sessions (id, user_id, expires_at) SELECT 'expired-session-' || i, ?3, ?1 FROM n")
                .bind(past.clone())
                .bind(count)
                .bind(user_id)
                .execute(raw)
                .await
                .unwrap();
            sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?2) INSERT INTO oauth_states (state, pkce_verifier, provider, expires_at) SELECT 'expired-state-' || i, 'verifier', 'github', ?1 FROM n")
                .bind(past)
                .bind(count)
                .execute(raw)
                .await
                .unwrap();
        }
    }
}

async fn check_expired_identity_rows_are_removed_in_batches(pool: &db::DbPool) {
    let admin = setup(pool, "public").await;
    // 501 rows per table: each statement runs one full batch, then one short batch.
    insert_expired_identity_rows(pool, admin.id, 501).await;
    let future = Utc::now() + Duration::days(1);
    let live_anonymous = token();
    let live_session = token();
    sessions::create_browser_session(pool, &live_anonymous, None, None, future)
        .await
        .unwrap();
    sessions::create_session(pool, &live_session, admin.id, None, None, future)
        .await
        .unwrap();
    sessions::save_oauth_state(pool, "live-state", "verifier", "github", None)
        .await
        .unwrap();
    assert_eq!(
        sessions::remove_expired_identity_rows(pool).await.unwrap(),
        3 * 501
    );
    assert_eq!(
        sessions::remove_expired_identity_rows(pool).await.unwrap(),
        0
    );
    assert!(sessions::get_browser_session(pool, &live_anonymous)
        .await
        .unwrap()
        .is_some());
    assert!(sessions::get_session(pool, &live_session)
        .await
        .unwrap()
        .is_some());
    assert!(sessions::get_oauth_state(pool, "live-state")
        .await
        .unwrap()
        .is_some());
}

async fn check_processed_signup_verifiers_are_cleared(pool: &db::DbPool) {
    use db::users::signup_requests;
    let admin = setup(pool, "private").await;
    signup_requests::create_signup_request(pool, "applicant-pending", None, "hash-pending")
        .await
        .unwrap();
    let approved =
        signup_requests::create_signup_request(pool, "applicant-approved", None, "hash-approved")
            .await
            .unwrap();
    let rejected =
        signup_requests::create_signup_request(pool, "applicant-rejected", None, "hash-rejected")
            .await
            .unwrap();
    signup_requests::approve_signup_request(pool, approved.id, admin.id)
        .await
        .unwrap();
    signup_requests::reject_signup_request(pool, rejected.id, admin.id)
        .await
        .unwrap();
    assert_eq!(
        sessions::remove_expired_identity_rows(pool).await.unwrap(),
        2
    );
    assert_eq!(
        sessions::remove_expired_identity_rows(pool).await.unwrap(),
        0
    );
    let pending = signup_requests::list_signup_requests(pool, "pending")
        .await
        .unwrap();
    assert_eq!(pending[0].password_hash, "hash-pending");
    for status in ["approved", "rejected"] {
        let processed = signup_requests::list_signup_requests(pool, status)
            .await
            .unwrap();
        assert_eq!(processed.len(), 1);
        assert_eq!(processed[0].password_hash, "");
    }
    // The created account keeps its own verifier after the signup copy is cleared.
    let user = db::users::accounts::get_user_by_username(pool, "applicant-approved")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.password_hash.as_deref(), Some("hash-approved"));
}

async fn check_anonymous_sessions_keep_the_newest_rows(pool: &db::DbPool) {
    let admin = setup(pool, "public").await;
    let now = Utc::now();
    // Five live anonymous sessions, oldest first, and one authenticated session.
    let mut anonymous = Vec::new();
    for hours in 0..5 {
        let id = token();
        sessions::create_browser_session(
            pool,
            &id,
            None,
            None,
            now + Duration::days(1) + Duration::hours(hours),
        )
        .await
        .unwrap();
        anonymous.push(id);
    }
    let authenticated = token();
    sessions::create_session(
        pool,
        &authenticated,
        admin.id,
        None,
        None,
        now + Duration::days(1),
    )
    .await
    .unwrap();
    assert_eq!(
        sessions::keep_newest_anonymous_sessions(pool, 3)
            .await
            .unwrap(),
        2
    );
    for id in &anonymous[..2] {
        assert!(sessions::get_browser_session(pool, id)
            .await
            .unwrap()
            .is_none());
    }
    for id in &anonymous[2..] {
        assert!(sessions::get_browser_session(pool, id)
            .await
            .unwrap()
            .is_some());
    }
    assert!(sessions::get_session(pool, &authenticated)
        .await
        .unwrap()
        .is_some());
    // Rows under the cap are never removed, so repeating the pass changes nothing.
    assert_eq!(
        sessions::keep_newest_anonymous_sessions(pool, 3)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sessions::keep_newest_anonymous_sessions(pool, 10)
            .await
            .unwrap(),
        0
    );
}

async fn anonymous_session_ids_newest_first(pool: &db::DbPool) -> Vec<String> {
    const SQL: &str = "SELECT id FROM anonymous_sessions ORDER BY expires_at DESC, id DESC";
    match pool {
        db::DbPool::Postgres(raw) => sqlx::query_scalar(SQL).fetch_all(raw).await.unwrap(),
        db::DbPool::Sqlite(raw) => sqlx::query_scalar(SQL).fetch_all(raw).await.unwrap(),
    }
}

async fn check_concurrent_anonymous_session_sweeps_keep_the_newest_rows(pool: &db::DbPool) {
    const CAP: i64 = 10;
    // 510 rows beyond the cap: the first sweep deletes a full batch, the second the rest.
    let base = Utc::now() + Duration::days(2);
    let mut ids = Vec::new();
    for offset in 0..CAP + 510 {
        let id = token();
        sessions::create_browser_session(pool, &id, None, None, base + Duration::seconds(offset))
            .await
            .unwrap();
        ids.push(id);
    }
    let before = anonymous_session_ids_newest_first(pool).await.len() as i64;
    let (first, second) = tokio::join!(
        sessions::keep_newest_anonymous_sessions(pool, CAP),
        sessions::keep_newest_anonymous_sessions(pool, CAP),
    );
    // Together the sweeps remove exactly the surplus, and the newest rows remain.
    assert_eq!((first.unwrap() + second.unwrap()) as i64, before - CAP);
    let newest: Vec<String> = ids.iter().rev().take(CAP as usize).cloned().collect();
    assert_eq!(anonymous_session_ids_newest_first(pool).await, newest);
}

#[tokio::test]
async fn sqlite_anonymous_sessions_keep_only_the_newest_rows() {
    let fixture = NativeDatabase::new().await;
    check_anonymous_sessions_keep_the_newest_rows(&fixture.pool).await;
    // One connection makes the two sweeps take turns statement by statement, so the second
    // starts after the first has deleted a batch.
    let single: db::DbPool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&fixture.url)
        .await
        .unwrap()
        .into();
    check_concurrent_anonymous_session_sweeps_keep_the_newest_rows(&single).await;
    single.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_anonymous_sessions_keep_only_the_newest_rows() {
    let fixture = PostgresDatabase::new().await;
    check_anonymous_sessions_keep_the_newest_rows(&fixture.pool).await;
    // As in the SQLite test, one connection makes the two sweeps take turns.
    let single: db::DbPool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&fixture.connection_url())
        .await
        .unwrap()
        .into();
    check_concurrent_anonymous_session_sweeps_keep_the_newest_rows(&single).await;
    single.close().await;
    fixture.pool.close().await;
}

async fn anonymous_session_count(pool: &db::DbPool) -> i64 {
    const SQL: &str = "SELECT count(*) FROM anonymous_sessions";
    match pool {
        db::DbPool::Postgres(raw) => sqlx::query_scalar(SQL).fetch_one(raw).await.unwrap(),
        db::DbPool::Sqlite(raw) => sqlx::query_scalar(SQL).fetch_one(raw).await.unwrap(),
    }
}

async fn check_sessions_arriving_during_a_sweep_survive(pool: &db::DbPool) {
    const CAP: i64 = 10;
    // Three batches of surplus, so the sweep runs more than one delete.
    let base = Utc::now() + Duration::days(2);
    let mut ids = Vec::new();
    for offset in 0..CAP + 1_010 {
        let id = token();
        sessions::create_browser_session(pool, &id, None, None, base + Duration::seconds(offset))
            .await
            .unwrap();
        ids.push(id);
    }
    let before = anonymous_session_count(pool).await;
    let arrival = token();
    let arrival_expiry = base + Duration::days(1);
    let sweep = sessions::keep_newest_anonymous_sessions(pool, CAP);
    let arrive = async {
        // Insert only after the first batch has committed, so the boundary was read before
        // the arrival existed.
        for _ in 0..2_000 {
            if anonymous_session_count(pool).await < before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        sessions::create_browser_session(pool, &arrival, None, None, arrival_expiry)
            .await
            .unwrap();
    };
    let (removed, ()) = tokio::join!(sweep, arrive);
    // Exactly the surplus counted at the boundary read is removed. The arrival is newer than
    // the boundary, so it survives beside the newest `CAP` rows.
    assert_eq!(removed.unwrap(), (before - CAP) as u64);
    let mut newest = vec![arrival];
    newest.extend(ids.iter().rev().take(CAP as usize).cloned());
    assert_eq!(anonymous_session_ids_newest_first(pool).await, newest);
}

#[tokio::test]
async fn sqlite_sessions_arriving_during_a_sweep_survive() {
    let fixture = NativeDatabase::new().await;
    check_sessions_arriving_during_a_sweep_survive(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_sessions_arriving_during_a_sweep_survive() {
    let fixture = PostgresDatabase::new().await;
    check_sessions_arriving_during_a_sweep_survive(&fixture.pool).await;
    fixture.pool.close().await;
}

#[tokio::test]
async fn sqlite_expired_identity_rows_are_removed_in_batches() {
    let fixture = NativeDatabase::new().await;
    check_expired_identity_rows_are_removed_in_batches(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_expired_identity_rows_are_removed_in_batches() {
    let fixture = PostgresDatabase::new().await;
    check_expired_identity_rows_are_removed_in_batches(&fixture.pool).await;
    fixture.pool.close().await;
}

fn postgres_plan_nodes(plan: &Value) -> Vec<&Value> {
    let mut nodes = vec![&plan[0]["Plan"]];
    let mut index = 0;
    while index < nodes.len() {
        if let Some(children) = nodes[index]["Plans"].as_array() {
            nodes.extend(children);
        }
        index += 1;
    }
    nodes
}

#[tokio::test]
async fn postgres_identity_cleanup_uses_expiry_index_bounds() {
    let fixture = PostgresDatabase::new().await;
    let owner = setup(&fixture.pool, "public").await;
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!()
    };
    insert_expired_identity_rows(&fixture.pool, owner.id, 1).await;
    sqlx::query("INSERT INTO sessions(id,user_id,expires_at) SELECT 'live-' || i,$1,now()+interval '1 day' FROM generate_series(1,20000) i")
        .bind(owner.id).execute(raw).await.unwrap();
    sqlx::raw_sql("INSERT INTO anonymous_sessions(id,expires_at) SELECT 'live-' || i,now()+interval '1 day' FROM generate_series(1,20000) i;
        INSERT INTO oauth_states(state,pkce_verifier,provider,expires_at) SELECT 'live-' || i,'verifier','github',now()+interval '1 day' FROM generate_series(1,20000) i;
        ANALYZE sessions; ANALYZE anonymous_sessions; ANALYZE oauth_states;")
        .execute(raw).await.unwrap();

    // Explain the actual sweep statements, not copies that can drift from the operation.
    let source = include_str!("../../src/backend/db/postgres/users/sessions.rs");
    let statements = source
        .split_once("const STATEMENTS: [&str; 4] = [")
        .unwrap()
        .1
        .split_once("];")
        .unwrap()
        .0;
    let statements: Vec<_> = statements
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix('"')
                .and_then(|line| line.strip_suffix("\","))
        })
        .take(3)
        .collect();
    assert_eq!(statements.len(), 3);
    for expired in [true, false] {
        for sql in &statements {
            let mut tx = raw.begin().await.unwrap();
            let plan: Value = sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"))
                .bind(500_i64)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            let nodes = postgres_plan_nodes(&plan);
            assert!(
                nodes.iter().any(|node| node["Index Cond"]
                    .as_str()
                    .is_some_and(|condition| condition.contains("expires_at <="))),
                "expiry must bound an index scan: {plan}"
            );
            assert!(
                nodes
                    .iter()
                    .all(|node| node["Rows Removed by Filter"].as_u64().unwrap_or(0) < 100),
                "retained rows must not be filtered: {plan}"
            );
        }
        assert_eq!(
            sessions::remove_expired_identity_rows(&fixture.pool)
                .await
                .unwrap(),
            if expired { 3 } else { 0 }
        );
    }
    fixture.pool.close().await;
}

#[tokio::test]
async fn sqlite_processed_signup_verifiers_are_cleared() {
    let fixture = NativeDatabase::new().await;
    check_processed_signup_verifiers_are_cleared(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_processed_signup_verifiers_are_cleared() {
    let fixture = PostgresDatabase::new().await;
    check_processed_signup_verifiers_are_cleared(&fixture.pool).await;
    fixture.pool.close().await;
}

/// An acknowledgement queues behind an account or membership transition before it locks
/// the recipient row. A ban takes the membership lock first and then organization rows,
/// so an acknowledgement that locked the recipient first could deadlock with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_acknowledgement_waits_for_membership_transition() {
    let fixture = PostgresDatabase::new().await;
    let owner = db::users::accounts::create_user(&fixture.pool, "ack_owner", None)
        .await
        .unwrap();
    let recipient = owner.id;
    db::notifications::records::create_notification(
        &fixture.pool,
        db::notifications::records::NewNotification {
            user_id: recipient,
            actor_id: None,
            repo_id: None,
            kind: "account",
            title: "Notice",
            body: None,
            link: None,
            metadata: &json!({}),
        },
    )
    .await
    .unwrap();
    let db::DbPool::Postgres(raw) = &fixture.pool else {
        unreachable!("PostgreSQL fixture")
    };

    // A transition holds the membership lock until it commits.
    let mut transition = raw.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()), 37002)")
        .execute(&mut *transition)
        .await
        .unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *transition)
        .await
        .unwrap();

    let acknowledged = tokio::spawn({
        let pool = fixture.pool.clone();
        async move { db::notifications::records::mark_all_notifications_read(&pool, recipient).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let queued: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))",
            )
            .bind(holder)
            .fetch_one(raw)
            .await
            .unwrap();
            if queued {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("acknowledgement queues behind the membership transition");
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE NOWAIT")
        .bind(recipient)
        .fetch_one(raw)
        .await
        .expect("the queued acknowledgement has not locked the recipient yet");

    transition.commit().await.unwrap();
    assert_eq!(acknowledged.await.unwrap().unwrap(), 1);
}

async fn profile_after_update(
    pool: &db::DbPool,
    actor: i64,
    organization: i64,
    profile: db::organizations::OrganizationProfile,
) -> (Option<String>, Option<String>) {
    match db::organizations::update_organization_profile(pool, actor, organization, &profile)
        .await
        .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(metadata) => {
            (metadata.display_name, metadata.description)
        }
        _ => panic!("the owner updates the profile"),
    }
}

/// A field that an update leaves out keeps the value the database holds when the update
/// runs. A description-only update must not restore a display name that another update
/// changed after the request read the organization.
async fn check_partial_profile_updates_keep_omitted_fields(pool: &db::DbPool) {
    use db::organizations::OrganizationProfile;

    let owner = db::users::accounts::create_user(pool, "profile_owner", None)
        .await
        .unwrap();
    let organization = match db::organizations::create_organization(
        pool,
        owner.id,
        &db::organizations::NewOrganization::new(
            "profile-org",
            OrganizationProfile::new(Some("Old name"), Some("Old description")).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    {
        db::organizations::OrganizationOutcome::Applied(metadata) => metadata,
        _ => panic!("the owner creates the organization"),
    };

    let renamed = OrganizationProfile::partial(Some(Some("New name")), None).unwrap();
    assert_eq!(
        profile_after_update(pool, owner.id, organization.id, renamed).await,
        (
            Some("New name".to_owned()),
            Some("Old description".to_owned())
        )
    );
    let described = OrganizationProfile::partial(None, Some(Some("New description"))).unwrap();
    assert_eq!(
        profile_after_update(pool, owner.id, organization.id, described).await,
        (
            Some("New name".to_owned()),
            Some("New description".to_owned())
        )
    );
    let cleared = OrganizationProfile::partial(Some(None), None).unwrap();
    assert_eq!(
        profile_after_update(pool, owner.id, organization.id, cleared).await,
        (None, Some("New description".to_owned()))
    );
}

#[tokio::test]
async fn sqlite_partial_profile_updates_keep_omitted_fields() {
    let fixture = NativeDatabase::new().await;
    check_partial_profile_updates_keep_omitted_fields(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_partial_profile_updates_keep_omitted_fields() {
    let fixture = PostgresDatabase::new().await;
    check_partial_profile_updates_keep_omitted_fields(&fixture.pool).await;
    fixture.pool.close().await;
}

const GIB: i64 = 1 << 30;
const MIB: i64 = 1 << 20;

/// Records a storage snapshot at a chosen time instead of the schema's "now" default.
async fn record_storage_snapshot(
    pool: &db::DbPool,
    repo_id: i64,
    size_bytes: i64,
    recorded_at: DateTime<Utc>,
) {
    match pool {
        db::DbPool::Sqlite(raw) => {
            sqlx::query(
                "INSERT INTO repo_storage_history (repo_id, size_bytes, recorded_at) VALUES (?1, ?2, ?3)",
            )
            .bind(repo_id)
            .bind(size_bytes)
            .bind(timestamp(recorded_at))
            .execute(raw)
            .await
            .unwrap();
        }
        db::DbPool::Postgres(raw) => {
            sqlx::query(
                "INSERT INTO repo_storage_history (repo_id, size_bytes, recorded_at) VALUES ($1, $2, $3)",
            )
            .bind(repo_id)
            .bind(size_bytes)
            .bind(recorded_at)
            .execute(raw)
            .await
            .unwrap();
        }
    }
}

/// Moves a row's creation time so the activity window can be tested at its edges.
async fn set_created_at(pool: &db::DbPool, table: &str, id: i64, created_at: DateTime<Utc>) {
    match pool {
        db::DbPool::Sqlite(raw) => {
            sqlx::query(&format!("UPDATE {table} SET created_at = ?1 WHERE id = ?2"))
                .bind(timestamp(created_at))
                .bind(id)
                .execute(raw)
                .await
                .unwrap();
        }
        db::DbPool::Postgres(raw) => {
            sqlx::query(&format!("UPDATE {table} SET created_at = $1 WHERE id = $2"))
                .bind(created_at)
                .bind(id)
                .execute(raw)
                .await
                .unwrap();
        }
    }
}

async fn check_storage_growth_counts_only_bytes_added_in_the_window(pool: &db::DbPool) {
    let owner = db::users::accounts::create_user(pool, "storage-owner", None)
        .await
        .unwrap();
    let repo = repository(pool, owner.id, "growth").await;
    let now = Utc::now();
    record_storage_snapshot(pool, repo.id, 2 * GIB, now - Duration::days(30)).await;
    // The latest snapshot before the window is the baseline for the first day in it.
    record_storage_snapshot(pool, repo.id, GIB, now - Duration::days(10)).await;
    record_storage_snapshot(pool, repo.id, GIB + MIB, now).await;
    let points = db::users::accounts::list_admin_activity_points(pool, 7)
        .await
        .unwrap();
    assert_eq!(points.len(), 7);
    assert_eq!(
        points
            .iter()
            .map(|point| point.daily_storage_bytes)
            .sum::<i64>(),
        MIB
    );
    let last = points.last().unwrap();
    assert_eq!(last.daily_storage_bytes, MIB);
    assert_eq!(last.cumulative_storage_bytes, MIB);
}

async fn check_activity_window_starts_at_midnight_utc(pool: &db::DbPool) {
    let owner = db::users::accounts::create_user(pool, "activity-owner", None)
        .await
        .unwrap();
    let repo = repository(pool, owner.id, "activity").await;
    // A seven-day window starts at midnight UTC six days ago.
    let first_day = Utc::now().date_naive() - Duration::days(6);
    let at = |day: NaiveDate, hour: u32, minute: u32, second: u32| {
        day.and_hms_opt(hour, minute, second).unwrap().and_utc()
    };
    let before = at(first_day - Duration::days(1), 23, 59, 59);
    let midnight = at(first_day, 0, 0, 0);
    let last_second = at(first_day, 23, 59, 59);
    for created_at in [before, midnight, last_second] {
        let issue = db::issues::create_issue(pool, repo.id, owner.id, "boundary", "body")
            .await
            .unwrap();
        set_created_at(pool, "issues", issue.id, created_at).await;
    }
    record_storage_snapshot(pool, repo.id, GIB, before).await;
    record_storage_snapshot(pool, repo.id, GIB + MIB, midnight).await;
    record_storage_snapshot(pool, repo.id, GIB + 3 * MIB, last_second).await;

    let points = db::users::accounts::list_admin_activity_points(pool, 7)
        .await
        .unwrap();
    assert_eq!(points.len(), 7);
    let first = &points[0];
    assert_eq!(first.day, first_day);
    // The issue just before midnight is outside the window; the two on the first day count.
    assert_eq!(first.daily_contributions, 2);
    // Growth on the first day is measured from the snapshot before midnight.
    assert_eq!(first.daily_storage_bytes, 3 * MIB);
    // The repository created today is the only contribution after the first day.
    assert_eq!(points.last().unwrap().cumulative_contributions, 3);
}

#[tokio::test]
async fn sqlite_storage_growth_counts_only_bytes_added_in_the_window() {
    let fixture = NativeDatabase::new().await;
    check_storage_growth_counts_only_bytes_added_in_the_window(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_storage_growth_counts_only_bytes_added_in_the_window() {
    let fixture = PostgresDatabase::new().await;
    check_storage_growth_counts_only_bytes_added_in_the_window(&fixture.pool).await;
    fixture.pool.close().await;
}

async fn check_activity_baselines_use_per_repository_lookups(pool: &db::DbPool) {
    let owner = db::users::accounts::create_user(pool, "history-owner", None)
        .await
        .unwrap();
    let active = repository(pool, owner.id, "active").await;
    let idle = repository(pool, owner.id, "idle").await;
    let fresh = repository(pool, owner.id, "fresh").await;
    let midnight = (Utc::now().date_naive() - Duration::days(6))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    match pool {
        db::DbPool::Postgres(raw) => {
            sqlx::query("INSERT INTO repo_storage_history(repo_id,size_bytes,recorded_at) SELECT CASE WHEN i%2=0 THEN $1 ELSE $2 END,$3,$4::timestamptz+i*interval '1 second' FROM generate_series(1,20000) i")
                .bind(active.id).bind(idle.id).bind(GIB).bind(midnight - Duration::days(30)).execute(raw).await.unwrap();
        }
        db::DbPool::Sqlite(raw) => {
            sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<20000) INSERT INTO repo_storage_history(repo_id,size_bytes,recorded_at) SELECT CASE WHEN i%2=0 THEN ?1 ELSE ?2 END,?3,strftime('%Y-%m-%dT%H:%M:%f000Z',?4,printf('%d seconds',i)) FROM n")
                .bind(active.id).bind(idle.id).bind(GIB).bind(timestamp(midnight - Duration::days(30))).execute(raw).await.unwrap();
        }
    }
    // Equal timestamps use the last ID, decreases add nothing, and new repos start at zero.
    record_storage_snapshot(pool, active.id, 2 * GIB, midnight - Duration::seconds(1)).await;
    record_storage_snapshot(pool, active.id, GIB, midnight - Duration::seconds(1)).await;
    record_storage_snapshot(pool, active.id, GIB + MIB, midnight).await;
    record_storage_snapshot(pool, active.id, GIB, midnight + Duration::seconds(1)).await;
    record_storage_snapshot(pool, active.id, GIB + MIB, midnight + Duration::seconds(1)).await;
    record_storage_snapshot(pool, fresh.id, 4 * MIB, midnight).await;
    let points = db::users::accounts::list_admin_activity_points(pool, 7)
        .await
        .unwrap();
    assert_eq!(points.len(), 7);
    assert_eq!(points[0].daily_storage_bytes, 6 * MIB);
    assert_eq!(points.last().unwrap().cumulative_storage_bytes, 6 * MIB);

    // Keep plan coverage tied to the SQL executed by the database function.
    let source = match pool {
        db::DbPool::Postgres(_) => include_str!("../../src/backend/db/postgres/users/accounts.rs"),
        db::DbPool::Sqlite(_) => include_str!("../../src/backend/db/sqlite/users/accounts.rs"),
    };
    let sql = source
        .split_once("pub async fn list_admin_activity_points(")
        .unwrap()
        .1
        .split_once("r#\"")
        .unwrap()
        .1
        .split_once("\"#")
        .unwrap()
        .0;
    match pool {
        db::DbPool::Postgres(raw) => {
            sqlx::query("ANALYZE repo_storage_history")
                .execute(raw)
                .await
                .unwrap();
            let plan: Value = sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"))
                .bind(7_i32)
                .fetch_one(raw)
                .await
                .unwrap();
            let nodes = postgres_plan_nodes(&plan);
            let history: Vec<_> = nodes
                .iter()
                .filter(|node| node["Relation Name"] == "repo_storage_history")
                .collect();
            // Each in-window repository runs the predecessor lookup once. PostgreSQL may
            // walk either index for it, so this check bounds only the other history scans.
            let lookups: Vec<_> = history
                .iter()
                .filter(|node| node["Actual Loops"] == 2)
                .collect();
            assert_eq!(
                lookups.len(),
                1,
                "one predecessor lookup per in-window repository: {plan}"
            );
            assert!(
                [&lookups[0]["Index Cond"], &lookups[0]["Filter"]]
                    .iter()
                    .any(|condition| condition
                        .as_str()
                        .is_some_and(|text| text.contains("repo_id ="))),
                "{plan}"
            );
            for node in history.iter().filter(|node| node["Actual Loops"] != 2) {
                let examined = (node["Actual Rows"].as_f64().unwrap_or(0.0)
                    + node["Rows Removed by Filter"].as_f64().unwrap_or(0.0))
                    * node["Actual Loops"].as_f64().unwrap_or(1.0);
                assert!(examined < 100.0, "history must not be scanned: {plan}");
            }
        }
        db::DbPool::Sqlite(raw) => {
            sqlx::query("ANALYZE repo_storage_history")
                .execute(raw)
                .await
                .unwrap();
            let plan: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(&format!("EXPLAIN QUERY PLAN {sql}"))
                    .bind(7_i32)
                    .fetch_all(raw)
                    .await
                    .unwrap();
            assert!(
                plan.iter()
                    .any(|row| row.3.contains("CORRELATED SCALAR SUBQUERY")),
                "{plan:?}"
            );
            assert!(
                plan.iter().any(|row| row
                    .3
                    .contains("idx_repo_storage_history_repo_time (repo_id=? AND recorded_at<?)")),
                "{plan:?}"
            );
            assert!(
                !plan
                    .iter()
                    .any(|row| row.3.contains("SCAN repo_storage_history")),
                "{plan:?}"
            );
        }
    }
}

#[tokio::test]
async fn sqlite_activity_baselines_use_per_repository_lookups() {
    let fixture = NativeDatabase::new().await;
    check_activity_baselines_use_per_repository_lookups(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_activity_baselines_use_per_repository_lookups() {
    let fixture = PostgresDatabase::new().await;
    check_activity_baselines_use_per_repository_lookups(&fixture.pool).await;
    fixture.pool.close().await;
}

#[tokio::test]
async fn sqlite_activity_window_starts_at_midnight_utc() {
    let fixture = NativeDatabase::new().await;
    check_activity_window_starts_at_midnight_utc(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test]
async fn postgres_activity_window_starts_at_midnight_utc() {
    let fixture = PostgresDatabase::new().await;
    check_activity_window_starts_at_midnight_utc(&fixture.pool).await;
    fixture.pool.close().await;
}

fn issue_titles(entries: Vec<db::issues::IssueListEntry>) -> Vec<String> {
    let mut titles = entries
        .into_iter()
        .map(|entry| entry.issue.title)
        .collect::<Vec<_>>();
    titles.sort();
    titles
}

fn pull_request_titles(
    entries: Vec<db::pull_requests::records::PullRequestListEntry>,
) -> Vec<String> {
    let mut titles = entries
        .into_iter()
        .map(|entry| entry.pull_request.title)
        .collect::<Vec<_>>();
    titles.sort();
    titles
}

/// Counts and search are one rule per database, so both kinds share one fixture: the state
/// tabs and the search must agree with the list they label.
async fn check_issue_and_pull_request_counts_and_search(pool: &db::DbPool) {
    use db::{
        discussions::StateCounts,
        issues,
        pull_requests::records::{self, NewPullRequest},
    };

    let owner = setup(pool, "public").await;
    let repo = repository(pool, owner.id, "searchable").await;
    for (title, body) in [
        ("Crash on startup", "steps"),
        ("Docs: update README", "mentions the startup flow"),
        ("Fix 50% slowdown", ""),
        ("axb wildcard", ""),
        ("Menu crash", "Café menu"),
    ] {
        issues::create_issue(pool, repo.id, owner.id, title, body)
            .await
            .unwrap();
    }
    let closed = issues::create_issue(pool, repo.id, owner.id, "a_b name", "")
        .await
        .unwrap();
    issues::update_issue(
        pool,
        closed.id,
        DiscussionEdit {
            actor_id: owner.id,
            expected_state: "open",
            expected_updated: closed.updated_at,
            title: None,
            body: None,
            state: Some("closed"),
        },
    )
    .await
    .unwrap()
    .expect("the issue closes at its revision");

    assert_eq!(
        issues::count_repo_issues(pool, repo.id, None)
            .await
            .unwrap(),
        StateCounts {
            open: 5,
            closed: 1,
            all: 6
        }
    );
    let counts = |search| issues::count_repo_issues(pool, repo.id, search);
    assert_eq!(
        counts(Some("startup")).await.unwrap(),
        StateCounts {
            open: 2,
            closed: 0,
            all: 2
        }
    );
    assert_eq!(counts(Some("zzz")).await.unwrap(), StateCounts::default());
    // Title and body both match, case folds for ASCII, and the text is a substring.
    let repo_id = repo.id;
    let search = |state, text| async move {
        issues::list_repo_issues(
            pool,
            repo_id,
            &ListFilter {
                state,
                search: text,
                ..Default::default()
            },
            100,
        )
        .await
    };
    assert_eq!(
        issue_titles(search(None, Some("STARTUP")).await.unwrap()),
        ["Crash on startup", "Docs: update README"]
    );
    assert_eq!(
        issue_titles(search(None, Some("mentions")).await.unwrap()),
        ["Docs: update README"]
    );
    assert_eq!(
        issue_titles(search(None, Some("café")).await.unwrap()),
        ["Menu crash"]
    );
    assert_eq!(
        issue_titles(search(Some("closed"), Some("startup")).await.unwrap()),
        Vec::<String>::new()
    );
    // LIKE wildcards in the search text match only themselves.
    assert_eq!(
        issue_titles(search(None, Some("50%")).await.unwrap()),
        ["Fix 50% slowdown"]
    );
    assert_eq!(
        issue_titles(search(None, Some("%")).await.unwrap()),
        ["Fix 50% slowdown"]
    );
    assert_eq!(
        issue_titles(search(None, Some("_")).await.unwrap()),
        ["a_b name"]
    );
    assert_eq!(
        issue_titles(search(Some("closed"), Some("a_b")).await.unwrap()),
        ["a_b name"]
    );
    assert_eq!(
        issues::list_repo_issues(
            pool,
            repo.id,
            &ListFilter {
                search: Some("crash"),
                ..Default::default()
            },
            1
        )
        .await
        .unwrap()
        .len(),
        1
    );

    // Pull requests follow the same rules; a merged or closed one is in `all` only when the
    // state filter allows it.
    let mut pull_requests = Vec::new();
    for (title, body, head) in [
        ("Retry uploads", "backoff", "retry"),
        ("Docs refresh", "startup notes", "docs"),
        ("Legacy crash", "", "legacy"),
    ] {
        pull_requests.push(
            records::create_pull_request(
                pool,
                NewPullRequest {
                    head_repo_id: None,
                    repo_id: repo.id,
                    author_id: owner.id,
                    title,
                    body,
                    head_branch: head,
                    head_sha: "1111111111111111111111111111111111111111",
                    base_branch: "main",
                    base_sha: "0000000000000000000000000000000000000000",
                    is_draft: false,
                },
            )
            .await
            .unwrap(),
        );
    }
    let legacy = &pull_requests[2];
    records::update_pull_request(
        pool,
        legacy.id,
        DiscussionEdit {
            actor_id: owner.id,
            expected_state: "open",
            expected_updated: legacy.updated_at,
            title: None,
            body: None,
            state: Some("closed"),
        },
        None,
    )
    .await
    .unwrap()
    .expect("the pull request closes at its revision");

    assert_eq!(
        records::count_repo_pull_requests(pool, repo.id, None)
            .await
            .unwrap(),
        StateCounts {
            open: 2,
            closed: 1,
            all: 3
        }
    );
    assert_eq!(
        records::count_repo_pull_requests(pool, repo.id, Some("crash"))
            .await
            .unwrap(),
        StateCounts {
            open: 0,
            closed: 1,
            all: 1
        }
    );
    let pulls = |state, text| async move {
        records::list_repo_pull_requests(
            pool,
            repo_id,
            &ListFilter {
                state,
                search: text,
                ..Default::default()
            },
            100,
        )
        .await
    };
    assert_eq!(
        pull_request_titles(pulls(None, Some("startup")).await.unwrap()),
        ["Docs refresh"]
    );
    assert_eq!(
        pull_request_titles(pulls(Some("open"), Some("crash")).await.unwrap()),
        Vec::<String>::new()
    );
    assert_eq!(
        pull_request_titles(pulls(Some("closed"), Some("CRASH")).await.unwrap()),
        ["Legacy crash"]
    );
    assert!(pulls(None, Some("%")).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_issue_and_pull_request_counts_and_search() {
    let fixture = NativeDatabase::new().await;
    check_issue_and_pull_request_counts_and_search(&fixture.pool).await;
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_issue_and_pull_request_counts_and_search() {
    let fixture = PostgresDatabase::new().await;
    check_issue_and_pull_request_counts_and_search(&fixture.pool).await;
    fixture.pool.close().await;
}
