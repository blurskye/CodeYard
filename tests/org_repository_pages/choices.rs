use super::fixture::{applied, Fixture};
use axum::http::StatusCode;
use codeyard::backend::db::{
    models::OrgRole,
    organizations::{self as orgs, OrganizationOutcome, OrganizationPage},
};

pub async fn contracts(f: &Fixture) {
    let pool = f.database.pool();
    for actor in [&f.owner, &f.admin] {
        let reply = f.get(actor, "/new", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        reply.secure();
        reply.no_internal_fields();
        let (_, choices, form, error) = reply.creation();
        assert_eq!(choices.organizations.len(), 1);
        assert_eq!(choices.organizations[0].id, f.org);
        assert_eq!(choices.organizations[0].slug, "canonical_org");
        assert!(form.owner_org_id.is_empty());
        assert!(!form.is_private);
        assert!(error.is_none());
    }
    for actor in [&f.member, &f.outsider, &f.staff, &f.writer, &f.unknown] {
        let reply = f.get(actor, "/new", true).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.creation().1.organizations.is_empty());
        assert!(!reply.body.contains("canonical_org"));
        reply.no_internal_fields();
    }
    // Lower-ID ineligible memberships must not consume the requested LIMIT.
    let member_org = Fixture::organization(pool, f.outsider.id, "member_only_org").await;
    let suspended = Fixture::organization(pool, f.outsider.id, "suspended_org").await;
    let malformed = Fixture::organization(pool, f.owner.id, "malformed_role_org").await;
    let allowed = Fixture::organization(pool, f.owner.id, "later_allowed_org").await;
    for (owner, org, role) in [
        (f.outsider.id, member_org, OrgRole::Member),
        (f.outsider.id, suspended, OrgRole::Admin),
        (f.owner.id, malformed, OrgRole::Member),
        (f.owner.id, allowed, OrgRole::Admin),
    ] {
        applied(
            orgs::add_organization_member(pool, owner, org, f.admin.id, role)
                .await
                .unwrap(),
        );
    }
    f.database
        .execute(
            "UPDATE org_memberships SET role='future_role' WHERE org_id=$1 AND user_id=$2",
            &[malformed, f.admin.id],
        )
        .await;
    f.database
        .execute(
            "UPDATE users SET must_change_password=TRUE WHERE id=$1",
            &[f.outsider.id],
        )
        .await;
    let rows = applied(
        orgs::list_repository_creation_organizations(
            pool,
            f.admin.id,
            OrganizationPage::new(f.org, 1).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, allowed);
    assert!(!rows[0].governance_suspended);
    let path = format!(
        "/new?org_after={}&org_limit=1&owner_org_id={}",
        f.org, f.org
    );
    let reply = f.get(&f.admin, &path, true).await;
    assert_eq!(reply.status, StatusCode::OK);
    let (_, choices, draft, _) = reply.creation();
    assert_eq!(choices.organizations.len(), 1);
    assert_eq!(choices.organizations[0].id, allowed);
    assert_eq!(choices.next_after, Some(allowed));
    assert_eq!(draft.owner_org_id, f.org.to_string());
    assert!(
        !reply.body.contains("canonical_org"),
        "paged-away selection must not leak metadata or certify a choice"
    );
    for slug in ["member_only_org", "suspended_org", "malformed_role_org"] {
        assert!(!reply.body.contains(slug));
    }
    let end = f
        .get(
            &f.admin,
            &format!("/new?org_after={allowed}&org_limit=1"),
            true,
        )
        .await;
    assert!(end.creation().1.organizations.is_empty());
    assert_eq!(end.creation().1.next_after, None);
    for path in [
        "/new?org_after=-1",
        "/new?org_limit=0",
        "/new?org_limit=101",
        "/new?org_after=no",
        "/new?owner_org_id=-1",
        "/new?owner_org_id=0",
    ] {
        assert_eq!(
            f.get(&f.admin, path, true).await.status,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    // Absent and forbidden selections are identical except for the submitted ID.
    for target in [member_org, i64::MAX] {
        let reply = f
            .get(
                &f.admin,
                &format!("/new?org_after={allowed}&owner_org_id={target}"),
                true,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.creation().1.organizations.is_empty());
        assert_eq!(reply.creation().2.owner_org_id, target.to_string());
        assert!(!reply.body.contains("member_only_org"));
    }
    for column in ["is_banned", "must_change_password"] {
        f.database
            .execute(
                &format!("UPDATE users SET {column}=TRUE WHERE id=$1"),
                &[f.admin.id],
            )
            .await;
        assert!(matches!(
            orgs::list_repository_creation_organizations(
                pool,
                f.admin.id,
                OrganizationPage::new(0, 100).unwrap()
            )
            .await
            .unwrap(),
            OrganizationOutcome::Denied
        ));
        f.database
            .execute(
                &format!("UPDATE users SET {column}=FALSE WHERE id=$1"),
                &[f.admin.id],
            )
            .await;
    }
    f.database
        .execute(
            "UPDATE users SET must_change_password=FALSE WHERE id=$1",
            &[f.outsider.id],
        )
        .await;
}
