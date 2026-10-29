//! Workflow-level leases persist with run configuration after the queue is empty.
use super::{models::CiRun, DbPool};
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug)]
pub struct Configuration {
    pub group: String,
    pub cancel_in_progress: bool,
}
impl Configuration {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.group.is_empty()
                && self.group.len() <= 256
                && !self.group.chars().any(char::is_control),
            super::ci::CiAdmissionRefusal::Invalid("Invalid concurrency group")
        );
        Ok(())
    }
}
#[derive(sqlx::FromRow)]
pub struct Group {
    pub name: String,
    pub last_acquired_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
pub struct Member {
    #[sqlx(flatten)]
    pub run: CiRun,
    pub holding: bool,
    pub job_id: Option<i64>,
    pub job_name: Option<String>,
}

native_operation!(crate::backend::db::postgres::action_concurrency::groups,crate::backend::db::sqlite::action_concurrency::groups,
    fn groups(pool:&DbPool,repo:i64,run:Option<i64>)->Result<Vec<Group>>);
native_operation!(crate::backend::db::postgres::action_concurrency::members,crate::backend::db::sqlite::action_concurrency::members,
    fn members(pool:&DbPool,repo:i64,group:&str)->Result<Vec<Member>>);
