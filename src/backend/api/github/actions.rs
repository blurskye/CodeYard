//! Actions REST adapters use the same runs and object storage as both CI engines.
mod approval;
mod cache_policy;
mod concurrency;
mod jobs;
mod logs;
mod objects;
mod oidc;
mod org_policy;
mod org_usage;
mod reruns;
mod reviews;
mod run_mutations;
mod runs;
mod settings;
mod usage;
mod workflows;
pub(super) use jobs::annotations as job_annotations;
pub(super) use workflows::admission_error;

use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use axum::{routing::MethodRouter, Router};
use std::{collections::HashMap, sync::Arc};

fn id(params: &HashMap<String, String>, key: &str) -> Result<i64> {
    params
        .get(key)
        .and_then(|id| id.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)
}

async fn run(state: &AppState, repo: i64, id: i64) -> Result<codeyard_db_core::models::CiRun> {
    crate::backend::db::action_runs::get(&state.pool, repo, id, None)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}

pub(super) fn aliases() -> Router<Arc<AppState>> {
    workflows::aliases()
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = objects::handlers();
    handlers.extend(approval::handlers());
    handlers.extend(cache_policy::handlers());
    handlers.extend(concurrency::handlers());
    handlers.extend(org_usage::handlers());
    handlers.extend(org_policy::handlers());
    handlers.extend(settings::handlers());
    handlers.extend(reviews::handlers());
    handlers.extend(oidc::handlers());
    handlers.extend(workflows::handlers());
    handlers.extend(run_mutations::handlers());
    handlers.extend(reruns::handlers());
    handlers.extend(runs::handlers());
    handlers.extend(usage::handlers());
    handlers.extend(jobs::handlers());
    handlers.extend(logs::handlers());
    handlers
}
