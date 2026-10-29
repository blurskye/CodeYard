//! Durable OSV data and leases for lockfile snapshot scans.
use super::DbPool;
use crate::backend::{dependency_graph::Snapshot, vulnerabilities::PreparedAdvisory};
use anyhow::Result;
use serde::{Deserialize, Serialize};
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Security administration required")]
    Denied,
    #[error("Invalid vulnerability settings")]
    Invalid,
}
#[derive(Clone, Debug, sqlx::FromRow, Serialize, Deserialize)]
pub struct Settings {
    pub base_url: String,
    pub ecosystems: String,
}
native_operation!(super::postgres::vulnerabilities::settings,super::sqlite::vulnerabilities::settings,
    fn settings(pool:&DbPool)->Result<Settings>);
native_operation!(super::postgres::vulnerabilities::save_settings,super::sqlite::vulnerabilities::save_settings,
    fn save_settings(pool:&DbPool,actor:i64,session:&str,settings:&Settings)->Result<()>);
native_operation!(super::postgres::vulnerabilities::upsert,super::sqlite::vulnerabilities::upsert,
    fn upsert(pool:&DbPool,advisory:&PreparedAdvisory)->Result<bool>);
native_operation!(super::postgres::vulnerabilities::rescan_all,super::sqlite::vulnerabilities::rescan_all,
    fn rescan_all(pool:&DbPool)->Result<()>);
native_operation!(super::postgres::vulnerabilities::advisories,super::sqlite::vulnerabilities::advisories,
    fn advisories(pool:&DbPool,ecosystem:&str,name:&str)->Result<Vec<String>>);
native_operation!(super::postgres::vulnerabilities::enqueue,super::sqlite::vulnerabilities::enqueue,
    fn enqueue(pool:&DbPool,repo:i64)->Result<()>);
native_operation!(super::postgres::vulnerabilities::queue_changed,super::sqlite::vulnerabilities::queue_changed,
    fn queue_changed(pool:&DbPool)->Result<()>);
native_operation!(super::postgres::vulnerabilities::claim,super::sqlite::vulnerabilities::claim,
    fn claim(pool:&DbPool)->Result<Option<(i64,i64)>>);
native_operation!(super::postgres::vulnerabilities::claim_sync,super::sqlite::vulnerabilities::claim_sync,
    fn claim_sync(pool:&DbPool)->Result<bool>);
native_operation!(super::postgres::vulnerabilities::save_snapshot,super::sqlite::vulnerabilities::save_snapshot,
    fn save_snapshot(pool:&DbPool,repo:i64,generation:i64,snapshot:&Snapshot)->Result<()>);
