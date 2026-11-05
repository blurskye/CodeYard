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
