//! The cache retains home answers without creating credentials on stub accounts.
use crate::support::database::Database;
use codeyard_db_core::{
    cells::authorization::{self, Answer, Authority, Home, Proof},
    DbPool,
};
use codeyard_support::cells::Prefix;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
struct Provider {
    answer: Answer,
    live: AtomicBool,
    calls: AtomicUsize,
}
impl Home for Provider {
    fn authorize<'a>(
        &'a self,
        _proof: &'a Proof,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Option<Answer>>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .live
                .load(Ordering::SeqCst)
                .then(|| self.answer.clone()))
        })
    }
}
async fn expire(pool: &DbPool) {
    match pool {
        DbPool::Postgres(pool) => {
            sqlx::query("UPDATE foreign_credentials SET expires_at=validated_at")
                .execute(pool)
                .await
                .unwrap();
        }
        DbPool::Sqlite(pool) => {
            sqlx::query("UPDATE foreign_credentials SET expires_at=validated_at")
                .execute(pool)
                .await
                .unwrap();
        }
    }
}
async fn count(pool: &DbPool, table: &str) -> i64 {
    let query = format!("SELECT COUNT(*) FROM {table}");
    match pool {
        DbPool::Postgres(pool) => sqlx::query_scalar(&query).fetch_one(pool).await.unwrap(),
        DbPool::Sqlite(pool) => sqlx::query_scalar(&query).fetch_one(pool).await.unwrap(),
    }
}
async fn contract(pg: bool) {
    let mut database = Database::empty(pg).await;
    let pool = database.pool();
    let native = codeyard_db_identity::users::accounts::create_user(pool, "local", None)
        .await
        .unwrap();
    let mut remote = native.clone();
    remote.id = *Prefix::new(1).ids().start();
    remote.home_cell = 1;
    remote.username = "remote".into();
    remote.is_admin = false;
    remote.email = None;
    remote.password_hash = None;
    remote.avatar_file_id = None;
    remote.email_verified = false;
    let provider = Arc::new(Provider {
        answer: Answer {
            home: Prefix::new(1),
            user: remote.clone(),
            credential_id: Some(remote.id + 1),
            write: true,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            authority: Authority::Classic,
        },
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    });
    pool.set_foreign_authorization(provider.clone());
    let proof = Proof::Token {
        home: Prefix::new(1),
        hash: "a".repeat(64),
    };
    assert!(authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .is_some());
    let stub = codeyard_db_identity::users::accounts::get_user_by_id(pool, remote.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stub.is_stub && stub.home_cell == 1);
    assert_eq!(count(pool, "personal_access_tokens").await, 0);
    assert_eq!(count(pool, "sessions").await, 0);
    assert_eq!(count(pool, "ssh_keys").await, 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    provider.live.store(false, Ordering::SeqCst);
    assert!(authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .is_some());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    expire(pool).await;
    assert!(authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .is_none());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(authorization::authorize(
        pool,
        &Proof::Token {
            home: Prefix::new(1),
            hash: "hostile".into()
        }
    )
    .await
    .is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    // An authoritative peer still cannot convert an existing native row into a foreign account.
    let mut forged = provider.answer.clone();
    forged.user.id = native.id;
    forged.user.username = native.username.clone();
    pool.set_foreign_authorization(Arc::new(Provider {
        answer: forged,
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    }));
    assert!(authorization::authorize(
        pool,
        &Proof::Token {
            home: Prefix::new(1),
            hash: "b".repeat(64)
        }
    )
    .await
    .unwrap()
    .is_none());
    assert!(
        !codeyard_db_identity::users::accounts::get_user_by_id(pool, native.id)
            .await
            .unwrap()
            .unwrap()
            .is_stub
    );
    // Private metadata never crosses this cache boundary, even from an enrolled source.
    let mut private = provider.answer.clone();
    private.user.email = Some("private@example.test".into());
    pool.set_foreign_authorization(Arc::new(Provider {
        answer: private,
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    }));
    assert!(authorization::authorize(
        pool,
        &Proof::Token {
            home: Prefix::new(1),
            hash: "c".repeat(64)
        }
    )
    .await
    .is_err());
    assert_eq!(count(pool, "personal_access_tokens").await, 0);
    // If the cache write fails after metadata arrives, the account remains a stub with no credential.
    let mut half = provider.answer.clone();
    half.user.id += 20;
    half.user.username = "halfway".into();
    half.credential_id = Some(half.user.id + 1);
    pool.set_foreign_authorization(Arc::new(Provider {
        answer: half.clone(),
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    }));
    match pool {
        DbPool::Postgres(pool) => {
            sqlx::query("CREATE FUNCTION reject_foreign_answer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected cache failure'; END $$").execute(pool).await.unwrap();
            sqlx::query("CREATE TRIGGER reject_foreign_answer BEFORE INSERT ON foreign_credentials FOR EACH ROW EXECUTE FUNCTION reject_foreign_answer()").execute(pool).await.unwrap();
        }
        DbPool::Sqlite(pool) => {
            sqlx::query("CREATE TRIGGER reject_foreign_answer BEFORE INSERT ON foreign_credentials BEGIN SELECT RAISE(ABORT,'injected cache failure'); END").execute(pool).await.unwrap();
        }
    }
    assert!(authorization::authorize(
        pool,
        &Proof::Token {
            home: Prefix::new(1),
            hash: "d".repeat(64)
        }
    )
    .await
    .is_err());
    assert!(
        codeyard_db_identity::users::accounts::get_user_by_id(pool, half.user.id)
            .await
            .unwrap()
            .unwrap()
            .is_stub
    );
    assert_eq!(count(pool, "personal_access_tokens").await, 0);
    assert_eq!(count(pool, "sessions").await, 0);
    database.finish().await;
}
#[tokio::test]
async fn sqlite_foreign_answers_expire_and_do_not_create_local_credentials() {
    contract(false).await
}
#[tokio::test]
async fn postgres_foreign_answers_expire_and_do_not_create_local_credentials() {
    contract(true).await
}
