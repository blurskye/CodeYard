//! Actual application router, native forms and empty Git initialization on
//! disposable PostgreSQL/SQLite fixtures. Setup failures fail the test; nothing
//! is skipped or mocked. These contracts do not establish browser hydration.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "org_repository_pages/choices.rs"]
mod choices;
#[path = "support/database.rs"]
mod database;
#[path = "support/fake_webui_bundle.rs"]
mod fake_webui_bundle;
#[path = "org_repository_pages/fixture.rs"]
mod fixture;
#[path = "org_repository_pages/forms.rs"]
mod forms;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;

async fn choices_contract(pg: bool) {
    fake_webui_bundle::install();
    let mut fixture = fixture::Fixture::new(pg).await;
    choices::contracts(&fixture).await;
    fixture.database.finish().await;
}
async fn forms_contract(pg: bool) {
    fake_webui_bundle::install();
    let mut fixture = fixture::Fixture::new(pg).await;
    forms::contracts(&fixture).await;
    forms::revocation(&fixture).await;
    fixture.database.finish().await;
}
async fn raw_contract(pg: bool) {
    fake_webui_bundle::install();
    let mut fixture = fixture::Fixture::new(pg).await;
    forms::raw(&fixture).await;
    fixture.database.finish().await;
}
async fn repository_pages_contract(pg: bool) {
    fake_webui_bundle::install();
    let mut fixture = fixture::Fixture::new(pg).await;
    forms::repository_pages(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_native_repository_forks_collaborators_and_git_outage() {
    repository_pages_contract(false).await;
}
#[tokio::test]
async fn postgres_native_repository_forks_collaborators_and_git_outage() {
    repository_pages_contract(true).await;
}

async fn stale_settings_contract(pg: bool) {
    fake_webui_bundle::install();
    let mut fixture = fixture::Fixture::new(pg).await;
    forms::stale_settings_forms(&fixture).await;
    fixture.database.finish().await;
}
#[tokio::test]
async fn sqlite_stale_repository_settings_forms_are_refused() {
    stale_settings_contract(false).await;
}
#[tokio::test]
async fn postgres_stale_repository_settings_forms_are_refused() {
    stale_settings_contract(true).await;
}

async fn transfer_contract(pg: bool) {
    fake_webui_bundle::install();
    let mut fixture = fixture::Fixture::new(pg).await;
    forms::transfer_form(&fixture).await;
    fixture.database.finish().await;
}
#[tokio::test]
async fn sqlite_repository_transfer_form() {
    transfer_contract(false).await;
}
#[tokio::test]
async fn postgres_repository_transfer_form() {
    transfer_contract(true).await;
}

#[tokio::test]
async fn sqlite_bounded_native_owner_choices() {
    choices_contract(false).await;
}
#[tokio::test]
async fn postgres_bounded_native_owner_choices() {
    choices_contract(true).await;
}
#[tokio::test]
async fn sqlite_native_creation_forms_and_revocation() {
    forms_contract(false).await;
}
#[tokio::test]
async fn postgres_native_creation_forms_and_revocation() {
    forms_contract(true).await;
}
#[tokio::test]
async fn sqlite_raw_creation_html_and_matching_models() {
    raw_contract(false).await;
}
#[tokio::test]
async fn postgres_raw_creation_html_and_matching_models() {
    raw_contract(true).await;
}
