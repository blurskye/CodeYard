//! Owner names on discovery pages and APIs, on disposable PostgreSQL and SQLite
//! databases. Model responses are not raw HTML or hydrated pages.
#![cfg(not(target_arch = "wasm32"))]

#[path = "org_discovery_owner/contracts.rs"]
mod contracts;
#[path = "support/database.rs"]
mod database;
#[path = "org_discovery_owner/fixture.rs"]
mod fixture;
#[path = "org_discovery_owner/malformed.rs"]
mod malformed;
#[path = "support/native_database.rs"]
mod native;
#[path = "support/production_ci.rs"]
mod production;

use database::Database;
use fixture::Fixture;

async fn contract(database: Database) {
    let mut fixture = Fixture::new(database).await;
    contracts::run(&fixture).await;
    malformed::run(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_organization_discovery_owner_names() {
    contract(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_organization_discovery_owner_names() {
    // Requires the existing guarded gitserv_test URL; setup failure is not a skip.
    contract(Database::new(true).await).await;
}

async fn bounded_queries(database: Database) {
    let mut fixture = Fixture::new(database).await;
    contracts::bounded_queries(&fixture).await;
    fixture.database.finish().await;
}

#[tokio::test]
async fn sqlite_bounded_discovery_queries() {
    bounded_queries(Database::new(false).await).await;
}

#[tokio::test]
async fn postgres_bounded_discovery_queries() {
    // Requires the existing guarded gitserv_test URL; setup failure is not a skip.
    bounded_queries(Database::new(true).await).await;
}
