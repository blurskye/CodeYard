use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use codeyard_model::dependency_graph::Snapshot;
use codeyard_model::vulnerabilities::PreparedAdvisory;
use serde::Deserialize;
use serde::Serialize;

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
impl Settings {
    pub fn normalized(&self) -> std::result::Result<Self, Refusal> {
        let url = url::Url::parse(self.base_url.trim()).map_err(|_| Refusal::Invalid)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Refusal::Invalid);
        }
        let ecosystems: Vec<String> =
            serde_json::from_str(&self.ecosystems).map_err(|_| Refusal::Invalid)?;
        let mut unique = Vec::new();
        for ecosystem in ecosystems {
            if !codeyard_model::vulnerabilities::ECOSYSTEMS.contains(&ecosystem.as_str()) {
                return Err(Refusal::Invalid);
            }
            if !unique.contains(&ecosystem) {
                unique.push(ecosystem);
            }
        }
        if unique.len() > 10 {
            return Err(Refusal::Invalid);
        }
        Ok(Self {
            base_url: url.into(),
            ecosystems: serde_json::json!(unique).to_string(),
        })
    }
}
native_operation!(crate::postgres::vulnerabilities::settings,crate::sqlite::vulnerabilities::settings,
    fn settings(pool:&DbPool)->Result<Settings>);
native_operation!(crate::postgres::vulnerabilities::save_settings,crate::sqlite::vulnerabilities::save_settings,
    fn save_settings(pool:&DbPool,actor:i64,session:&str,settings:&Settings)->Result<()>);
native_operation!(crate::postgres::vulnerabilities::upsert,crate::sqlite::vulnerabilities::upsert,
    fn upsert(pool:&DbPool,advisory:&PreparedAdvisory)->Result<bool>);
native_operation!(crate::postgres::vulnerabilities::rescan_all,crate::sqlite::vulnerabilities::rescan_all,
    fn rescan_all(pool:&DbPool)->Result<()>);
native_operation!(crate::postgres::vulnerabilities::advisories,crate::sqlite::vulnerabilities::advisories,
    fn advisories(pool:&DbPool,ecosystem:&str,name:&str)->Result<Vec<String>>);
native_operation!(crate::postgres::vulnerabilities::enqueue,crate::sqlite::vulnerabilities::enqueue,
    fn enqueue(pool:&DbPool,repo:i64)->Result<()>);
native_operation!(crate::postgres::vulnerabilities::queue_changed,crate::sqlite::vulnerabilities::queue_changed,
    fn queue_changed(pool:&DbPool)->Result<()>);
native_operation!(crate::postgres::vulnerabilities::claim,crate::sqlite::vulnerabilities::claim,
    fn claim(pool:&DbPool)->Result<Option<(i64,i64)>>);
native_operation!(crate::postgres::vulnerabilities::claim_sync,crate::sqlite::vulnerabilities::claim_sync,
    fn claim_sync(pool:&DbPool)->Result<bool>);
native_operation!(crate::postgres::vulnerabilities::save_snapshot,crate::sqlite::vulnerabilities::save_snapshot,
    fn save_snapshot(pool:&DbPool,repo:i64,generation:i64,snapshot:&Snapshot)->Result<()>);
