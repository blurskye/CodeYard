use crate::support::database::wait_until_blocked;
use crate::support::database::Database;

use codeyard_db_identity::users::accounts;
use codeyard_db_identity::users::transitions;
use codeyard_db_identity::users::transitions::BanChange;
use std::time::Duration;

async fn concurrent_account_transitions(pg: bool) {
    let mut fixture = Database::new(pg).await;
    let first = accounts::create_user_with_password(fixture.pool(), "first", None, "initial")
        .await
        .unwrap()
        .id;
    let second = accounts::create_user_with_password(fixture.pool(), "second", None, "initial")
        .await
        .unwrap()
        .id;
    let pool = fixture.pool();
    let (a, b) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            transitions::compare_exchange_password(
                pool, first, "initial", "first", true, false, None
            ),
            transitions::compare_exchange_password(
                pool, second, "initial", "second", true, false, None
            ),
        )
    })
    .await
    .expect("both unrelated account transitions finish");
    assert!(a.unwrap().is_some());
    assert!(b.unwrap().is_some());

    let (a, b) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            transitions::compare_exchange_password(
                pool, first, "first", "winner-a", true, false, None
            ),
            transitions::compare_exchange_password(
                pool, first, "first", "winner-b", true, false, None
            ),
        )
    })
    .await
    .expect("same-account transitions serialize without deadlock");
    assert_ne!(a.unwrap().is_some(), b.unwrap().is_some());
    let stored = accounts::get_user_by_id(pool, first)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        stored.password_hash.as_deref(),
        Some("winner-a" | "winner-b")
    ));

    if pg {
        let raw = fixture.pg();
        let mut gate = raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE")
            .bind(first)
            .execute(&mut *gate)
            .await
            .unwrap();
        let pool = fixture.pool().clone();
        let blocked = tokio::spawn(async move {
            accounts::update_user_password(&pool, first, "blocked-first", false).await
        });
        wait_until_blocked(raw, holder, 1).await;

        // The first transition now owns its advisory lock but is waiting on its row.
        // Another account must not wait for it, while a second change to it must.
        tokio::time::timeout(
            Duration::from_secs(10),
            accounts::update_user_password(fixture.pool(), second, "independent", false),
        )
        .await
        .expect("an unrelated transition must not wait for the blocked account")
        .unwrap();
        let pool = fixture.pool().clone();
        let serialized = tokio::spawn(async move {
            accounts::update_user_password(&pool, first, "blocked-second", false).await
        });
        wait_until_blocked(raw, holder, 2).await;
        assert!(!blocked.is_finished());
        assert!(!serialized.is_finished());
        gate.rollback().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            blocked.await.unwrap().unwrap();
            serialized.await.unwrap().unwrap();
        })
        .await
        .expect("both account changes finish after the barrier is released");
        assert_eq!(
            accounts::get_user_by_id(fixture.pool(), first)
                .await
                .unwrap()
                .unwrap()
                .password_hash
                .as_deref(),
            Some("blocked-second")
        );
    }
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_concurrent_account_transitions() {
    concurrent_account_transitions(false).await;
}

#[tokio::test]
async fn postgres_concurrent_account_transitions() {
    concurrent_account_transitions(true).await;
}

async fn reciprocal_bans_recheck_live_authority(pg: bool) {
    let mut fixture = Database::new(pg).await;
    let first = fixture.actor("first_admin").await;
    let second = fixture.actor("second_admin").await;
    fixture
        .execute(
            "UPDATE users SET is_admin=TRUE WHERE id IN ($1,$2)",
            &[first, second],
        )
        .await;
    let pool = fixture.pool();
    let (a, b) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            transitions::change_ban(pool, first, second, true),
            transitions::change_ban(pool, second, first, true),
        )
    })
    .await
    .expect("opposing account pairs must use one lock order");
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, BanChange::Updated(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, BanChange::Denied))
            .count(),
        1
    );
    assert_eq!(accounts::count_unbanned_admin_users(pool).await.unwrap(), 1);
    let banned = accounts::get_user_by_id(pool, first)
        .await
        .unwrap()
        .unwrap()
        .is_banned;
    let (revoked, active) = if banned {
        (first, second)
    } else {
        (second, first)
    };
    assert!(matches!(
        transitions::change_admin_role(pool, revoked, active, false)
            .await
            .unwrap(),
        transitions::RoleChange::Denied
    ));
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_reciprocal_bans_recheck_live_authority() {
    reciprocal_bans_recheck_live_authority(false).await;
}

#[tokio::test]
async fn postgres_reciprocal_bans_recheck_live_authority() {
    reciprocal_bans_recheck_live_authority(true).await;
}

async fn app_owner_revocation_rechecks_bot_writes(pg: bool) {
    let mut fixture = Database::new(pg).await;
    let admin = fixture.actor("admin").await;
    let owner = fixture.actor("app_owner").await;
    let bot = fixture.actor("app_bot").await;
    fixture
        .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[admin])
        .await;
    let repo = codeyard_db_repos::repositories::records::create_repository(
        fixture.pool(),
        admin,
        "private",
        None,
        true,
        "/unused/app-owner.git",
    )
    .await
    .unwrap();
    fixture.execute("INSERT INTO oauth_applications(owner_id,client_id,secret_hash,name,homepage,redirect_uri,is_github_app) VALUES($1,'test-app','unused','App','','',TRUE)", &[owner]).await;
    fixture.execute("INSERT INTO github_apps(id,owner_id,bot_id,client_id,slug,name,description,homepage,public_key,permissions,events,access_level) VALUES(9000,$1,$2,'test-app','test-app','App','','','','{}','[]',3)", &[owner, bot]).await;
    fixture
        .execute(
            "INSERT INTO github_app_installations(app_id,user_id,all_repos) VALUES(9000,$1,TRUE)",
            &[admin],
        )
        .await;

    if pg {
        // A trusted bot call has no token issuer to retain its owner's availability.
        // A late-discovered owner must never introduce a reverse-order row wait.
        let mut gate = fixture.pg().begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE")
            .bind(owner)
            .execute(&mut *gate)
            .await
            .unwrap();
        assert!(tokio::time::timeout(
            Duration::from_secs(10),
            codeyard_db_repos::issues::create_issue(
                fixture.pool(),
                repo.id,
                bot,
                "Blocked",
                "Body"
            ),
        )
        .await
        .expect("late issuer discovery must fail closed, not wait")
        .is_err());
        gate.rollback().await.unwrap();
    }
    codeyard_db_repos::issues::create_issue(fixture.pool(), repo.id, bot, "Allowed", "Body")
        .await
        .unwrap();
    assert!(matches!(
        transitions::change_ban(fixture.pool(), admin, owner, true)
            .await
            .unwrap(),
        BanChange::Updated(_)
    ));
    assert!(codeyard_db_repos::issues::create_issue(
        fixture.pool(),
        repo.id,
        bot,
        "Revoked",
        "Body"
    )
    .await
    .is_err());
    assert_eq!(
        fixture
            .scalar("SELECT COUNT(*) FROM issues WHERE repo_id=$1", &[repo.id])
            .await,
        1
    );
    fixture.finish().await;
}

#[tokio::test]
async fn sqlite_app_owner_revocation_rechecks_bot_writes() {
    app_owner_revocation_rechecks_bot_writes(false).await;
}

#[tokio::test]
async fn postgres_app_owner_revocation_rechecks_bot_writes() {
    app_owner_revocation_rechecks_bot_writes(true).await;
}
