use codeyard_ci::config::CiPipelineKind;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct CiRunRequest {
    pub repo_id: i64,
    pub repo_owner: String,
    pub repo_name: String,
    pub branch: String,
    pub head_sha: Option<String>,
    pub event: String,
    pub pull_request_id: Option<i64>,
    pub pipeline: CiPipelineKind,
    pub workflow_paths: Vec<String>,
    pub environment: Option<String>,
    /// Submitted `workflow_dispatch` inputs. Admission checks them and replaces them with the
    /// complete set the run gets, defaults included.
    pub inputs: Option<BTreeMap<String, String>>,
    pub triggered_by_id: i64,
    /// Whether a person asked for the run (see `NewCiRun::manual`).
    pub manual: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("Pull request records are unavailable")]
    Database,
    #[error("Pull request repository storage is unavailable")]
    Storage,
    #[error("Base repository not found")]
    MissingBase,
    #[error("Pull request revisions kept changing during refresh")]
    Conflict,
}

pub trait ReceiveCi: Send + Sync {
    fn cluster(&self) -> Option<&crate::context::NodeContext>;
    fn config(&self) -> &codeyard_ci::config::CiConfig;
    fn trigger_event_run<'a>(
        &'a self,
        request: CiRunRequest,
        access: codeyard_db_core::models::AccessLevel,
        payload: serde_json::Value,
        credential: Option<codeyard_db_core::queries::ci::CiCredential>,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<codeyard_db_core::models::CiRun, codeyard_db_core::ci_admission::AdmissionError>,
    >;
    fn refresh_open_pull_requests<'a>(
        &'a self,
        pool: &'a codeyard_db_core::DbPool,
        repo: &'a codeyard_db_core::models::Repository,
        branches: Option<&'a [&'a str]>,
        actor_id: i64,
        ci_access: codeyard_db_core::models::AccessLevel,
    ) -> futures_util::future::BoxFuture<'a, Result<usize, RefreshError>>;
}
