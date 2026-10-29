//! Token and browser inboxes use the same native rows and transaction-scoped authority.
use super::{db, ids, Fixture};
use codeyard::backend::identity::tokens::{self, TokenInput};
use db::notifications::records::{
    NotificationCredential as Credential, NotificationFilter, NotificationInboxAction as Action,
};

fn list() -> Action {
    Action::List {
        filter: NotificationFilter::inbox(false),
        limit: 100,
        before: None,
    }
}
async fn contract(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("token_owner").await;
    let user = f.actor("token_recipient").await;
    let foreign = f.actor("token_foreign").await;
    let session = super::session_inbox::session(f.pool(), user).await;
    let reader = tokens::create(
        f.pool(),
        user,
        Some(&session),
        TokenInput {
            name: "reader",
            scope: "read",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let writer = tokens::create(
        f.pool(),
        user,
        Some(&session),
        TokenInput {
            name: "writer",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let account = f.notify(user, None, "account").await;
    let repo = f.repo(owner, None, "token_private").await;
    let hidden = f.notify(user, Some(repo), "HIDDEN TOKEN PAYLOAD").await;
    let visible = db::notifications::records::notification_inbox_with_credential(
        f.pool(),
        Credential::Token(reader.token.id),
        user,
        list(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ids(&visible.notifications), vec![account]);
    let before = f.history(user).await;
    assert!(
        db::notifications::records::notification_inbox_with_credential(
            f.pool(),
            Credential::Token(reader.token.id),
            user,
            Action::Read {
                notification_id: account
            }
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(before, f.history(user).await);
    assert!(
        db::notifications::records::notification_inbox_with_credential(
            f.pool(),
            Credential::Token(writer.token.id),
            foreign,
            list()
        )
        .await
        .unwrap()
        .is_none()
    );
    let inaccessible = db::notifications::records::notification_inbox_with_credential(
        f.pool(),
        Credential::Token(writer.token.id),
        user,
        Action::Read {
            notification_id: hidden,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(inaccessible.notifications.is_empty());
    assert_eq!(inaccessible.updated, 0);
    let read = db::notifications::records::notification_inbox_with_credential(
        f.pool(),
        Credential::Token(writer.token.id),
        user,
        Action::Read {
            notification_id: account,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(read.updated, 1);
    let browser = db::notifications::records::notification_inbox(f.pool(), &session, user, list())
        .await
        .unwrap()
        .unwrap();
    assert!(browser.notifications[0].read_at.is_some());
    tokens::delete(f.pool(), user, Some(&session), writer.token.id)
        .await
        .unwrap();
    let before = f.history(user).await;
    assert!(
        db::notifications::records::notification_inbox_with_credential(
            f.pool(),
            Credential::Token(writer.token.id),
            user,
            Action::ReadAll
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(before, f.history(user).await);
    f.corrupt(
        "UPDATE personal_access_tokens SET expires_at='2000-01-01T00:00:00.000000Z' WHERE id=$1",
        &[reader.token.id],
    )
    .await;
    assert!(
        db::notifications::records::notification_inbox_with_credential(
            f.pool(),
            Credential::Token(reader.token.id),
            user,
            list()
        )
        .await
        .unwrap()
        .is_none()
    );
    f.finish().await;
}
#[tokio::test]
async fn sqlite_token_inbox() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_token_inbox() {
    contract(true).await;
}
