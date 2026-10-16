//! Organization deletion refusals, and the unknown-username message for owners and admins.
use crate::fixture::{Fixture, INDEX};
use axum::http::StatusCode;

pub async fn contracts(f: &Fixture) {
    let path = f.path();
    // Only owners and admins learn that a username is unknown; a plain member is refused.
    let add = [
        ("action", "add-member"),
        ("username", "no_such_account"),
        ("role", "member"),
    ];
    assert_eq!(
        f.form(&f.member, &path, &add).await.status,
        StatusCode::FORBIDDEN
    );
    let unknown = f.form(&f.owner, &path, &add).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(
        unknown.organization().message.as_deref(),
        Some("No user named no_such_account.")
    );

    // Only an owner deletes, and only with the organization's name typed.
    let delete = [("action", "delete-organization"), ("confirm", "native_org")];
    assert_eq!(
        f.form(&f.member, &path, &delete).await.status,
        StatusCode::FORBIDDEN
    );
    let mismatch = f
        .form(
            &f.owner,
            &path,
            &[("action", "delete-organization"), ("confirm", "native")],
        )
        .await;
    assert_eq!(mismatch.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        mismatch.organization().message.as_deref(),
        Some("Type the organization name to confirm deletion.")
    );
    assert_eq!(
        f.database
            .count("SELECT COUNT(*) FROM organizations WHERE id=$1", f.org)
            .await,
        1
    );

    // Deleting the organization also deletes the repositories it owns.
    let repository = f
        .database
        .organization_repository(f.org, "kept", false)
        .await;
    f.form(&f.owner, &path, &delete)
        .await
        .redirect(&format!("{INDEX}?notice=organization-deleted"));
    let notice = f
        .get(&f.owner, &format!("{INDEX}?notice=organization-deleted"))
        .await
        .organization();
    assert_eq!(notice.notice.as_deref(), Some("Organization deleted."));
    assert_eq!(f.get(&f.owner, &path).await.status, StatusCode::NOT_FOUND);
    for sql in [
        "SELECT COUNT(*) FROM organizations WHERE id=$1",
        "SELECT COUNT(*) FROM teams WHERE org_id=$1",
        "SELECT COUNT(*) FROM org_memberships WHERE org_id=$1",
    ] {
        assert_eq!(f.database.count(sql, f.org).await, 0, "{sql}");
    }
    assert_eq!(
        f.database
            .count("SELECT COUNT(*) FROM repositories WHERE id=$1", repository)
            .await,
        0
    );
}
