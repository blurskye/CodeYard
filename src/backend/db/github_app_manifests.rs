//! Untrusted manifest drafts become one-time conversion grants only after browser consent.
use crate::backend::db::{
    github_apps::App,
    webhooks::{SealedSecret, WebhookFields},
    DbPool,
};
use anyhow::Result;
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Draft {
    pub hash: String,
    pub conversion_hash: Option<String>,
    pub manifest: String,
    pub state: String,
    pub user_id: Option<i64>,
    pub session_id: Option<String>,
    pub expires_at: i64,
}
native_operation!(crate::backend::db::postgres::github_app_manifests::save,crate::backend::db::sqlite::github_app_manifests::save,
    fn save(pool:&DbPool,hash:&str,manifest:&str,state:&str)->Result<()>);
native_operation!(crate::backend::db::postgres::github_app_manifests::draft,crate::backend::db::sqlite::github_app_manifests::draft,
    fn draft(pool:&DbPool,hash:&str,approved:bool)->Result<Option<Draft>>);
native_operation!(crate::backend::db::postgres::github_app_manifests::approve,crate::backend::db::sqlite::github_app_manifests::approve,
    fn approve(pool:&DbPool,hash:&str,actor:i64,session:&str,code:&str)->Result<bool>);
native_operation!(crate::backend::db::postgres::github_app_manifests::convert,crate::backend::db::sqlite::github_app_manifests::convert,
    fn convert(pool:&DbPool,hash:&str,app:&App,secret_hash:&str,callbacks:&[String],hook:Option<&WebhookFields>,secret:Option<&SealedSecret>)->Result<Option<App>>);
native_operation!(crate::backend::db::postgres::github_app_manifests::callback,crate::backend::db::sqlite::github_app_manifests::callback,
    fn callback(pool:&DbPool,client:&str,uri:&str)->Result<bool>);
