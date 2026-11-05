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
    assert_eq!(
        codeyard_db_identity::users::sessions::get_session_user(pool, &value)
            .await
            .unwrap()
            .unwrap()
            .id,
        user
    );
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
    assert_eq!(response.status(), 201);
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
    expire(pool).await;
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

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
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
    let metadata = codeyard_identity::cells::resolve_user(pool, &remote.username)
        .await
        .unwrap()
        .unwrap();
    assert!(metadata.is_stub && metadata.email.is_none() && metadata.password_hash.is_none());
    assert_eq!(metadata.id, remote.id);
    assert_eq!(count(pool, "foreign_credentials").await, 0);
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
    let foreign_hash = "a".repeat(64);
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
        .is_some());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    expire(pool).await;
    assert!(authorization::authorize(pool, &proof)
        .await
        .unwrap()
        .is_none());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
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
    let mut private_metadata = provider.answer.clone();
    private_metadata.user.id += 30;
    private_metadata.user.username = "private-metadata".into();
    private_metadata.user.email = Some("private@example.test".into());
    pool.set_foreign_authorization(Arc::new(Provider {
        answer: private_metadata.clone(),
        live: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
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
