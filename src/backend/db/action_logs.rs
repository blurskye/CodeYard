//! Short-lived read capabilities bind real durable logs and their observed file manifest.
use super::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;

pub(crate) const RETENTION_BATCH: i64 = 8;

#[derive(Clone, Copy)]
pub struct Target {
    pub run: i64,
    pub job: Option<i64>,
    pub step: Option<i32>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub jobs: Vec<Job>,
}
#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub name: String,
    #[sqlx(skip)]
    pub steps: Vec<Step>,
}
#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Step {
    pub number: i32,
    pub name: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}
#[derive(sqlx::FromRow)]
pub struct Grant {
    pub repo_id: i64,
    pub run_id: i64,
    pub job_id: Option<i64>,
    pub step_number: Option<i32>,
    pub log_version: i64,
    pub max_log_id: i64,
    pub manifest: Json<Manifest>,
}
#[derive(sqlx::FromRow)]
pub struct Line {
    pub id: i64,
    pub job_id: Option<i64>,
    pub step_number: Option<i32>,
    pub stage: String,
    pub line: String,
    pub created_at: DateTime<Utc>,
}
impl From<super::models::CiLogLine> for Line {
    fn from(line: super::models::CiLogLine) -> Self {
        Self {
            id: line.id,
            job_id: line.job_id,
            step_number: line.step_number,
            stage: line.stage,
            line: line.line,
            created_at: line.created_at,
        }
    }
}

pub struct Snapshot {
    pub grant: Grant,
    pub lines: Vec<Line>,
}

#[derive(sqlx::FromRow)]
pub(crate) struct ObservedStep {
    pub job_id: i64,
    #[sqlx(flatten)]
    pub step: Step,
}
impl Manifest {
    pub(crate) fn capture(
        mut jobs: Vec<Job>,
        steps: Vec<ObservedStep>,
        number: Option<i32>,
    ) -> Option<Self> {
        let indices: std::collections::HashMap<_, _> = jobs
            .iter()
            .enumerate()
            .map(|(index, job)| (job.id, index))
            .collect();
        for observed in steps {
            if let Some(index) = indices.get(&observed.job_id) {
                jobs[*index].steps.push(observed.step);
            }
        }
        if number.is_some_and(|number| {
            jobs.iter()
                .any(|job| !job.steps.iter().any(|step| step.number == number))
        }) {
            return None;
        }
        Some(Self { jobs })
    }
}

native_operation!(crate::backend::db::postgres::action_logs::issue,crate::backend::db::sqlite::action_logs::issue,
    fn issue(pool:&DbPool,actor:impl Into<super::credentials::Actor>,repo:i64,resource:&str,target:Target,hash:&str)->Result<bool>);
native_operation!(crate::backend::db::postgres::action_logs::read,crate::backend::db::sqlite::action_logs::read,
    fn read(pool:&DbPool,hash:&str,resource:&str)->Result<Option<Snapshot>>);
