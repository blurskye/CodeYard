//! Pointer delivery is ordered and atomic; a pointer never becomes native account authority.
use crate::support::database::Database;
use codeyard_db_core::cells::memberships::{self, Admission, Change, Event};
use codeyard_model::cells::{Object, ObjectKind};
use codeyard_support::cells::Prefix;
use futures_util::future::BoxFuture;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Gate {
    checks: AtomicUsize,
    fail: usize,
}
impl Admission for Gate {
    fn check<'a>(&'a self, _event: &'a Event) -> BoxFuture<'a, anyhow::Result<Prefix>> {
        Box::pin(async move {
            let check = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
            anyhow::ensure!(check != self.fail, "source key revoked before commit");
            Ok(Prefix::new(1))
        })
    }
}
struct PendingPage {
    session: String,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl codeyard_db_core::cells::authorization::Home for PendingPage {
    fn user<'a>(
        &'a self,
        _name: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<codeyard_db_core::models::User>>> {
        Box::pin(async { Ok(None) })
    }
    fn authorize<'a>(
        &'a self,
        _proof: &'a codeyard_db_core::cells::authorization::Proof,
    ) -> BoxFuture<'a, anyhow::Result<Option<codeyard_db_core::cells::authorization::Answer>>> {
        Box::pin(async { Ok(None) })
    }
    fn membership_page<'a>(
        &'a self,
        _cell: Prefix,
        _page: &'a memberships::Page,
        credential: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<memberships::PageResult>> {
        Box::pin(async move {
            assert_eq!(credential, self.session);
            self.started.notify_one();
            self.release.notified().await;
            Ok(memberships::PageResult {
                rows: vec![
                    serde_json::json!({"repository":{"id":*Prefix::new(1).ids().start(),
                    "owner":"remote","name":"never-disclose","description":null,"is_private":true,
                    "stars":0,"forks":0,"default_branch":"main","updated_at":"2026-11-01"},"role":"Write"}),
                ],
                total: Some(1),
            })
        })
    }
}

async fn contract(pg: bool) {
    let mut f = Database::empty(pg).await;
    f.complete_setup().await;
    let user = f.actor("membership-home").await;
    let first = *Prefix::new(1).ids().start();
    let event = Event {
        id: first + 1,
        change: Change {
            user_id: user,
            object: Object {
                kind: ObjectKind::Repository,
                id: first + 20,
            },
            active: true,
        },
    };
    let revoked = Gate {
        checks: AtomicUsize::new(0),
        fail: 3,
    };
    assert!(memberships::apply(f.pool(), &event, &revoked)
        .await
        .is_err());
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM remote_memberships WHERE user_id=$1",
            &[user]
        )
        .await,
        0
    );
    let gate = Gate {
        checks: AtomicUsize::new(0),
        fail: 0,
    };
    memberships::apply(f.pool(), &event, &gate).await.unwrap();
    memberships::apply(f.pool(), &event, &gate).await.unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM remote_memberships WHERE user_id=$1 AND active",
            &[user]
        )
        .await,
        1
    );
    // An active pointer whose source became this home must not fan out to itself.
    f.execute("INSERT INTO remote_memberships(user_id,resource_kind,resource_id,source_cell,source_event,active) VALUES($1,'repository',$2,0,$3,true)",
        &[user, first + 21, event.id]).await;
    let session = codeyard_identity::sessions::fresh_id(f.pool()).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        f.pool(),
        &session,
        user,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let actor = codeyard_db_core::queries::credentials::Actor::from((user, session.clone()));
    let read = async {
        match f.pool() {
            codeyard_db_core::DbPool::Postgres(pool) => {
                memberships::postgres::Reader::homes(pool, &actor, Default::default()).await
            }
            codeyard_db_core::DbPool::Sqlite(pool) => {
                memberships::sqlite::Reader::homes(pool, &actor, Default::default()).await
            }
        }
    };
    assert_eq!(read.await.unwrap(), vec![Prefix::new(1)]);
    f.execute(
        "DELETE FROM remote_memberships WHERE user_id=$1 AND source_cell=0",
        &[user],
    )
    .await;
    // Revoke the original browser session while its remote list is in flight.
    let request_limit =
        std::time::Duration::from_secs(memberships::Config::default().request_seconds * 2);
    let provider = std::sync::Arc::new(PendingPage {
        session: session.clone(),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    f.pool().set_foreign_authorization(provider.clone());
    let root = tempfile::tempdir().unwrap();
    let app = f.router(root.path()).await;
    let request = axum::http::Request::builder()
        .uri("/")
        .header("cookie", format!("session_id={session}"))
        .header("accept", "application/vnd.codeyard.page+json")
        .body(axum::body::Body::empty())
        .unwrap();
    let mut pending = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        use tower::ServiceExt;
        app.oneshot(request).await.unwrap()
    }));
    tokio::time::timeout(request_limit, provider.started.notified())
        .await
        .unwrap();
    codeyard_db_identity::users::sessions::delete_session(f.pool(), &session)
        .await
        .unwrap();
    provider.release.notify_one();
    let response = tokio::time::timeout(request_limit, &mut pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let body = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("never-disclose"));
    let mut removal = event.clone();
    removal.id += 1;
    removal.change.active = false;
    memberships::apply(f.pool(), &removal, &gate).await.unwrap();
    memberships::apply(f.pool(), &event, &gate).await.unwrap();
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM remote_memberships WHERE user_id=$1 AND active",
            &[user]
        )
        .await,
        0
    );
    let mut conflict = removal.clone();
    conflict.change.active = true;
    assert!(memberships::apply(f.pool(), &conflict, &gate)
        .await
        .is_err());
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM remote_memberships WHERE user_id=$1 AND active",
            &[user]
        )
        .await,
        0
    );
    let mut remote = codeyard_db_identity::users::accounts::get_user_by_id(f.pool(), user)
        .await
        .unwrap()
        .unwrap();
    remote.id = first + 30;
    remote.home_cell = 1;
    remote.email = None;
    remote.password_hash = None;
    remote.username = "membership-remote".into();
    codeyard_db_core::cells::authorization::redact_user(&mut remote);
    codeyard_db_core::cells::stubs::user(f.pool(), Prefix::new(1), &remote)
        .await
        .unwrap();
    let mut hostile = event.clone();
    hostile.change.user_id = remote.id;
    assert!(memberships::apply(f.pool(), &hostile, &gate).await.is_err());
    hostile.change.user_id = user;
    hostile.change.object.kind = ObjectKind::User;
    assert!(memberships::apply(f.pool(), &hostile, &gate).await.is_err());
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM remote_memberships WHERE user_id=$1",
            &[remote.id]
        )
        .await,
        0
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_membership_pointer_delivery_rolls_back_revocation_and_refuses_stubs() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_membership_pointer_delivery_rolls_back_revocation_and_refuses_stubs() {
    contract(true).await;
}
