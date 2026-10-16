//! Original-scope contracts: a notification keeps the repository it was about,
//! even after that repository is deleted, so private history stays hidden.
use super::{agrees, db, Fixture};

async fn provenance(pg: bool) {
    let mut f = Fixture::new(pg).await;
    let owner = f.actor("scope_owner").await;
    let recipient = f.actor("scope_recipient").await;
    let repo = f.repo(owner, None, "scope_original").await;
    db::repositories::access::grant_repo_access(
        f.pool(),
        recipient,
        repo,
        db::models::repositories::AccessLevel::Read,
    )
    .await
    .unwrap();
    let account = f.notify(recipient, None, "new-account").await;
    let notice = f.notify(recipient, Some(repo), "original-private").await;
    // A row with no recorded scope is ambiguous history and stays hidden.
    f.scalar("INSERT INTO notifications(user_id,kind,title) VALUES($1,'account','AMBIGUOUS_PRIVATE_HISTORY') RETURNING id", &[recipient]).await;
    agrees(&f, recipient, &[notice, account]).await;
    assert_eq!(
        f.scalar(
            "SELECT scope_repository_id FROM notifications WHERE id=$1",
            &[account]
        )
        .await,
        0
    );
    assert_eq!(
        f.scalar(
            "SELECT scope_repository_id FROM notifications WHERE id=$1",
            &[notice]
        )
        .await,
        repo
    );
    let before = f.history(recipient).await;
    // The schema still refuses a negative scope; everything else about the
    // scope is set by the application when it creates the notification.
    f.rejected("INSERT INTO notifications(user_id,kind,title,scope_repository_id) VALUES($1,'issue','invalid',-1)", &[recipient]).await;
    assert_eq!(f.history(recipient).await, before);
    // Real FK SET NULL, with enforcement still enabled, cannot erase origin.
    f.execute("DELETE FROM repositories WHERE id=$1", &[repo])
        .await;
    let after_delete = f.history(recipient).await;
    let row = after_delete.iter().find(|r| r["id"] == notice).unwrap();
    assert!(row["repo"].is_null());
    assert_eq!(row["scope"], repo);
    agrees(&f, recipient, &[account]).await;
    assert_eq!(f.history(recipient).await, after_delete);
    f.reopen().await;
    agrees(&f, recipient, &[account]).await;
    assert_eq!(f.history(recipient).await, after_delete);
    f.close().await;
}

#[tokio::test]
async fn postgres_original_scope_constraints() {
    provenance(true).await;
}
#[tokio::test]
async fn sqlite_original_scope_constraints() {
    provenance(false).await;
}
