use crate::support::database::{wait_until_blocked, Database};
use codeyard::backend::db::users::{
    accounts,
    transitions::{self, BanChange},
};
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
