use serde::{Deserialize, Serialize};

pub const MAX_CI_WORKFLOW_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogLine {
    pub stage: String,
    pub line: String,
    pub level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_number: Option<i32>,
}

/// Reusable calls multiply job identities, so credentials and environment bindings share one budget.
pub const MAX_EXPANDED_JOBS: usize = 1000;

/// Lines one run may store, counting its truncation line.
pub const MAX_RUN_LOG_LINES: i64 = 100_000;
/// Stage and line bytes one run may store.
pub const MAX_RUN_LOG_BYTES: i64 = 32 * 1024 * 1024;
