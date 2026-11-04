use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

native_operation!(codeyard_db_core::queries::postgres::oidc_settings::get,
    codeyard_db_core::queries::sqlite::oidc_settings::get,
    fn settings(pool: &DbPool, actor: i64, scope: Scope) -> Result<Settings>);
native_operation!(codeyard_db_core::queries::postgres::oidc_settings::save,
    codeyard_db_core::queries::sqlite::oidc_settings::save,
    fn save_settings(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, scope: Scope, settings: &Settings) -> Result<()>);
native_operation!(codeyard_db_core::queries::postgres::oidc_settings::inclusions,
    codeyard_db_core::queries::sqlite::oidc_settings::inclusions,
    fn inclusions(pool: &DbPool, actor: i64, org: i64) -> Result<Vec<Inclusion>>);
native_operation!(codeyard_db_core::queries::postgres::oidc_settings::change_inclusion,
    codeyard_db_core::queries::sqlite::oidc_settings::change_inclusion,
    fn change_inclusion(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, org: i64, name: &str, remove: bool) -> Result<()>);
native_operation!(codeyard_db_core::queries::postgres::oidc_settings::token_settings,
    codeyard_db_core::queries::sqlite::oidc_settings::token_settings,
    fn token_settings(pool: &DbPool, repo: i64) -> Result<TokenSettings>);

#[derive(sqlx::FromRow)]
pub struct Job {
    pub run_id: uuid::Uuid,
    /// The credential's job identity, `root workflow:job/called job`, or `act`.
    pub job: String,
    pub workflow: String,
    pub workflow_name: String,
    pub environment: Option<String>,
}

native_operation!(crate::postgres::oidc::key,
    crate::sqlite::oidc::key,
    fn key(pool: &DbPool) -> Result<Option<String>>);
native_operation!(crate::postgres::oidc::store_key,
    crate::sqlite::oidc::store_key,
    fn store_key(pool: &DbPool, pem: &str) -> Result<String>);
native_operation!(crate::postgres::oidc::bind_job,
    crate::sqlite::oidc::bind_job,
    fn bind_job(pool: &DbPool, hash: &str, workflow: &str, name: &str, environment: Option<&str>) -> Result<()>);
native_operation!(crate::postgres::oidc::job,
    crate::sqlite::oidc::job,
    fn job(pool: &DbPool, hash: &str) -> Result<Option<Job>>);

use codeyard_db_core::queries::oidc::Scope;

use codeyard_db_core::queries::oidc::Inclusion;

use codeyard_db_core::queries::oidc::TokenSettings;

use codeyard_db_core::queries::oidc::Settings;
