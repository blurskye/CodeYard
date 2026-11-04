//! Actions REST adapters use the same runs and object storage as both CI engines.
use axum::routing::MethodRouter;
use axum::Router;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use std::collections::HashMap;
use std::sync::Arc;
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

fn id(params: &HashMap<String, String>, key: &str) -> Result<i64> {
    params
        .get(key)
        .and_then(|id| id.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)
}

async fn run(state: &AppState, repo: i64, id: i64) -> Result<codeyard_db_core::models::CiRun> {
    codeyard_db_actions::action_runs::get(&state.pool, repo, id, None)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}

pub(super) fn aliases() -> Router<Arc<AppState>> {
    crate::actions::workflows::aliases()
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = crate::actions::objects::handlers();
    handlers.extend(crate::actions::approval::handlers());
    handlers.extend(crate::actions::cache_policy::handlers());
    handlers.extend(crate::actions::concurrency::handlers());
    handlers.extend(crate::actions::org_usage::handlers());
    handlers.extend(crate::actions::org_policy::handlers());
    handlers.extend(crate::actions::settings::handlers());
    handlers.extend(crate::actions::reviews::handlers());
    handlers.extend(crate::actions::oidc::handlers());
    handlers.extend(crate::actions::workflows::handlers());
    handlers.extend(crate::actions::run_mutations::handlers());
    handlers.extend(crate::actions::reruns::handlers());
    handlers.extend(crate::actions::runs::handlers());
    handlers.extend(crate::actions::usage::handlers());
    handlers.extend(crate::actions::jobs::handlers());
    handlers.extend(crate::actions::logs::handlers());
    handlers
}

pub mod environment_values;

pub mod organization_values;

pub mod action_permissions;

pub mod action_variables;

pub mod action_secrets;

pub mod runners;
