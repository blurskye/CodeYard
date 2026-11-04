//! The fine-grained token pages render on the server with plain forms, so they work without
//! JavaScript, and they are only for the signed-in account and the organization's owners.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::{
    codeyard_db_orgs::organizations::applied, database::Database, http_client::Client,
};
use chrono::{Duration, Utc};

use codeyard_db_orgs::organizations::NewOrganization;
use codeyard_db_orgs::organizations::OrganizationProfile;

use codeyard::backend::identity::fine_grained::Access;
use codeyard::backend::identity::fine_grained::Owner;
use codeyard_db_core::models::OrgRole;

async fn contract(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let carol = f.actor("carol").await;
    let org = applied(
        codeyard_db_orgs::organizations::create_organization(
            f.pool(),
            alice,
            &NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    applied(
        codeyard_db_orgs::organizations::add_organization_member(
            f.pool(),
            alice,
            org,
            carol,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let owner = Client::login(&router, f.pool(), alice).await;
    let member = Client::login(&router, f.pool(), carol).await;

    let visitor = Client::anonymous(&router);
    for path in [
        "/settings/personal-access-tokens",
        "/settings/personal-access-tokens/new",
    ] {
        let reply = visitor.exchange("GET", path, "text/html", Vec::new()).await;
        assert_eq!(reply.status, 303, "{path}");
        assert!(reply.location.unwrap().starts_with("/login"), "{path}");
    }

    let created = codeyard::backend::identity::fine_grained::create(
        f.pool(),
        carol,
        Some(&member.session),
        codeyard::backend::identity::fine_grained::Request {
            name: "nightly export".into(),
            description: "Exports issues".into(),
            owner: Owner::Organization(org),
            access: Access::All,
            repositories: vec![],
            permissions: serde_json::json!({"issues":"read"})
                .as_object()
                .unwrap()
                .clone(),
            expires_at: Some(Utc::now() + Duration::days(30)),
        },
    )
    .await
    .unwrap();
    assert_eq!(created.status, "pending");

    // The member's list says the token waits, and never shows its value again.
    let html = member.page_ok("/settings/personal-access-tokens").await;
    assert!(html.contains("nightly export") && html.contains("Waiting for organization approval"));
    assert!(!html.contains(&created.value));
    assert!(html.contains("href=\"/settings/personal-access-tokens/new\""));

    // Owners review it with ordinary forms; members do not get the tab.
    let tab = format!("/settings/organizations/{org}?tab=personal-access-tokens");
    let html = owner.page_ok(&tab).await;
    let action = format!("action=\"/settings/organizations/{org}/personal-access-tokens\"");
    assert!(html.contains(&action));
    for text in [
        "Token policy",
        "Pending requests",
        "nightly export",
        "Exports issues",
        "name=\"decision\"",
        "value=\"approve\"",
        "value=\"deny\"",
        "name=\"max_lifetime_days\"",
    ] {
        assert!(html.contains(text), "{text}");
    }
    let html = member.page_ok(&tab).await;
    assert!(!html.contains(&action));
    assert!(!html.contains("nightly export"));
    f.finish().await;
}

#[tokio::test]
async fn sqlite() {
    contract(false).await;
}

#[tokio::test]
async fn postgres() {
    contract(true).await;
}
