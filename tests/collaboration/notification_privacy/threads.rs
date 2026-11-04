use super::{db, Fixture};
use codeyard_db_repos::notifications::records::NotificationCredential;
use codeyard_db_repos::notifications::records::NotificationInboxAction;
use codeyard_db_repos::notifications::threads::ThreadAction;
use codeyard_db_repos::notifications::threads::ThreadFilter;
use codeyard_db_repos::notifications::threads::ThreadInbox;
use codeyard_identity::tokens;
use codeyard_identity::tokens::TokenInput;
use serde_json::json;

async fn act(f: &Fixture, user: i64, token: i64, action: ThreadAction) -> ThreadInbox {
    codeyard_db_repos::notifications::records::notification_inbox_with_credential(
        f.pool(),
        NotificationCredential::Token(token),
        user,
        NotificationInboxAction::Threads(action),
    )
    .await
    .unwrap()
    .unwrap()
    .threads
    .unwrap()
}
async fn notice(
    f: &Fixture,
    user: i64,
    repo: i64,
    kind: &str,
) -> anyhow::Result<codeyard_db_core::models::Notification> {
    codeyard_db_repos::notifications::records::create_notification(
        f.pool(),
        codeyard_db_core::queries::notifications::records::NewNotification {
            user_id: user,
            actor_id: None,
            repo_id: Some(repo),
            kind,
            title: "Real native notice",
            body: None,
            link: None,
            metadata: &json!({}),
        },
    )
    .await
}
async fn contract(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let user = f.actor("thread_recipient").await;
    let foreign = f.actor("thread_foreign").await;
    let session = super::session_inbox::session(f.pool(), user).await;
    let token = tokens::create(
        f.pool(),
        user,
        Some(&session),
        TokenInput {
            name: "threads",
            scope: "write",
            expires_at: None,
        },
    )
    .await
    .unwrap()
    .token
    .id;
    let repo = f.repo(user, None, "threads").await;
    let private = f.repo(foreign, None, "hidden_threads").await;
    let account = f.notify(user, None, "Account only").await;
    let first = f.notify(user, Some(repo), "First repository notice").await;
    let second = f.notify(user, Some(repo), "Second repository notice").await;
    let hidden = f.notify(user, Some(private), "Hidden newer notice").await;
    let list = act(
        &f,
        user,
        token,
        ThreadAction::List(ThreadFilter {
            limit: 1,
            ..Default::default()
        }),
    )
    .await;
    assert!(list.found);
    assert_eq!(list.threads.len(), 1, "scope filtering precedes the limit");
    let thread = list.threads[0].thread.id;
    assert_eq!(list.threads[0].notification.id, second);
    assert!(list.threads[0].unread);
    let read = act(&f, user, token, ThreadAction::Read(thread)).await;
    assert_eq!(read.updated, 2);
    let rows = codeyard_db_repos::notifications::records::list_notifications_for_user(
        f.pool(),
        user,
        100,
        false,
    )
    .await
    .unwrap();
    assert!(rows
        .iter()
        .find(|n| n.id == first)
        .unwrap()
        .read_at
        .is_some());
    assert!(rows
        .iter()
        .find(|n| n.id == second)
        .unwrap()
        .read_at
        .is_some());
    assert!(rows
        .iter()
        .find(|n| n.id == account)
        .unwrap()
        .read_at
        .is_none());
    let history = f.history(user).await;
    assert!(history.iter().find(|n| n["id"] == hidden).unwrap()["read_at"].is_null());
    let read = act(&f, user, token, ThreadAction::Get(thread)).await;
    assert!(!read.threads[0].unread);
    assert!(read.threads[0].last_read_at.is_some());
    let third = f.notify(user, Some(repo), "Third notice").await;
    let list = act(
        &f,
        user,
        token,
        ThreadAction::List(ThreadFilter {
            limit: 100,
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(list.threads[0].thread.id, thread);
    assert_eq!(list.threads[0].notification.id, third);
    let initial = act(&f, user, token, ThreadAction::GetSubscription(thread))
        .await
        .subscription
        .unwrap();
    assert!(initial.subscribed);
    let unsubscribed = act(&f, user, token, ThreadAction::DeleteSubscription(thread))
        .await
        .subscription
        .unwrap();
    assert!(!unsubscribed.subscribed);
    assert_eq!(initial.created_at, unsubscribed.created_at);
    assert!(notice(&f, user, repo, "test_notice")
        .await
        .unwrap_err()
        .is::<codeyard_db_core::queries::notifications::records::NotificationSuppressed>());
    assert!(codeyard_db_repos::notifications::threads::subscribers(
        f.pool(),
        repo,
        "Repository",
        &repo.to_string()
    )
    .await
    .unwrap()
    .is_empty());
    // Direct participation can resume delivery, but an explicit ignore always wins.
    notice(&f, user, repo, "mention").await.unwrap();
    act(
        &f,
        user,
        token,
        ThreadAction::SetSubscription {
            thread_id: thread,
            subscribed: true,
            ignored: true,
        },
    )
    .await;
    assert!(notice(&f, user, repo, "mention").await.is_err());
    act(
        &f,
        user,
        token,
        ThreadAction::SetSubscription {
            thread_id: thread,
            subscribed: true,
            ignored: false,
        },
    )
    .await;
    assert_eq!(
        codeyard_db_repos::notifications::threads::subscribers(
            f.pool(),
            repo,
            "Repository",
            &repo.to_string()
        )
        .await
        .unwrap(),
        vec![user]
    );
    act(&f, user, token, ThreadAction::Done(thread)).await;
    let done = act(&f, user, token, ThreadAction::Get(thread)).await;
    assert!(!done.threads[0].unread);
    assert!(act(
        &f,
        user,
        token,
        ThreadAction::List(ThreadFilter {
            all: true,
            limit: 100,
            ..Default::default()
        })
    )
    .await
    .threads
    .is_empty());
    let native = codeyard_db_repos::notifications::records::list_notifications_for_user(
        f.pool(),
        user,
        100,
        false,
    )
    .await
    .unwrap();
    assert!(native
        .iter()
        .filter(|n| n.repo_id == Some(repo))
        .all(|n| n.done_at.is_some()));
    // Removing a source notice does not make its real subscription impossible to manage.
    f.corrupt(
        "DELETE FROM notifications WHERE user_id=$1 AND repo_id=$2",
        &[user, repo],
    )
    .await;
    assert!(
        act(&f, user, token, ThreadAction::DeleteSubscription(thread))
            .await
            .found
    );
    assert!(
        !act(&f, user, token, ThreadAction::GetSubscription(thread))
            .await
            .subscription
            .unwrap()
            .subscribed
    );
    let before = f.history(user).await;
    f.corrupt(
        "UPDATE personal_access_tokens SET expires_at='2000-01-01T00:00:00.000000Z' WHERE id=$1",
        &[token],
    )
    .await;
    assert!(
        codeyard_db_repos::notifications::records::notification_inbox_with_credential(
            f.pool(),
            NotificationCredential::Token(token),
            user,
            NotificationInboxAction::Threads(ThreadAction::ReadAll {
                repository_id: None,
                last_read_at: None,
                read: true
            })
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(before, f.history(user).await);
    f.finish().await;
}
#[tokio::test]
async fn sqlite_threads() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_threads() {
    contract(true).await;
}
