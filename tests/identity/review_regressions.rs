//! Review, profile and session regressions on both database engines.
use crate::support::{
    database::Database, fake_webui_bundle, native_database::token, pull_requests::Client,
    PASSWORD_WORK,
};
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db, identity,
    notifications::{NotificationBroadcaster, NotificationEvent},
    storage::ObjectStore,
};
use serde_json::json;
use std::sync::Arc;

async fn session(pool: &db::DbPool, user: i64) -> String {
    let id = token();
    db::users::sessions::create_session(
        pool,
        &id,
        user,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    id
}

async fn signup_review(pg: bool) {
    use db::users::{
        signup_requests as requests,
        transitions::{self, AccountAdmission},
    };
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed,node_mode) VALUES(1,TRUE,'private') ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE,node_mode='private'", &[]).await;
    let admin = f.actor("review_admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin])
        .await;
    let first = requests::create_signup_request(
        f.pool(),
        "Älice",
        Some("FIRST@example.test"),
        "secret-verifier",
    )
    .await
    .unwrap();
    assert!(
        requests::create_signup_request(f.pool(), "älice", Some("other@example.test"), "hash")
            .await
            .is_err()
    );
    assert!(
        requests::create_signup_request(f.pool(), "other", Some("first@EXAMPLE.test"), "hash")
            .await
            .is_err()
    );
    assert_eq!(
        requests::get_pending_signup_request_by_identity(f.pool(), "äLICE", None)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    assert!(matches!(
        transitions::register_password(f.pool(), "äLICE", None, "hash", None)
            .await
            .unwrap(),
        AccountAdmission::Conflict
    ));
    for n in 0..60 {
        requests::create_signup_request(
            f.pool(),
            &format!("applicant-{n:03}"),
            None,
            "secret-verifier",
        )
        .await
        .unwrap();
    }
    assert_eq!(
        requests::count_signup_requests(f.pool(), "pending")
            .await
            .unwrap(),
        61
    );
    let first_page = requests::list_signup_requests(f.pool(), "pending", 30, 0, 0)
        .await
        .unwrap();
    let second_page = requests::list_signup_requests(f.pool(), "pending", 30, 30, 0)
        .await
        .unwrap();
    assert_eq!((first_page.len(), second_page.len()), (30, 30));
    assert!(first_page
        .iter()
        .all(|a| second_page.iter().all(|b| a.id != b.id)));
    assert!(!serde_json::to_string(&first_page)
        .unwrap()
        .contains("secret-verifier"));
    assert_eq!(
        requests::list_signup_requests(f.pool(), "pending", i64::MAX, 0, 0)
            .await
            .unwrap()
            .len(),
        61
    );
    db::users::accounts::create_user(f.pool(), "Älice", None)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ));
    let client = Client::login(&router, f.pool(), admin).await;
    let (status, page) = client.model("/settings?tab=access&request_page=2").await;
    assert_eq!(status, StatusCode::OK);
    let codeyard::webui::app::Page::Identity(codeyard::webui::pages::account::Page::Settings {
        model,
        ..
    }) = page
    else {
        panic!("settings")
    };
    assert_eq!(
        (
            model.requests.len(),
            model.requests_count,
            model.requests_page
        ),
        (30, 61, 2)
    );
    let (status, result) = client
        .api(
            "GET",
            "/api/auth/admin/signup-requests?page=3",
            serde_json::Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result.as_array().unwrap().len(), 1);
    let (status, result) = client
        .api(
            "POST",
            "/api/auth/admin/signup-requests/resolve",
            json!({"request_id":first.id,"action":"approve"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    assert_eq!(
        requests::get_pending_signup_request_by_identity(f.pool(), "Älice", None)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_signup_review_is_bounded_unicode_consistent_and_reports_conflicts() {
    signup_review(false).await;
}
#[tokio::test]
async fn postgres_signup_review_is_bounded_unicode_consistent_and_reports_conflicts() {
    signup_review(true).await;
}

async fn profile_and_follow(pg: bool) {
    let mut f = Database::new(pg).await;
    let actor = f.actor("profile_actor").await;
    let other = f.actor("profile_other").await;
    let original = session(f.pool(), actor).await;
    let input = || identity::profiles::ProfileUpdate {
        display_name: None,
        bio: Some("new bio"),
        location: None,
        website: None,
    };
    identity::profiles::update(f.pool(), actor, &original, input())
        .await
        .unwrap();
    db::users::sessions::delete_session(f.pool(), &original)
        .await
        .unwrap();
    assert_eq!(
        identity::profiles::update(f.pool(), actor, &original, input())
            .await
            .unwrap_err(),
        identity::profiles::ProfileError::Denied
    );
    let fresh = session(f.pool(), actor).await;
    for restriction in ["is_banned", "must_change_password"] {
        f.execute(
            &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
            &[actor],
        )
        .await;
        assert_eq!(
            identity::profiles::update(f.pool(), actor, &fresh, input())
                .await
                .unwrap_err(),
            identity::profiles::ProfileError::Denied
        );
        assert!(db::following::follow_user(f.pool(), actor, other)
            .await
            .unwrap_err()
            .is::<db::following::FollowRestricted>());
        f.execute(
            &format!("UPDATE users SET {restriction}=FALSE WHERE id=$1"),
            &[actor],
        )
        .await;
    }
    assert!(!db::following::is_user_following(f.pool(), actor, other)
        .await
        .unwrap());
    assert!(db::following::follow_user(f.pool(), actor, other)
        .await
        .unwrap());
    f.finish().await;
}
#[tokio::test]
async fn sqlite_profile_and_follows_require_current_authority() {
    profile_and_follow(false).await;
}
#[tokio::test]
async fn postgres_profile_and_follows_require_current_authority() {
    profile_and_follow(true).await;
}

#[tokio::test]
async fn sqlite_admin_search_matches_unicode_without_interpreting_regex_syntax() {
    let mut f = Database::new(false).await;
    f.actor("Émil").await;
    for query in ["Émil", "émil", "ÉMIL"] {
        assert_eq!(
            db::users::accounts::count_matching_users(f.pool(), query)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            db::users::accounts::list_admin_users(f.pool(), query, 30, 0)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    assert_eq!(
        db::users::accounts::count_matching_users(f.pool(), ".*")
            .await
            .unwrap(),
        0
    );
    f.finish().await;
}

async fn totp_rollback(pg: bool) {
    use db::users::{
        sessions::{self, SessionCredential},
        two_factor::{self, FactorAuthority, StoredTwoFactor},
    };
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let actor =
        db::users::accounts::create_user_with_password(f.pool(), "totp_actor", None, "hash")
            .await
            .unwrap();
    let session = session(f.pool(), actor.id).await;
    let stored = StoredTwoFactor {
        secret_encrypted: vec![1],
        nonce: vec![2; 12],
        last_used_step: Some(1),
    };
    two_factor::enable_two_factor(
        f.pool(),
        actor.id,
        FactorAuthority::Password {
            session: &session,
            expected: "hash",
        },
        &stored,
        &[],
    )
    .await
    .unwrap();
    let fresh = token();
    sessions::create_browser_session(
        f.pool(),
        &fresh,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    let proof = SessionCredential::SecondFactor {
        password: "hash",
        recovery_hash: None,
        totp: Some((&stored.nonce, 2)),
    };
    assert!(sessions::rotate_authenticated_session(
        f.pool(),
        None,
        &fresh,
        actor.id,
        proof,
        None,
        Utc::now() + Duration::hours(1)
    )
    .await
    .is_err());
    assert_eq!(
        two_factor::get_two_factor(f.pool(), actor.id)
            .await
            .unwrap()
            .unwrap()
            .last_used_step,
        Some(1)
    );
    assert!(sessions::rotate_authenticated_session(
        f.pool(),
        None,
        &token(),
        actor.id,
        proof,
        None,
        Utc::now() + Duration::hours(1)
    )
    .await
    .unwrap()
    .is_some());
    assert!(sessions::rotate_authenticated_session(
        f.pool(),
        None,
        &token(),
        actor.id,
        proof,
        None,
        Utc::now() + Duration::hours(1)
    )
    .await
    .unwrap()
    .is_none());
    f.finish().await;
}
#[tokio::test]
async fn sqlite_totp_consumption_rolls_back_with_failed_session_issuance() {
    totp_rollback(false).await;
}
#[tokio::test]
async fn postgres_totp_consumption_rolls_back_with_failed_session_issuance() {
    totp_rollback(true).await;
}

async fn temporary_password(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut f = Database::new(pg).await;
    let hash = identity::passwords::hash_password("temporary password")
        .await
        .unwrap();
    let user = db::users::accounts::create_user_with_password_and_flags(
        f.pool(),
        "managed_user",
        None,
        &hash,
        false,
        true,
    )
    .await
    .unwrap();
    let session = session(f.pool(), user.id).await;
    assert_eq!(
        identity::accounts::change_password(
            f.pool(),
            Some(&session),
            identity::accounts::PasswordChange {
                current_password: "temporary password",
                new_password: "temporary password"
            }
        )
        .await
        .unwrap_err(),
        identity::accounts::PasswordChangeError::UnchangedPassword
    );
    assert!(
        db::users::accounts::get_user_by_id(f.pool(), user.id)
            .await
            .unwrap()
            .unwrap()
            .must_change_password
    );
    assert!(db::users::sessions::get_session(f.pool(), &session)
        .await
        .unwrap()
        .is_some());
    identity::accounts::change_password(
        f.pool(),
        Some(&session),
        identity::accounts::PasswordChange {
            current_password: "temporary password",
            new_password: "different password",
        },
    )
    .await
    .unwrap();
    f.finish().await;
}
#[tokio::test]
async fn sqlite_temporary_password_must_be_replaced() {
    temporary_password(false).await;
}
#[tokio::test]
async fn postgres_temporary_password_must_be_replaced() {
    temporary_password(true).await;
}

async fn audit_changes(pg: bool) {
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed,node_mode) VALUES(1,TRUE,'private') ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE,node_mode='private'", &[]).await;
    let actor = f.actor("audit_admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[actor])
        .await;
    let session = session(f.pool(), actor).await;
    let admitted = db::users::transitions::register_password(
        f.pool(),
        "new_admin",
        None,
        "hash",
        Some((actor, &session, true)),
    )
    .await
    .unwrap();
    let db::users::transitions::AccountAdmission::User(user) = admitted else {
        panic!("managed user")
    };
    assert!(user.is_admin);
    assert!(
        db::node_config::set_ai_crawlers(f.pool(), actor, &session, "allow")
            .await
            .unwrap()
    );
    let node = db::cluster::register_own_node(
        f.pool(),
        &"a1".repeat(32),
        "audit_node",
        &["web".into()],
        "test",
    )
    .await
    .unwrap();
    for status in ["revoked", "active"] {
        db::cluster::set_node_status(
            f.pool(),
            db::cluster::Authority::Administrator {
                user_id: actor,
                session: &session,
            },
            "audit_node",
            status,
        )
        .await
        .unwrap();
    }
    let query = "SELECT action,target_id FROM audit_log WHERE actor_id=$1 ORDER BY id";
    let rows: Vec<(String, i64)> = match f.pool() {
        db::DbPool::Postgres(raw) => sqlx::query_as(query)
            .bind(actor)
            .fetch_all(raw)
            .await
            .unwrap(),
        db::DbPool::Sqlite(raw) => sqlx::query_as(query)
            .bind(actor)
            .fetch_all(raw)
            .await
            .unwrap(),
    };
    assert_eq!(
        rows,
        vec![
            ("user.create".into(), user.id),
            ("node.ai_crawlers".into(), 1),
            ("node.status".into(), node.id),
            ("node.status".into(), node.id)
        ]
    );
    f.execute(
        "CREATE UNIQUE INDEX one_creation_audit ON audit_log(action) WHERE action='user.create'",
        &[],
    )
    .await;
    assert!(db::users::transitions::register_password(
        f.pool(),
        "audit_failure",
        None,
        "hash",
        Some((actor, &session, true))
    )
    .await
    .is_err());
    assert!(
        db::users::accounts::get_user_by_username(f.pool(), "audit_failure")
            .await
            .unwrap()
            .is_none()
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_privileged_changes_audit_atomically() {
    audit_changes(false).await;
}
#[tokio::test]
async fn postgres_privileged_changes_audit_atomically() {
    audit_changes(true).await;
}

async fn activity_scan(pg: bool) {
    let mut f = Database::new(pg).await;
    let user = f.actor("empty_history").await;
    let empty = db::activity::list_user_activity(f.pool(), user, None, 100, 0)
        .await
        .unwrap();
    assert_eq!(empty.scanned, 0);
    assert!(empty.rows.is_empty());
    let repo = db::repositories::records::create_repository(
        f.pool(),
        user,
        "private_history",
        None,
        true,
        "/owned/scan.git",
    )
    .await
    .unwrap();
    for _ in 0..101 {
        db::activity::record_repository_push(f.pool(), repo.id, user, "main", 1)
            .await
            .unwrap();
    }
    let hidden = db::activity::list_user_activity(f.pool(), user, None, 100, 0)
        .await
        .unwrap();
    assert_eq!(hidden.scanned, 100);
    assert!(hidden.rows.is_empty());
    let exhausted = db::activity::list_user_activity(f.pool(), user, None, 100, 100)
        .await
        .unwrap();
    assert!(exhausted.scanned < 100 && exhausted.scanned > 0);
    assert!(exhausted.rows.is_empty());
    let eof = db::activity::list_user_activity(f.pool(), user, None, 100, 200)
        .await
        .unwrap();
    assert_eq!(eof.scanned, 0);
    f.finish().await;
}
#[tokio::test]
async fn sqlite_activity_reports_raw_exhaustion_not_authorized_row_count() {
    activity_scan(false).await;
}
#[tokio::test]
async fn postgres_activity_reports_raw_exhaustion_not_authorized_row_count() {
    activity_scan(true).await;
}

#[tokio::test]
async fn sqlite_activity_and_cleanup_queries_seek_their_indexes() {
    let mut f = Database::new(false).await;
    let db::DbPool::Sqlite(raw) = f.pool() else {
        unreachable!()
    };
    for (query,index) in [
        ("SELECT SUM(commit_count) FROM repository_pushes WHERE created_at>'2026-01-01'", "idx_repository_pushes_created_at"),
        ("SELECT COUNT(*) FROM ci_runs WHERE created_at>'2026-01-01' AND triggered_by_id IS NOT NULL AND event='push'", "idx_ci_runs_push_created_at"),
        ("SELECT id FROM signup_requests WHERE status<>'pending' AND password_hash<>'' LIMIT 500", "idx_signup_requests_processed_hash"),
    ] {
        let plan: Vec<(i64,i64,i64,String)> = sqlx::query_as(&format!("EXPLAIN QUERY PLAN {query}")).fetch_all(raw).await.unwrap();
        assert!(plan.iter().any(|row| row.3.contains(index)), "{plan:?}");
    }
    f.finish().await;
}

async fn wait_blocked(raw: &sqlx::PgPool, blocker: i32) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)))").bind(blocker).fetch_one(raw).await.unwrap();
            if blocked { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn postgres_factor_reset_locks_the_account_before_the_factor() {
    use db::users::two_factor::{self, FactorAuthority, StoredTwoFactor};
    let mut f = Database::new(true).await;
    let actor = f.actor("factor_admin").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[actor])
        .await;
    let admin_session = session(f.pool(), actor).await;
    let target =
        db::users::accounts::create_user_with_password(f.pool(), "factor_target", None, "hash")
            .await
            .unwrap();
    let target_session = session(f.pool(), target.id).await;
    two_factor::enable_two_factor(
        f.pool(),
        target.id,
        FactorAuthority::Password {
            session: &target_session,
            expected: "hash",
        },
        &StoredTwoFactor {
            secret_encrypted: vec![1],
            nonce: vec![2; 12],
            last_used_step: Some(1),
        },
        &[identity::two_factor::recovery_code_hash("recovery")],
    )
    .await
    .unwrap();
    let db::DbPool::Postgres(raw) = f.pool() else {
        unreachable!()
    };
    let mut factor_lock = raw.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *factor_lock)
        .await
        .unwrap();
    sqlx::query("SELECT user_id FROM two_factor_auth WHERE user_id=$1 FOR UPDATE")
        .bind(target.id)
        .execute(&mut *factor_lock)
        .await
        .unwrap();
    let pool = f.pool().clone();
    let reset = tokio::spawn(async move {
        two_factor::disable_two_factor(
            &pool,
            target.id,
            FactorAuthority::Administrator {
                user_id: actor,
                session: &admin_session,
            },
        )
        .await
    });
    wait_blocked(raw, pid).await;
    let mut observer = raw.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout='150ms'")
        .execute(&mut *observer)
        .await
        .unwrap();
    assert!(
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR SHARE")
            .bind(target.id)
            .execute(&mut *observer)
            .await
            .is_err(),
        "reset must already hold the account lock while waiting for the factor"
    );
    observer.rollback().await.unwrap();
    factor_lock.commit().await.unwrap();
    assert!(reset.await.unwrap().unwrap());
    f.finish().await;
}

#[tokio::test]
async fn postgres_credential_enrollment_rechecks_expiry_after_the_session_lock_wait() {
    let mut f = Database::new(true).await;
    let actor = f.actor("expiry_actor").await;
    for ssh in [false, true] {
        let session = session(f.pool(), actor).await;
        let db::DbPool::Postgres(raw) = f.pool() else {
            unreachable!()
        };
        let mut lock = raw.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET expires_at=clock_timestamp()+INTERVAL '400 milliseconds' WHERE id=$1").bind(&session).execute(&mut *lock).await.unwrap();
        let pool = f.pool().clone();
        let enroll = tokio::spawn(async move {
            if ssh {
                db::users::ssh_keys::add_ssh_key_for_session(&pool, actor, &session, "expired", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl").await.is_err()
            } else {
                db::users::tokens::create_token(
                    &pool, actor, &session, "expired", "hash", "read", None,
                )
                .await
                .unwrap()
                .is_none()
            }
        });
        wait_blocked(raw, pid).await;
        sqlx::query("SELECT pg_sleep(0.5)")
            .execute(&mut *lock)
            .await
            .unwrap();
        lock.commit().await.unwrap();
        assert!(enroll.await.unwrap());
    }
    f.finish().await;
}

#[tokio::test]
async fn unrelated_notifications_do_not_fill_another_recipients_queue() {
    let mut f = Database::new(false).await;
    let broadcaster = NotificationBroadcaster::new(f.pool());
    let mut mine = broadcaster.subscribe(1);
    let mut other = broadcaster.subscribe(2);
    for _ in 0..1100 {
        broadcaster
            .send(NotificationEvent::ReadAll {
                user_id: 2,
                unread_count: 0,
            })
            .await;
    }
    broadcaster
        .send(NotificationEvent::ReadAll {
            user_id: 1,
            unread_count: 0,
        })
        .await;
    assert_eq!(mine.recv().await.unwrap().user_id(), 1);
    assert!(mine.try_recv().is_err());
    assert!(matches!(
        other.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
    ));
    f.finish().await;
}
