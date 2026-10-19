//! Actual application-router and native-form contracts on disposable databases.
//! Model responses prove authorization and transport only; the raw HTML test covers
//! server-rendered pages. Neither establishes browser hydration.
#![cfg(not(target_arch = "wasm32"))]

use crate::support::database;
mod deletion;
use crate::support::fake_webui_bundle;
mod fixture;
mod members;
use crate::support::native_database as native;
mod pagination;
mod profile;
mod public;
mod routes;
mod team_page;

use database::Database;
use fixture::Fixture;

async fn contracts(database: Database) {
    let mut fixture = Fixture::new(database).await;
    routes::contracts(&fixture).await;
    routes::section_tabs(&fixture).await;
    profile::contracts(&fixture).await;
    pagination::contracts(&fixture).await;
    // Adds repositories, so it runs after the contracts that count them.
    public::contracts(&fixture).await;
    // Needs the members the pagination contract adds.
    public::tabs(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_actual_router_and_native_forms() {
    fake_webui_bundle::install();
    contracts(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_actual_router_and_native_forms() {
    fake_webui_bundle::install();
    // Required owned URL; unavailable infrastructure is a failure, never a skip.
    contracts(Database::new(true).await).await;
}

async fn membership_contracts(database: Database) {
    let mut fixture = Fixture::new(database).await;
    // The invitation contract runs first: the member contract ends with its owner leaving.
    members::invitations(&fixture).await;
    members::contracts(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_member_role_and_refusal_contracts() {
    fake_webui_bundle::install();
    membership_contracts(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_member_role_and_refusal_contracts() {
    fake_webui_bundle::install();
    membership_contracts(Database::new(true).await).await;
}

async fn access_management_contracts(database: Database) {
    let mut fixture = Fixture::new(database).await;
    members::access_management(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_named_access_rows_submit_native_actions() {
    fake_webui_bundle::install();
    access_management_contracts(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_named_access_rows_submit_native_actions() {
    fake_webui_bundle::install();
    access_management_contracts(Database::new(true).await).await;
}

async fn deletion_contracts(database: Database) {
    let mut fixture = Fixture::new(database).await;
    deletion::contracts(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_organization_delete_and_unknown_member_refusals() {
    fake_webui_bundle::install();
    deletion_contracts(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_organization_delete_and_unknown_member_refusals() {
    fake_webui_bundle::install();
    deletion_contracts(Database::new(true).await).await;
}

async fn banned_team_member_contracts(database: Database) {
    let mut fixture = Fixture::new(database).await;
    members::banned_team_member_removal(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_banned_team_member_leaves_with_organization_removal() {
    fake_webui_bundle::install();
    banned_team_member_contracts(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_banned_team_member_leaves_with_organization_removal() {
    fake_webui_bundle::install();
    banned_team_member_contracts(Database::new(true).await).await;
}

async fn team_page_contracts(database: Database) {
    let mut fixture = Fixture::new(database).await;
    team_page::contracts(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_public_team_page_contracts() {
    fake_webui_bundle::install();
    team_page_contracts(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_public_team_page_contracts() {
    fake_webui_bundle::install();
    team_page_contracts(Database::new(true).await).await;
}

async fn administration_contracts(database: Database) {
    let mut fixture = Fixture::new(database).await;
    routes::teams(&fixture).await;
    members::recovery(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_team_detail_and_audited_recovery_contracts() {
    fake_webui_bundle::install();
    administration_contracts(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_team_detail_and_audited_recovery_contracts() {
    fake_webui_bundle::install();
    administration_contracts(Database::new(true).await).await;
}

#[tokio::test]
async fn raw_repository_clone_toolbar_keeps_wrapped_actions_right_aligned() {
    fake_webui_bundle::install();
    let mut fixture = Fixture::new(Database::new(false).await).await;
    let response = fixture
        .request(
            &fixture.owner,
            "GET",
            "/orgs/repo/tree/feature/responsive-toolbar",
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(response.status, axum::http::StatusCode::OK);
    let html = &response.body;
    assert!(html.contains("feature/responsive-toolbar"));
    assert!(html.contains("repo-add-menu"));
    assert!(html.contains("repo-code-menu"));
    assert!(html.contains("HTTPS clone URL"));
    let mobile = html
        .split_once("@media (max-width: 639px)")
        .unwrap()
        .1
        .split("</style>")
        .next()
        .unwrap();
    assert!(mobile.contains(".repo-file-actions { margin-left: auto; }"));
    fixture.database.finish().await;
}

#[tokio::test]
async fn raw_organization_html_requires_matching_runtime_assets() {
    fake_webui_bundle::install();
    let mut fixture = Fixture::new(Database::new(false).await).await;
    let response = fixture
        .request(
            &fixture.owner,
            "GET",
            &fixture.path(),
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(
        response.status,
        axum::http::StatusCode::OK,
        "raw SSR requires matching assets; model tests cannot substitute: {}",
        response.body
    );
    assert_eq!(response.headers["x-codeyard-rendering"], "ssr-hydrated");
    response.secure();
    assert!(response.body.contains("Stored display"));
    assert!(response.body.contains("name=\"display_name\""));
    assert!(response.body.contains("<textarea"));
    assert!(response.body.contains("Stored description\nSecond line"));
    assert!(response
        .body
        .contains(&format!("action=\"{}\"", fixture.path())));
    let xss = "\n</textarea><script>unsafe</script>&\nDraft line";
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf_token", &fixture.owner.csrf)
        .append_pair("action", "profile")
        .append_pair("display_name", "   ")
        .append_pair("description", xss)
        .finish();
    let invalid = fixture
        .request(
            &fixture.owner,
            "POST",
            &fixture.path(),
            body,
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(invalid.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(invalid.headers["x-codeyard-rendering"], "ssr-hydrated");
    invalid.secure();
    assert!(invalid
        .body
        .contains("\n\n&lt;/textarea&gt;&lt;script&gt;unsafe&lt;/script&gt;&amp;\nDraft line"));
    assert!(!invalid.body.contains("<script>unsafe</script>"));
    let team = fixture
        .team(
            "raw_team",
            codeyard::backend::db::organizations::TeamPrivacy::Visible,
        )
        .await;
    let team_path = format!("{}?team={team}", fixture.path());
    let detail = fixture
        .request(
            &fixture.owner,
            "GET",
            &team_path,
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(detail.status, axum::http::StatusCode::OK);
    assert_eq!(detail.headers["x-codeyard-rendering"], "ssr-hydrated");
    detail.secure();
    for text in [
        "Team members",
        "Team repository grants",
        "Save team",
        "Delete team and its grants",
        "value=\"raw_team\"",
    ] {
        assert!(detail.body.contains(text));
    }
    assert!(!detail.body.contains("/admin/organizations/recovery"));
    let staff = fixture
        .request(
            &fixture.outsider,
            "GET",
            fixture::INDEX,
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(staff.status, axum::http::StatusCode::OK);
    // No organization lacks an owner here, so the recovery picker stays hidden.
    assert!(!staff.body.contains("/admin/organizations/recovery"));
    assert!(!staff.body.contains("Stored display"));
    fixture.database.finish().await;
}
