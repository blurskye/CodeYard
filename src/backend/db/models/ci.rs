use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CiRun {
    #[serde(default)]
    #[sqlx(default)]
    pub workflow_run_number: Option<i64>,
    #[serde(default)]
    #[sqlx(default)]
    pub workflow_run_id: Option<i64>,
    #[serde(default)]
    #[sqlx(default)]
    pub workflow_run_attempt: Option<i32>,
    #[serde(default)]
    #[sqlx(default)]
    pub engine: Option<String>,
    pub id: i64,
    pub run_id: Uuid,
    pub repo_id: i64,
    pub triggered_by_id: Option<i64>,
    /// Unassigned until a worker claims the run.
    pub mode: String,
    pub pipeline: String,
    pub event: String,
    #[serde(default)]
    pub pull_request_id: Option<i64>,
    /// The attempt at a pull request revision: 1 for the first run, and one more for each run
    /// started again by hand. Other runs are attempt 1.
    #[serde(default)]
    pub attempt: i32,
    #[serde(default)]
    pub pull_request_event: Option<serde_json::Value>,
    #[serde(default)]
    #[sqlx(default)]
    pub event_payload: Option<serde_json::Value>,
    /// Inputs of a manual `workflow_dispatch` run; NULL for other events.
    #[serde(default)]
    pub workflow_inputs: Option<serde_json::Value>,
    #[serde(default)]
    pub is_fork: bool,
    #[serde(default)]
    #[sqlx(default)]
    pub approval_required: bool,
    pub branch: String,
    pub head_sha: Option<String>,
    pub workflow_path: String,
    pub workflow_sha: Option<String>,
    pub workflow_blobs: serde_json::Value,
    #[serde(skip, default = "empty_runner_labels")]
    pub runner_labels: String,
    #[serde(skip)]
    pub runner_group: Option<String>,
    /// Worker node that claimed the run.
    pub worker: Option<String>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub cancel_requested: bool,
    pub environment: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn empty_runner_labels() -> String {
    "[]".into()
}

/// A branch or tag name without its `refs/heads/` or `refs/tags/` prefix.
pub fn workflow_branch(reference: &str) -> &str {
    reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/tags/"))
        .unwrap_or(reference)
}

impl CiRun {
    /// The branch GitHub names as the run's head: a pull request's head branch, otherwise the
    /// pushed branch or tag.
    pub fn head_branch(&self) -> &str {
        self.pull_request_event
            .as_ref()
            .and_then(|event| event["head_ref"].as_str())
            .unwrap_or_else(|| workflow_branch(&self.branch))
    }

    /// The executing ref, not a pull request's stored base branch. A missing PR identity
    /// cannot be treated as an ordinary branch when evaluating deployment protections.
    pub(crate) fn workflow_reference(&self, pull_number: Option<i64>) -> Option<String> {
        if self.event == "pull_request" {
            let number = self.pull_request_event.as_ref().and_then(|event| event["number"].as_i64())
                .or(pull_number).filter(|number| *number > 0)?;
            Some(format!("refs/pull/{number}/merge"))
        } else {
            let branch = if self.event == "pull_request_target" {
                self.pull_request_event.as_ref().and_then(|event| event["base_ref"].as_str()).unwrap_or(&self.branch)
            } else { &self.branch };
            Some(if branch.starts_with("refs/") { branch.to_owned() } else { format!("refs/heads/{branch}") })
        }
    }

    pub fn commit_status_context(&self) -> String {
        if self.workflow_path.contains(',') {
            format!("gitserv/pipeline/{}", self.pipeline)
        } else {
            format!("gitserv/workflow/{}", self.workflow_path)
        }
    }

    /// A protection rule may require this shorter name. Every run that shares it must pass.
    pub fn alias_commit_status_context(&self) -> String {
        let name = if self.workflow_path.contains(',') {
            &self.pipeline
        } else {
            std::path::Path::new(&self.workflow_path)
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or(&self.pipeline)
        };
        format!("gitserv/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &str) -> CiRun {
        serde_json::from_value(serde_json::json!({
            "id": 1, "run_id": Uuid::nil(), "repo_id": 1,
            "mode": "local", "pipeline": "ci", "event": "manual", "branch": "main",
            "workflow_path": path, "workflow_blobs": {}, "cancel_requested": false,
            "status": "queued", "created_at": Utc::now(), "updated_at": Utc::now(),
        }))
        .unwrap()
    }

    #[test]
    fn ci_contexts_keep_paths_extensions_and_pipeline_namespaces_distinct() {
        let runs = [
            run(".github/workflows/test.yml"),
            run(".github/workflows/test.yaml"),
            run(".codeyard/workflows/test.yml"),
            run(".github/workflows/ci.yml"),
            run(".github/workflows/test.yml,.github/workflows/deploy.yml"),
        ];
        let contexts: std::collections::HashSet<_> =
            runs.iter().map(CiRun::commit_status_context).collect();
        assert_eq!(contexts.len(), runs.len());
        assert_eq!(
            runs[0].commit_status_context(),
            "gitserv/workflow/.github/workflows/test.yml"
        );
        assert_eq!(runs[4].commit_status_context(), "gitserv/pipeline/ci");
        assert_eq!(
            runs[0].alias_commit_status_context(),
            runs[1].alias_commit_status_context()
        );
        assert_eq!(
            runs[3].alias_commit_status_context(),
            runs[4].alias_commit_status_context()
        );
        let path = format!(".github/workflows/{}.yml", "a".repeat(240));
        assert_eq!(
            run(&path).commit_status_context(),
            format!("gitserv/workflow/{path}")
        );
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CiLogLine {
    pub id: i64,
    pub run_id: i64,
    pub stage: String,
    pub line: String,
    pub level: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CiDeployment {
    pub id: i64,
    pub run_id: i64,
    pub repo_id: i64,
    pub environment: String,
    pub status: String,
    pub approved_by_id: Option<i64>,
    pub approved_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
