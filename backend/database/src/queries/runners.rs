use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    Organization(i64),
    Repository(i64),
}

impl Scope {
    pub fn ids(self) -> (Option<i64>, Option<i64>) {
        match self {
            Self::Organization(id) => (Some(id), None),
            Self::Repository(id) => (None, Some(id)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Runner resource not found")]
    Missing,
    #[error("Runner administration permission required")]
    Denied,
    #[error("Invalid {0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Group {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub visibility: String,
    pub is_default: bool,
    pub allows_public_repositories: bool,
    pub restricted_to_workflows: bool,
    pub selected_workflows: String,
    #[sqlx(skip)]
    pub repositories: Vec<i64>,
}

impl Group {
    pub fn allows_repository(&self, id: i64, private: bool) -> bool {
        (private || self.allows_public_repositories)
            && match self.visibility.as_str() {
                "all" => true,
                "private" => private,
                "selected" => self.repositories.contains(&id),
                _ => false,
            }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Runner {
    pub id: i64,
    pub org_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub group_id: Option<i64>,
    pub node_id: Option<i64>,
    pub name: String,
    pub os: String,
    pub labels: String,
    pub ephemeral: bool,
    pub used: bool,
    pub online: bool,
    pub busy: bool,
    pub version: Option<String>,
}

#[derive(sqlx::FromRow)]
pub struct RunnerProjection {
    #[sqlx(flatten)]
    pub runner: Runner,
    pub node_status: Option<String>,
    pub node_last_seen: Option<chrono::DateTime<chrono::Utc>>,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

impl RunnerProjection {
    pub fn into_runner(mut self) -> Runner {
        self.runner.online = crate::models::cluster::heartbeat_online(
            self.node_status.as_deref().unwrap_or(""),
            self.node_last_seen,
            self.observed_at,
        );
        self.runner
    }
}

#[derive(Clone, Copy)]
pub struct Window {
    pub offset: i64,
    pub limit: i64,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 30,
        }
    }
}

impl Window {
    pub fn validate(self) -> Result<()> {
        anyhow::ensure!(
            self.offset >= 0 && (1..=100).contains(&self.limit),
            Refusal::Invalid("page")
        );
        Ok(())
    }
}

#[derive(Default)]
pub struct RunnerQuery {
    pub page: Window,
    pub id: Option<i64>,
    pub group: Option<i64>,
    pub name: Option<String>,
}
