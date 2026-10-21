//! Account metadata, privileged mutations and enrollment retain their database authority.
use crate::support::{
    database::{wait_until_blocked, Database},
    native_database::token,
    PASSWORD_WORK,
};
use chrono::{Duration, Utc};
use codeyard::backend::{db, identity};

const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

async fn administration(pg: bool) {
    let mut f = Database::new(pg).await;
    let actor = f.actor("administrator").await;
    f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[actor])
        .await;
    let target = db::users::accounts::create_user_with_password(
        f.pool(),
        "target",
        Some("needle@example.test"),
        "hash",
    )
    .await
    .unwrap();
    f.execute("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<31) INSERT INTO users(username) SELECT 'listed-' || n FROM numbers", &[]).await;
    assert_eq!(
        db::users::accounts::count_matching_users(f.pool(), "LISTED-")
            .await
            .unwrap(),
        31
    );
    let first = db::users::accounts::list_admin_users(f.pool(), "listed-", 25, 0)
        .await
        .unwrap();
    let second = db::users::accounts::list_admin_users(f.pool(), "listed-", 25, 25)
        .await
        .unwrap();
    assert_eq!((first.len(), second.len()), (25, 6));
    assert!(first.iter().all(|a| second.iter().all(|b| a.id != b.id)));
    assert_eq!(
        db::users::accounts::count_matching_users(f.pool(), "needle@")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        db::users::accounts::count_matching_users(f.pool(), "%")
            .await
            .unwrap(),
        0
    );
    assert!(matches!(
        db::users::transitions::change_admin_role(f.pool(), actor, target.id, true)
            .await
            .unwrap(),
        db::users::transitions::RoleChange::Updated(_)
    ));
    assert!(matches!(
        db::users::transitions::change_ban(f.pool(), actor, target.id, true)
            .await
            .unwrap(),
        db::users::transitions::BanChange::Updated(_)
    ));
    let session = token();
    db::users::sessions::create_session(
        f.pool(),
        &session,
        actor,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    assert!(!db::users::two_factor::disable_two_factor(
        f.pool(),
        target.id,
        db::users::two_factor::FactorAuthority::Administrator {
            user_id: actor,
            session: &session
        },
    )
    .await
    .unwrap());
    let sql =
        "SELECT action,target_id FROM audit_log WHERE actor_id=$1 AND org_id IS NULL ORDER BY id";
    let rows: Vec<(String, i64)> = match f.pool() {
        db::DbPool::Postgres(pool) => sqlx::query_as(sql)
            .bind(actor)
            .fetch_all(pool)
            .await
            .unwrap(),
        db::DbPool::Sqlite(pool) => sqlx::query_as(sql)
            .bind(actor)
            .fetch_all(pool)
            .await
            .unwrap(),
    };
    assert_eq!(
        rows,
        vec![
            ("user.admin_role".into(), target.id),
            ("user.ban".into(), target.id),
            ("user.two_factor_reset".into(), target.id)
        ]
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_account_metadata_and_privileged_audit() {
    administration(false).await;
}
#[tokio::test]
async fn postgres_account_metadata_and_privileged_audit() {
    administration(true).await;
}

async fn blank_email(pg: bool) {
    let _work = PASSWORD_WORK.lock().await;
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let fresh = token();
    let registered = identity::registration::register_password(
        f.pool(),
        identity::credentials::PasswordAccount {
            username: "blank_mail",
            email: Some(""),
            password: "correct password",
        },
        identity::sessions::SessionContext {
            initiating: identity::sessions::InitiatingSession::Missing,
            fresh_id: &fresh,
            user_agent: None,
            client: None,
            expires_at: Utc::now() + Duration::hours(1),
        },
    )
    .await
    .unwrap();
    let identity::registration::RegistrationOutcome::Registered(account) = registered else {
        panic!("registration")
    };
    assert_eq!(account.user.email, None);
    assert_eq!(account.user.commit_email(), "blank_mail@users.noreply");
    f.execute("UPDATE users SET email='' WHERE id=$1", &[account.user.id])
        .await;
    let existing = db::users::accounts::get_user_by_id(f.pool(), account.user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(existing.commit_email(), "blank_mail@users.noreply");
    f.finish().await;
}
#[tokio::test]
async fn sqlite_blank_registration_email_has_commit_identity() {
    blank_email(false).await;
}
#[tokio::test]
async fn postgres_blank_registration_email_has_commit_identity() {
    blank_email(true).await;
}

#[tokio::test]
async fn postgres_credential_enrollment_holds_the_original_session_until_commit() {
    let mut f = Database::new(true).await;
    let user = f.actor("enrollment").await;
    let raw = f.pg();
    sqlx::raw_sql("CREATE FUNCTION wait_enrollment() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(hashtext(current_schema()),737701); RETURN NEW; END $$;")
        .execute(raw).await.unwrap();
    for table in ["personal_access_tokens", "ssh_keys"] {
        sqlx::query(&format!("CREATE TRIGGER wait_enrollment BEFORE INSERT ON {table} FOR EACH ROW EXECUTE FUNCTION wait_enrollment()"))
            .execute(raw).await.unwrap();
        let session = token();
        db::users::sessions::create_session(
            f.pool(),
            &session,
            user,
            None,
            None,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
        let mut gate = raw.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()),737701)")
            .execute(&mut *gate)
            .await
            .unwrap();
        let pool = f.pool().clone();
        let original = session.clone();
        let enrollment = tokio::spawn(async move {
            if table == "ssh_keys" {
                db::users::ssh_keys::add_ssh_key_for_session(&pool, user, &original, "laptop", KEY)
                    .await
                    .unwrap();
            } else {
                assert!(db::users::tokens::create_token(
                    &pool,
                    user,
                    &original,
                    "git",
                    "unique-hash",
                    "read",
                    None
                )
                .await
                .unwrap()
                .is_some());
            }
        });
        wait_until_blocked(raw, blocker, 1).await;
        let delete_pool = raw.clone();
        let revocation = tokio::spawn(async move {
            sqlx::query("DELETE FROM sessions WHERE id=$1")
                .bind(session)
                .execute(&delete_pool)
                .await
                .unwrap()
        });
        // The DELETE must wait on enrollment's session-row lock, not finish behind its back.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!revocation.is_finished());
        gate.commit().await.unwrap();
        enrollment.await.unwrap();
        revocation.await.unwrap();
        sqlx::query(&format!("DROP TRIGGER wait_enrollment ON {table}"))
            .execute(raw)
            .await
            .unwrap();
    }
    f.finish().await;
}

#[tokio::test]
async fn postgres_privileged_writes_observe_a_waited_for_demotion() {
    let mut f = Database::new(true).await;
    f.complete_setup().await;
    let actor = f.actor("revoked_admin").await;
    let target = f.actor("reset_target").await;
    let session = token();
    db::users::sessions::create_session(
        f.pool(),
        &session,
        actor,
        None,
        None,
        Utc::now() + Duration::hours(1),
    )
    .await
    .unwrap();
    for reset in [false, true] {
        f.execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[actor])
            .await;
        let raw = f.pg();
        let mut demotion = raw.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *demotion)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET is_admin=FALSE WHERE id=$1")
            .bind(actor)
            .execute(&mut *demotion)
            .await
            .unwrap();
        let pool = f.pool().clone();
        let original = session.clone();
        let mutation = tokio::spawn(async move {
            if reset {
                assert!(db::users::two_factor::disable_two_factor(
                    &pool,
                    target,
                    db::users::two_factor::FactorAuthority::Administrator {
                        user_id: actor,
                        session: &original
                    },
                )
                .await
                .is_err());
            } else {
                assert!(
                    !db::node_config::set_ai_crawlers(&pool, actor, &original, "allow")
                        .await
                        .unwrap()
                );
            }
        });
        wait_until_blocked(raw, blocker, 1).await;
        demotion.commit().await.unwrap();
        mutation.await.unwrap();
    }
    assert_eq!(
        db::node_config::get_node_configuration(f.pool())
            .await
            .unwrap()
            .unwrap()
            .ai_crawlers,
        "block"
    );
    f.finish().await;
}
