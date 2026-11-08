//! Heartbeat proofs belong to one operation and never create local credentials.
use crate::support::database::Database;
use codeyard_db_core::{
    cells::authorization::{self, Answer, Authority, Home, Proof},
    DbPool,
};
use codeyard_types::cells::Prefix;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc,
};
struct Provider {
    answer: Answer,
    live: AtomicBool,
    calls: AtomicUsize,
    /// Milliseconds each answer waits, for a home that is slow to answer.
    delay: AtomicU64,
}
impl Home for Provider {
    fn user<'a>(
        &'a self,
        name: &'a str,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Option<codeyard_db_core::models::User>>>
    {
        Box::pin(async move {
            Ok(
                (self.live.load(Ordering::SeqCst) && self.answer.user.username == name)
                    .then(|| self.answer.user.clone()),
            )
        })
    }
    fn authorize<'a>(
        &'a self,
        _proof: &'a Proof,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Option<Answer>>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let delay = self.delay.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            Ok(self.live.load(Ordering::SeqCst).then(|| {
                let mut answer = self.answer.clone();
                if answer.heartbeat == chrono::DateTime::<chrono::Utc>::MAX_UTC {
                    answer.heartbeat = chrono::Utc::now();
                }
                answer
            }))
        })
    }
}
async fn count(pool: &DbPool, table: &str) -> i64 {
    let query = format!("SELECT COUNT(*) FROM {table}");
    match pool {
        DbPool::Postgres(pool) => sqlx::query_scalar(&query).fetch_one(pool).await.unwrap(),
        DbPool::Sqlite(pool) => sqlx::query_scalar(&query).fetch_one(pool).await.unwrap(),
    }
}
async fn sessions(database: &Database, mut answer: Answer, owner: i64) {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let pool = database.pool();
    let csrf = "e".repeat(64);
    answer.credential_id = None;
    answer.authority = Authority::Session { csrf: csrf.clone() };
    let provider = Arc::new(Provider {
        answer,
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        delay: AtomicU64::new(0),
    });
    pool.set_foreign_authorization(provider.clone());
    let value = format!("1.{}", "f".repeat(64));
    let proof = Proof::Session {
        home: Prefix::new(1),
        value: value.clone(),
    };
    assert!(Proof::Session {
        home: Prefix::new(2),
        value: value.clone()
    }
    .validate()
    .is_err());
    let user = provider.answer.user.id;
    database.complete_setup().await;
    let root = tempfile::tempdir().unwrap();
    let repo = codeyard_repos_core::repositories::lifecycle::provision(
        (pool, None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "private",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    codeyard_db_repos::repositories::access::grant_repo_access(
        pool,
        user,
        repo.id,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    // Repository chrome counts linked packages with the same foreign browser proof.
    authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        codeyard_db_repos::packages::count_linked(
            pool,
            Some(codeyard_db_repos::packages::Credential::Session(&value)),
            repo.id
        )
        .await
        .unwrap(),
        0
    );
    let app = database.router(root.path()).await;
    let request = |method: &str, path: &str, supplied: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("host", "cell.test")
            .header("origin", "http://cell.test")
            .header("cookie", format!("session_id={value}"))
            .header("x-csrf-token", supplied)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"title":"foreign browser","body":"from home"}"#,
            ))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(request("GET", "/local/private/issues", &csrf))
        .await
        .unwrap();
    let status = response.status();
    let html = String::from_utf8(
        to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains(&csrf),
        "The resource page retains the home session's CSRF token"
    );
    authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        codeyard_db_identity::users::sessions::get_session_user(pool, &value)
            .await
            .unwrap()
            .unwrap()
            .id,
        user
    );
    let browser = codeyard_db_identity::users::sessions::get_browser_session(pool, &value)
        .await
        .unwrap()
        .expect("A non-expiring home credential still has a bounded browser proof");
    assert_eq!(browser.user_id, Some(user));
    assert!(browser.expires_at > chrono::Utc::now());
    assert!(browser.expires_at <= chrono::Utc::now() + chrono::Duration::seconds(2));
    assert!(
        codeyard_db_identity::users::sessions::get_or_create_csrf_token(
            pool,
            &value,
            Some(owner),
            &csrf,
            60
        )
        .await
        .unwrap()
        .is_none()
    );
    let response = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/repos/local/private/issues",
            &"0".repeat(64),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let response = app
        .clone()
        .oneshot(request("POST", "/api/repos/local/private/issues", &csrf))
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        database
            .scalar(
                "SELECT COUNT(*) FROM issues WHERE repo_id=$1 AND author_id=$2",
                &[repo.id, user]
            )
            .await,
        1
    );
    assert_eq!(count(pool, "sessions").await, 0);
    assert!(codeyard_db_identity::users::profiles::save_profile(
        pool,
        user,
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&value),
        &codeyard_db_identity::users::profiles::ProfilePatch {
            display_name: Some("local overwrite"),
            ..Default::default()
        },
    )
    .await
    .is_err());
    assert_eq!(
        codeyard_db_identity::users::accounts::get_user_by_id(pool, user)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        provider.answer.user.display_name
    );
    let credential = codeyard_db_core::queries::users::accounts::AccountCredential::Session(&value);
    assert!(codeyard_db_identity::users::emails::change_emails(
        pool,
        user,
        &credential,
        &["foreign@example.test".into()],
        true
    )
    .await
    .is_err());
    assert!(
        codeyard_db_identity::users::social_accounts::change_social_accounts(
            pool,
            user,
            &credential,
            &[
                codeyard_db_identity::users::social_accounts::SocialAccount {
                    provider: "web".into(),
                    url: "https://example.test/foreign".into()
                }
            ],
            true
        )
        .await
        .is_err()
    );
    assert!(codeyard_db_identity::users::blocks::change_block(
        pool,
        user,
        &credential,
        owner,
        true
    )
    .await
    .is_err());
    for table in ["user_emails", "user_social_accounts", "user_blocks"] {
        assert_eq!(
            count(pool, table).await,
            0,
            "foreign sessions cannot create account-local rows"
        );
    }
    database
        .execute(
            "DELETE FROM repo_access WHERE repo_id=$1 AND user_id=$2",
            &[repo.id, user],
        )
        .await;
    let invitation = codeyard_db_repos::repositories::collaborators::add(
        pool,
        owner,
        repo.id,
        &provider.answer.user.username,
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .unwrap();
    let codeyard_db_repos::repositories::collaborators::Added::Invited(invitation) = invitation
    else {
        panic!("A new collaborator must receive an invitation");
    };
    authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .unwrap();
    codeyard_db_repos::repositories::collaborators::respond(
        pool,
        (user, value.clone()),
        invitation.id,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        database
            .scalar(
                "SELECT access_level FROM repo_access WHERE repo_id=$1 AND user_id=$2",
                &[repo.id, user]
            )
            .await,
        3
    );
    provider.live.store(false, Ordering::SeqCst);
    assert!(authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .is_none());
    assert!(codeyard_db_repos::packages::count_linked(
        pool,
        Some(codeyard_db_repos::packages::Credential::Session(&value)),
        repo.id
    )
    .await
    .is_err());

    assert!(authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .is_none());
    codeyard_db_repos::repositories::access::grant_repo_access(
        pool,
        user,
        repo.id,
        codeyard_db_core::models::AccessLevel::Admin,
    )
    .await
    .unwrap();
    let target = database.actor("queued-target").await;
    let error = codeyard_db_repos::repositories::collaborators::add(
        pool,
        (user, value.clone()),
        repo.id,
        "queued-target",
        codeyard_db_core::models::AccessLevel::Write,
    )
    .await
    .err()
    .expect("An expired session cannot enqueue an invitation");
    assert!(matches!(
        error.downcast_ref::<codeyard_db_repos::repositories::collaborators::Refusal>(),
        Some(codeyard_db_repos::repositories::collaborators::Refusal::Denied)
    ));
    assert_eq!(
        database
            .scalar(
                "SELECT COUNT(*) FROM repository_invitations WHERE invitee_id=$1",
                &[target]
            )
            .await,
        0
    );
    // A queued operation retaining the formerly valid identity must fail at the effect too.
    assert!(codeyard_db_repos::issues::create_issue_with_metadata(
        pool,
        repo.id,
        (user, value.clone()),
        "stale browser",
        "",
        codeyard_db_repos::issues::IssueMetadata::default(),
    )
    .await
    .is_err());
    assert!(
        codeyard_db_identity::users::sessions::get_session_user(pool, &value)
            .await
            .unwrap()
            .is_none()
    );
    let response = app
        .oneshot(request("POST", "/api/repos/local/private/issues", &csrf))
        .await
        .unwrap();
    assert!(matches!(response.status().as_u16(), 401 | 403));
    assert_eq!(
        database
            .scalar("SELECT COUNT(*) FROM issues WHERE repo_id=$1", &[repo.id])
            .await,
        1
    );
}

/// Hold the stub row, not the proof row: the proof's SQL expiry predicate can be
/// evaluated before PostgreSQL waits to acquire the user lock.
async fn expires_while_waiting(pool: &DbPool, user: i64, token: i64, hash: &str) {
    let DbPool::Postgres(pg) = pool else {
        return;
    };
    let proof = Proof::Token {
        home: Prefix::new(1),
        hash: hash.to_owned(),
    };
    authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .unwrap();
    let mut held = pg.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user)
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let mut connection = pg.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let query = authorization::Query {
        kind: "token",
        id: Some(token),
        hash: Some(hash),
        write: true,
    };
    let waiting = authorization::postgres(&mut connection, &query);
    let release = async {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let mut poll = tokio::time::interval(std::time::Duration::from_millis(10));
            loop {
                poll.tick().await;
                let blocked: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND NOT granted)",
                )
                .bind(pid)
                .fetch_one(pg)
                .await
                .unwrap();
                if blocked {
                    break;
                }
            }
        })
        .await
        .expect("credential query must reach the row lock before its proof expires");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        held.rollback().await.unwrap();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(waiting, release)
    })
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "a lock wait cannot revive an expired proof"
    );
    // A new operation asks the home again; it does not extend the old proof.
    authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .unwrap();
}

async fn snapshot_contract(database: &Database, user: i64) {
    use authorization::{freshness::Policy, heartbeat};
    let pool = database.pool();
    let value = codeyard_identity::sessions::fresh_id(pool).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        pool,
        &value,
        user,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let proof = Proof::Session {
        home: Prefix::ZERO,
        value: value.clone(),
    };
    let heartbeat = heartbeat::primary(pool, &proof).await.unwrap().unwrap();
    let read = Policy::default()
        .start(chrono::Utc::now(), std::time::Instant::now())
        .unwrap();
    assert!(read
        .prove(
            heartbeat,
            None,
            chrono::Utc::now(),
            std::time::Instant::now()
        )
        .is_some());
    database
        .execute(
            "UPDATE identity_heartbeat SET committed_at='2000-01-01T00:00:00.000000Z'",
            &[],
        )
        .await;
    let stale = heartbeat::snapshot(pool, &proof).await.unwrap().unwrap();
    assert!(read
        .prove(stale, None, chrono::Utc::now(), std::time::Instant::now())
        .is_none());
    // Primary fallback republishes the heartbeat and reads the actual credential again.
    let home = codeyard_identity::cells::Local { pool: pool.clone() };
    assert!(home.authorize(&proof).await.unwrap().is_some());
    codeyard_db_identity::users::sessions::delete_session(pool, &value)
        .await
        .unwrap();
    assert!(heartbeat::primary(pool, &proof).await.unwrap().is_none());
    assert!(home.authorize(&proof).await.unwrap().is_none());
}

/// An operation that outlives its proof asks the home again before its next database
/// call, and a refusal stays denied. Spawned follow-up work keeps the operation's proofs.
async fn renewal(pool: &DbPool, answer: Answer, user: i64) {
    let provider = Arc::new(Provider {
        answer: answer.clone(),
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        delay: AtomicU64::new(0),
    });
    pool.set_foreign_authorization(provider.clone());
    let proof = Proof::Token {
        home: Prefix::new(1),
        hash: "c".repeat(64),
    };
    let hash = proof.hash();
    let credential = codeyard_db_core::queries::users::accounts::AccountCredential::ForeignToken(
        answer.credential_id.unwrap(),
        &hash,
    );
    let actor: codeyard_db_core::queries::credentials::Actor = (user, &credential).into();
    authorization::scope(async {
        authorization::authorize(pool, &proof)
            .await
            .unwrap()
            .unwrap();
        let id = answer.credential_id;
        let visible = move |pool: DbPool, hash: String| async move {
            let query = authorization::Query {
                kind: "token",
                id,
                hash: Some(&hash),
                write: false,
            };
            matches!(authorization::lookup(&pool, &query).await, Ok(Some(_)))
        };
        assert!(
            tokio::spawn(authorization::inherit(visible(pool.clone(), hash.clone())))
                .await
                .unwrap(),
            "spawned follow-up work keeps the operation's proof"
        );
        assert!(
            !tokio::spawn(visible(pool.clone(), hash.clone()))
                .await
                .unwrap(),
            "a task outside the operation has no proof"
        );
        // In the second half of its life the proof is asked for again by one part of the
        // operation, while a concurrent part (another GraphQL field) keeps using it.
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        provider.delay.store(300, Ordering::SeqCst);
        let (renewed, concurrent) = tokio::join!(
            codeyard_db_repos::graphql::admit_query(pool, &actor),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                visible(pool.clone(), hash.clone()).await
            }
        );
        provider.delay.store(0, Ordering::SeqCst);
        assert!(renewed.is_ok());
        assert!(
            concurrent,
            "the proof stays usable while it is asked for again"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(authorization::ensure_current().is_err());
        assert!(
            codeyard_db_repos::graphql::admit_query(pool, &actor)
                .await
                .is_ok(),
            "an expired proof is asked for again before the next operation"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        authorization::ensure_current().unwrap();
        provider.live.store(false, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        for _ in 0..2 {
            assert!(
                codeyard_db_repos::graphql::admit_query(pool, &actor)
                    .await
                    .is_err(),
                "a revoked credential stops at the next operation"
            );
        }
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            4,
            "a refusal is not asked again"
        );
        assert!(authorization::ensure_current().is_err());
    })
    .await;
}
async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut database = Database::empty(pg).await;
    let pool = database.pool();
    let native = codeyard_db_identity::users::accounts::create_user(pool, "local", None)
        .await
        .unwrap();
    snapshot_contract(&database, native.id).await;
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
            heartbeat: chrono::DateTime::<chrono::Utc>::MAX_UTC,
            home: Prefix::new(1),
            user: remote.clone(),
            credential_id: Some(remote.id + 1),
            write: true,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            authority: Authority::Classic,
        },
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        delay: AtomicU64::new(0),
    });
    pool.set_foreign_authorization(provider.clone());
    let metadata = codeyard_identity::cells::resolve_user(pool, &remote.username)
        .await
        .unwrap()
        .unwrap();
    assert!(metadata.is_stub && metadata.email.is_none() && metadata.password_hash.is_none());
    assert_eq!(metadata.id, remote.id);
    let proof = Proof::Token {
        home: Prefix::new(1),
        hash: "a".repeat(64),
    };
    assert!(authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .is_some());
    assert!(
        authorization::scope(authorization::lookup(
            pool,
            &authorization::Query {
                kind: "token",
                id: provider.answer.credential_id,
                hash: Some(&proof.hash()),
                write: false,
            }
        ))
        .await
        .unwrap()
        .is_none(),
        "another operation cannot reuse this proof"
    );
    let stub = codeyard_db_identity::users::accounts::get_user_by_id(pool, remote.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stub.is_stub && stub.home_cell == 1);
    assert_eq!(count(pool, "personal_access_tokens").await, 0);
    assert_eq!(count(pool, "sessions").await, 0);
    assert_eq!(count(pool, "ssh_keys").await, 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let foreign_hash = "a".repeat(64);
    expires_while_waiting(
        pool,
        remote.id,
        provider.answer.credential_id.unwrap(),
        &foreign_hash,
    )
    .await;
    let credential = codeyard_db_core::queries::users::accounts::AccountCredential::ForeignToken(
        provider.answer.credential_id.unwrap(),
        &foreign_hash,
    );
    let actor: codeyard_db_core::queries::credentials::Actor = (remote.id, &credential).into();
    assert_eq!(
        codeyard_db_repos::graphql::admit_query(pool, &actor)
            .await
            .unwrap()
            .unwrap()
            .used,
        1
    );
    assert_eq!(
        count(pool, "personal_access_tokens").await,
        0,
        "a foreign quota never creates a local token"
    );
    let forged = codeyard_db_core::queries::credentials::Actor {
        user_id: native.id,
        credential: actor.credential.clone(),
    };
    assert!(codeyard_db_repos::graphql::admit_query(pool, &forged)
        .await
        .is_err());
    database
        .execute(
            "UPDATE graphql_rate_limits SET used=5000 WHERE token_id=$1",
            &[provider.answer.credential_id.unwrap()],
        )
        .await;
    assert!(codeyard_db_repos::graphql::admit_query(pool, &actor)
        .await
        .unwrap()
        .is_none());
    provider.live.store(false, Ordering::SeqCst);
    assert!(authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        if pg { 4 } else { 2 }
    );
    assert!(
        codeyard_db_repos::graphql::admit_query(pool, &actor)
            .await
            .is_err(),
        "an expired home proof cannot mutate its quota"
    );
    assert!(authorization::authorize(
        pool,
        &Proof::Token {
            home: Prefix::new(1),
            hash: "hostile".into()
        }
    )
    .await
    .is_err());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        if pg { 4 } else { 2 }
    );
    // An authoritative peer still cannot convert an existing native row into a foreign account.
    let mut forged = provider.answer.clone();
    forged.user.id = native.id;
    forged.user.username = native.username.clone();
    pool.set_foreign_authorization(Arc::new(Provider {
        answer: forged,
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        delay: AtomicU64::new(0),
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
    // Private metadata never crosses this identity boundary, even from an enrolled source.
    let mut private = provider.answer.clone();
    private.user.email = Some("private@example.test".into());
    pool.set_foreign_authorization(Arc::new(Provider {
        answer: private,
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        delay: AtomicU64::new(0),
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
    let mut private_metadata = provider.answer.clone();
    private_metadata.user.id += 30;
    private_metadata.user.username = "private-metadata".into();
    private_metadata.user.email = Some("private@example.test".into());
    pool.set_foreign_authorization(Arc::new(Provider {
        answer: private_metadata.clone(),
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        delay: AtomicU64::new(0),
    }));
    assert!(
        codeyard_identity::cells::resolve_user(pool, "private-metadata")
            .await
            .is_err()
    );
    assert!(
        codeyard_db_identity::users::accounts::get_user_by_id(pool, private_metadata.user.id)
            .await
            .unwrap()
            .is_none()
    );
    sessions(&database, provider.answer.clone(), native.id).await;
    // A reachable home with a stale heartbeat is unavailable, never a cache hit.
    let mut stale = provider.answer.clone();
    stale.heartbeat = chrono::Utc::now() - chrono::Duration::seconds(3);
    pool.set_foreign_authorization(Arc::new(Provider {
        answer: stale,
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        delay: AtomicU64::new(0),
    }));
    assert!(authorization::authorize(pool, &proof).await.is_err());
    {
        use tower::ServiceExt;
        let root = tempfile::tempdir().unwrap();
        let app = database.router(root.path()).await;
        let token = Prefix::new(1)
            .token(codeyard_types::tokens::PAT_PREFIX, &"a".repeat(64))
            .unwrap();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v3/user")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            503,
            "stale home heartbeat is unavailable over HTTP"
        );
    }
    assert!(authorization::lookup(
        pool,
        &authorization::Query {
            kind: "token",
            id: provider.answer.credential_id,
            hash: Some(&proof.hash()),
            write: false,
        }
    )
    .await
    .unwrap()
    .is_none());
    // The old table is gone, not retained as a fallback.
    let absent = match pool {
        DbPool::Postgres(pg) => sqlx::query("SELECT 1 FROM foreign_credentials")
            .execute(pg)
            .await
            .is_err(),
        DbPool::Sqlite(sqlite) => sqlx::query("SELECT 1 FROM foreign_credentials")
            .execute(sqlite)
            .await
            .is_err(),
    };
    assert!(absent);
    renewal(pool, provider.answer.clone(), remote.id).await;
    assert_eq!(count(pool, "personal_access_tokens").await, 0);
    assert_eq!(count(pool, "sessions").await, 0);
    database.finish().await;
}
#[tokio::test]
async fn sqlite_foreign_answers_expire_and_do_not_create_local_credentials() {
    authorization::scope(contract(false)).await
}
#[tokio::test]
async fn postgres_foreign_answers_expire_and_do_not_create_local_credentials() {
    authorization::scope(contract(true)).await
}
