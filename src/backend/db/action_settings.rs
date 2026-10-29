//! Organization and repository Actions settings share their native settings rows.
use super::DbPool;
use anyhow::{ensure, Result};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub enum Scope {
    Repository(i64),
    Organization(String),
}

#[derive(Clone, Copy)]
pub enum Setting {
    Allowed,
    Retention,
    ContributorApproval,
    Workflow,
    ForkWorkflows,
    Access,
    /// Organization switch for which repositories may run Actions.
    Permissions,
    /// Organization switch for which repositories may use self-hosted runners.
    Runners,
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Actions administration required")]
    Denied,
    #[error("Actions resource not found")]
    Missing,
    #[error("Invalid Actions configuration")]
    Invalid,
    #[error("Deployment is no longer pending")]
    Conflict,
}

impl Setting {
    pub fn key(self) -> &'static str {
        match self {
            Self::Allowed => "selected-actions",
            Self::Retention => "artifact-and-log-retention",
            Self::ContributorApproval => "fork-pr-contributor-approval",
            Self::Workflow => "workflow",
            Self::ForkWorkflows => "fork-pr-workflows-private-repos",
            Self::Access => "access",
            Self::Permissions => "permissions",
            Self::Runners => "self-hosted-runners",
        }
    }

    pub fn defaults(self) -> Value {
        match self {
            Self::Allowed => {
                json!({"github_owned_allowed":false,"verified_allowed":false,"patterns_allowed":[]})
            }
            Self::Retention => json!({"days":90}),
            Self::ContributorApproval => json!({"approval_policy":"all_external_contributors"}),
            Self::Workflow => {
                json!({"default_workflow_permissions":"read","can_approve_pull_request_reviews":false})
            }
            Self::ForkWorkflows => {
                json!({"run_workflows_from_fork_pull_requests":false,"send_write_tokens_to_workflows":false,"send_secrets_and_variables":false,"require_approval_for_fork_pr_workflows":true})
            }
            Self::Access => json!({"access_level":"none"}),
            Self::Permissions => {
                json!({"enabled_repositories":"all","allowed_actions":"all","sha_pinning_required":false})
            }
            Self::Runners => json!({"enabled_repositories":"all"}),
        }
    }

    pub(crate) fn update(self, stored: &mut Value, input: &Value, maximum: i64) -> Result<()> {
        let fields = input.as_object().ok_or(Refusal::Invalid)?;
        let strings = |v: &Value| {
            v.as_array().is_some_and(|values| {
                values.len() <= 1000
                    && values.iter().all(|v| {
                        v.as_str().is_some_and(|s| {
                            !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
                        })
                    })
            })
        };
        let enumeration =
            |v: &Value, values: &[&str]| v.as_str().is_some_and(|s| values.contains(&s));
        for (key, value) in fields {
            let valid = match (self, key.as_str()) {
                (Self::Allowed, "github_owned_allowed" | "verified_allowed")
                | (Self::Workflow, "can_approve_pull_request_reviews")
                | (
                    Self::ForkWorkflows,
                    "run_workflows_from_fork_pull_requests"
                    | "send_write_tokens_to_workflows"
                    | "send_secrets_and_variables"
                    | "require_approval_for_fork_pr_workflows",
                ) => value.is_boolean(),
                (Self::Allowed, "patterns_allowed") => strings(value),
                (Self::Retention, "days") => value
                    .as_i64()
                    .is_some_and(|days| (1..=maximum).contains(&days)),
                (Self::ContributorApproval, "approval_policy") => enumeration(
                    value,
                    &[
                        "first_time_contributors_new_to_github",
                        "first_time_contributors",
                        "all_external_contributors",
                    ],
                ),
                (Self::Workflow, "default_workflow_permissions") => {
                    enumeration(value, &["read", "write"])
                }
                (Self::Access, "access_level") => {
                    enumeration(value, &["none", "user", "organization"])
                }
                (Self::Permissions | Self::Runners, "enabled_repositories") => {
                    enumeration(value, &["all", "none", "selected"])
                }
                // Only unrestricted action sources are enforced, as for repositories.
                (Self::Permissions, "allowed_actions") => enumeration(value, &["all"]),
                (Self::Permissions, "sha_pinning_required") => value.as_bool() == Some(false),
                (Self::Permissions | Self::Runners, "selected_repository_ids") => {
                    value.as_array().is_some_and(|ids| {
                        ids.len() <= 10_000
                            && ids.iter().all(|id| id.as_i64().is_some_and(|id| id > 0))
                    })
                }
                _ => false,
            };
            ensure!(valid, Refusal::Invalid);
        }
        let required = match self {
            Self::Retention => Some("days"),
            Self::ContributorApproval => Some("approval_policy"),
            Self::ForkWorkflows => Some("run_workflows_from_fork_pull_requests"),
            Self::Access => Some("access_level"),
            _ => None,
        };
        ensure!(
            required.is_none_or(|key| fields.contains_key(key)),
            Refusal::Invalid
        );
        let mut value = stored
            .get(self.key())
            .cloned()
            .unwrap_or_else(|| self.defaults());
        for (key, field) in fields {
            value[key] = field.clone();
        }
        stored[self.key()] = value;
        Ok(())
    }
}

/// Whether an organization switch lets one of its repositories use the feature.
pub fn organization_allows(org: &Value, setting: Setting, repo: i64) -> bool {
    let policy = &org[setting.key()];
    match policy["enabled_repositories"].as_str() {
        Some("none") => false,
        Some("selected") => policy["selected_repository_ids"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id.as_i64() == Some(repo))),
        _ => true,
    }
}

/// Actions start only when the site, the repository and its organization all allow them.
pub async fn actions_enabled(pool: &DbPool, repo: i64) -> Result<bool> {
    if !super::actions::get_actions_settings(pool).await?.enabled
        || !super::actions::get_repo_actions_settings(pool, repo)
            .await?
            .enabled
    {
        return Ok(false);
    }
    let (_, org) = effective(pool, repo).await?;
    Ok(organization_allows(&org, Setting::Permissions, repo))
}

/// A private repository runs workflows from fork pull requests only when it turned them on
/// and its organization, if it chose, did not turn them off. Public repositories always do.
pub fn fork_workflows_allowed(repo: &Value, org: &Value) -> bool {
    let allowed = |scope: &Value| {
        scope
            .get(Setting::ForkWorkflows.key())
            .and_then(|v| v.get("run_workflows_from_fork_pull_requests"))
            .and_then(Value::as_bool)
    };
    allowed(org) != Some(false) && allowed(repo).or(allowed(org)).unwrap_or(false)
}

/// Actions runs may approve reviews only when the narrowest scope that set the switch allows it.
pub fn can_approve_reviews(repo: &Value, org: &Value) -> bool {
    repo.get("workflow")
        .and_then(|v| v.get("can_approve_pull_request_reviews"))
        .or_else(|| {
            org.get("workflow")
                .and_then(|v| v.get("can_approve_pull_request_reviews"))
        })
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Resolve defaults only when a narrower scope has not selected its own value.
pub fn workflow_write(repo: &Value, org: &Value, site: bool) -> bool {
    repo.get("workflow")
        .and_then(|v| v.get("default_workflow_permissions"))
        .or_else(|| {
            org.get("workflow")
                .and_then(|v| v.get("default_workflow_permissions"))
        })
        .and_then(Value::as_str)
        .map_or(site, |v| v == "write")
}

/// Artifact retention in days, capped like the settings response: a public repository keeps
/// artifacts at most 90 days.
pub fn retention(repo: &Value, org: &Value, site: Option<i32>, private: bool) -> i32 {
    let days = |value: &Value| {
        value
            .get("artifact-and-log-retention")?
            .get("days")?
            .as_i64()
    };
    days(repo)
        .or_else(|| days(org))
        .unwrap_or(i64::from(site.unwrap_or(90)))
        .min(days(org).unwrap_or(365))
        .min(i64::from(site.unwrap_or(365)))
        .min(if private { 365 } else { 90 }) as i32
}

native_operation!(crate::backend::db::postgres::action_settings::settings,
    crate::backend::db::sqlite::action_settings::settings,
    fn settings(pool: &DbPool, actor: i64, token: i64, scope: &Scope, update: Option<(Setting, &Value)>) -> Result<Value>);
native_operation!(crate::backend::db::postgres::action_settings::effective,
    crate::backend::db::sqlite::action_settings::effective,
    fn effective(pool: &DbPool, repo: i64) -> Result<(Value, Value)>);
native_operation!(crate::backend::db::postgres::action_settings::policies,
    crate::backend::db::sqlite::action_settings::policies,
    fn policies(pool: &DbPool, actor: i64, token: i64, scope: &Scope, id: Option<i64>, change: Option<Option<&Value>>) -> Result<Vec<Policy>>);

pub enum SelectionChange {
    Set(Vec<i64>),
    Add(i64),
    Remove(i64),
}

pub(crate) fn change_selection(stored: &mut Value, setting: Setting, change: &SelectionChange) -> Result<Vec<i64>> {
    ensure!(matches!(setting, Setting::Permissions | Setting::Runners), Refusal::Invalid);
    ensure!(stored[setting.key()]["enabled_repositories"] == "selected", Refusal::Invalid);
    let mut ids: Vec<i64> = stored[setting.key()]["selected_repository_ids"].as_array()
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    let added = match change {
        SelectionChange::Set(next) => { ids = next.clone(); next.clone() }
        SelectionChange::Add(id) => { ids.push(*id); vec![*id] }
        SelectionChange::Remove(id) => { ids.retain(|value| value != id); Vec::new() }
    };
    ids.sort_unstable();
    ids.dedup();
    ensure!(ids.len() <= 10_000 && ids.iter().all(|id| *id > 0), Refusal::Invalid);
    stored[setting.key()]["selected_repository_ids"] = json!(ids);
    Ok(added)
}

native_operation!(crate::backend::db::postgres::action_settings::repositories,
    crate::backend::db::sqlite::action_settings::repositories,
    fn repositories(pool: &DbPool, actor: i64, token: i64, scope: &Scope, setting: Setting, change: Option<&SelectionChange>, window: (i64, i64)) -> Result<(i64, Vec<super::models::Repository>)>);

#[derive(sqlx::FromRow)]
pub struct Policy {
    pub id: i64,
    pub data: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub(crate) fn policy_update(existing: Option<&str>, input: &Value) -> Result<Value> {
    let fields = input.as_object().ok_or(Refusal::Invalid)?;
    let mut result: Value = existing
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_else(|| json!({"rules":[]}));
    for (key, value) in fields {
        let valid = match key.as_str() {
            "name" => value.as_str().is_some_and(|v| {
                !v.trim().is_empty() && v.len() <= 255 && !v.chars().any(char::is_control)
            }),
            "enforcement" => value
                .as_str()
                .is_some_and(|v| matches!(v, "active" | "disabled" | "evaluate")),
            "conditions" => value.is_null() || value.is_object(),
            "rules" => value.as_array().is_some_and(|rules| {
                rules.len() <= 100
                    && rules.iter().all(|rule| {
                        let parameter = match rule["type"].as_str() {
                            Some("restrict_actions_actors") => "allowed_actors",
                            Some("restrict_action_events") => "allowed_events",
                            _ => return false,
                        };
                        rule["parameters"][parameter]
                            .as_array()
                            .is_some_and(|values| {
                                values.len() <= 1000
                                    && values.iter().all(|v| {
                                        if parameter == "allowed_events" {
                                            v.as_str()
                                                .is_some_and(|v| !v.is_empty() && v.len() <= 100)
                                        } else {
                                            v["id"].as_i64().is_some_and(|id| id > 0)
                                                && v["type"].as_str().is_some_and(|v| {
                                                    matches!(
                                                        v,
                                                        "User"
                                                            | "Bot"
                                                            | "Team"
                                                            | "BusinessTeam"
                                                            | "EnterpriseTeam"
                                                            | "IntegrationInstallation"
                                                            | "App"
                                                            | "RepositoryRole"
                                                    )
                                                })
                                        }
                                    })
                            })
                    })
            }),
            _ => false,
        };
        ensure!(valid, Refusal::Invalid);
        result[key] = value.clone();
    }
    ensure!(
        result.get("name").is_some() && result.get("enforcement").is_some(),
        Refusal::Invalid
    );
    // Restrictive rules are stored but no CI admission or claiming path evaluates
    // them yet, so an active policy with rules would claim enforcement that does not run.
    ensure!(
        result["enforcement"] != "active" || result["rules"].as_array().is_some_and(Vec::is_empty),
        Refusal::Invalid
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_fork_workflows_follow_the_repository_and_organization_switches() {
        let on = json!({"fork-pr-workflows-private-repos":{"run_workflows_from_fork_pull_requests":true}});
        let off = json!({"fork-pr-workflows-private-repos":{"run_workflows_from_fork_pull_requests":false}});
        let unset = json!({});
        assert!(!fork_workflows_allowed(&unset, &unset));
        assert!(fork_workflows_allowed(&on, &unset));
        assert!(fork_workflows_allowed(&unset, &on));
        assert!(!fork_workflows_allowed(&off, &on));
        assert!(!fork_workflows_allowed(&on, &off));
    }
}
