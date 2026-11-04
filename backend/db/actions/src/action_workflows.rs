use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::models::CiRun;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

#[derive(Clone, sqlx::FromRow)]
pub struct Workflow {
    pub id: i64,
    pub repo_id: i64,
    pub path: String,
    pub name: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

native_operation!(crate::postgres::action_workflows::register_workflows,
    crate::sqlite::action_workflows::register_workflows,
    fn register_workflows(pool:&DbPool,repo:i64,definitions:&[(String,String)],refresh:bool) -> Result<Vec<Workflow>>);
native_operation!(crate::postgres::action_workflows::set_workflow_enabled,
    crate::sqlite::action_workflows::set_workflow_enabled,
    fn set_workflow_enabled(pool:&DbPool,actor:impl Into<codeyard_db_core::queries::credentials::Actor>,repo:i64,id:i64,enabled:bool) -> Result<Option<Workflow>>);
native_operation!(crate::postgres::action_workflows::workflow_by_id,
    crate::sqlite::action_workflows::workflow_by_id,
    fn workflow_by_id(pool: &DbPool, repo: i64, id:i64) -> Result<Option<Workflow>>);
native_operation!(crate::postgres::action_workflows::workflow_for_path,
    crate::sqlite::action_workflows::workflow_for_path,
    fn workflow_for_path(pool:&DbPool,repo:i64,path:&str) -> Result<Option<Workflow>>);
native_operation!(crate::postgres::action_workflows::latest_workflow_run,
    crate::sqlite::action_workflows::latest_workflow_run,
    fn latest_workflow_run(pool:&DbPool,repo:i64,path:&str,branch:Option<&str>,event:Option<&str>) -> Result<Option<CiRun>>);
