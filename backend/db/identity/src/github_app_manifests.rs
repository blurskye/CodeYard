use crate::github_apps::App;
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::webhooks::SealedSecret;
use codeyard_db_core::queries::webhooks::WebhookFields;
use codeyard_db_core::DbPool;

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
native_operation!(crate::postgres::github_app_manifests::save,crate::sqlite::github_app_manifests::save,
    fn save(pool:&DbPool,hash:&str,manifest:&str,state:&str)->Result<()>);
native_operation!(crate::postgres::github_app_manifests::draft,crate::sqlite::github_app_manifests::draft,
    fn draft(pool:&DbPool,hash:&str,approved:bool)->Result<Option<Draft>>);
native_operation!(crate::postgres::github_app_manifests::approve,crate::sqlite::github_app_manifests::approve,
    fn approve(pool:&DbPool,hash:&str,actor:i64,session:&str,code:&str)->Result<bool>);
native_operation!(crate::postgres::github_app_manifests::convert,crate::sqlite::github_app_manifests::convert,
    fn convert(pool:&DbPool,hash:&str,app:&App,secret_hash:&str,callbacks:&[String],hook:Option<&WebhookFields>,secret:Option<&SealedSecret>)->Result<Option<App>>);
native_operation!(crate::postgres::github_app_manifests::callback,crate::sqlite::github_app_manifests::callback,
    fn callback(pool:&DbPool,client:&str,uri:&str)->Result<bool>);
