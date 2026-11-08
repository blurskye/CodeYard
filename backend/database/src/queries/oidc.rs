use anyhow::ensure;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
pub(crate) const INCLUSION_PROPERTY_SQL: &str = "SELECT id FROM organization_repository_properties WHERE org_id=$1 AND property_name=$2 AND ($3 OR value_type IN ('string','single_select','multi_select','true_false'))";

pub(crate) const INCLUDED_DEFINITIONS_SQL: &str = "SELECT p.* FROM organization_repository_properties p JOIN actions_oidc_properties o ON o.definition_id=p.id WHERE p.org_id=$1 ORDER BY lower(p.property_name),p.id";

#[derive(Clone, Copy)]
pub enum Scope {
    Repository(i64),
    Organization(i64),
}

impl Scope {
    pub fn storage(self) -> (&'static str, &'static str, i64) {
        match self {
            Self::Repository(id) => ("actions_oidc_repo_settings", "repo_id", id),
            Self::Organization(id) => ("actions_oidc_org_settings", "org_id", id),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub use_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_claim_keys: Option<Vec<String>>,
}

impl Settings {
    pub fn from_keys(use_default: bool, keys: &str) -> Self {
        let keys = keys
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Self {
            use_default,
            include_claim_keys: (!keys.is_empty()).then_some(keys),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.use_default {
            return Ok(());
        }
        if let Some(keys) = &self.include_claim_keys {
            let mut seen = HashSet::new();
            ensure!(!keys.is_empty() && keys.len() <= 100, Refusal::Invalid);
            for key in keys {
                ensure!(seen.insert(key) && valid_claim(key), Refusal::Invalid);
            }
        }
        Ok(())
    }
}

fn valid_claim(key: &str) -> bool {
    matches!(
        key,
        "repo"
            | "context"
            | "repository"
            | "repository_owner"
            | "repository_id"
            | "repository_owner_id"
            | "actor"
            | "actor_id"
            | "workflow"
            | "workflow_ref"
            | "workflow_sha"
            | "job_workflow_ref"
            | "job_workflow_sha"
            | "event_name"
            | "ref"
            | "ref_type"
            | "sha"
            | "run_id"
            | "run_number"
            | "run_attempt"
            | "runner_environment"
            | "environment"
            | "repository_visibility"
    ) || key
        .strip_prefix("repo_property_")
        .is_some_and(crate::queries::organizations::properties::valid_name)
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("OIDC administration denied")]
    Denied,
    #[error("Invalid OIDC configuration")]
    Invalid,
    #[error("OIDC property inclusion already exists")]
    Duplicate,
    #[error("OIDC property inclusion not found")]
    Missing,
}

#[derive(Serialize)]
pub struct Inclusion {
    pub custom_property_name: String,
    pub inclusion_source: &'static str,
}

pub struct TokenSettings {
    pub include_claim_keys: Option<Vec<String>>,
    pub properties: BTreeMap<String, String>,
}
