use crate::runs::CiRuns;
use codeyard_repos_core::receive_ci::{CiRunRequest, ReceiveCi, RefreshError};
impl ReceiveCi for CiRuns {
    fn cluster(&self) -> Option<&codeyard_repos_core::context::NodeContext> { self.cluster.as_ref() }
    fn config(&self) -> &codeyard_ci::config::CiConfig { self.config() }
    fn trigger_event_run<'a>(&'a self, request: CiRunRequest, access: codeyard_db_core::models::AccessLevel, payload: serde_json::Value, credential: Option<codeyard_db_core::queries::ci::CiCredential>) -> futures_util::future::BoxFuture<'a, Result<codeyard_db_core::models::CiRun, codeyard_db_core::ci_admission::AdmissionError>> { Box::pin(self.trigger_event_run(request, access, payload, credential)) }
    fn refresh_open_pull_requests<'a>(&'a self, pool: &'a codeyard_db_core::DbPool, repo: &'a codeyard_db_core::models::Repository, branches: Option<&'a [&'a str]>, actor_id: i64, ci_access: codeyard_db_core::models::AccessLevel) -> futures_util::future::BoxFuture<'a, Result<usize, RefreshError>> { Box::pin(crate::pull_requests::branches::refresh_open_pull_requests(pool, repo, branches, self, actor_id, ci_access)) }
}
