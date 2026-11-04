use anyhow::ensure;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
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
    pub fn organization_permission(self) -> &'static str {
        if matches!(self, Self::Runners) {
            "organization_self_hosted_runners"
        } else {
            "organization_administration"
        }
    }

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

    pub fn update(self, stored: &mut Value, input: &Value, maximum: i64) -> Result<()> {
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
                (Self::ContributorApproval, "approval_policy") => {
                    enumeration(value, &["all_external_contributors"])
                }
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
        let mut value = stored.get(self.key()).cloned().unwrap_or_else(|| {
            if matches!(self, Self::Workflow) {
                json!({})
            } else {
                self.defaults()
            }
        });
        for (key, field) in fields {
            value[key] = field.clone();
        }
        stored[self.key()] = value;
        Ok(())
    }
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

pub enum SelectionChange {
    Set(Vec<i64>),
    Add(i64),
    Remove(i64),
}

pub(crate) fn change_selection(
    stored: &mut Value,
    setting: Setting,
    change: &SelectionChange,
) -> Result<Vec<i64>> {
    ensure!(
        matches!(setting, Setting::Permissions | Setting::Runners),
        Refusal::Invalid
    );
    ensure!(
        stored[setting.key()]["enabled_repositories"] == "selected",
        Refusal::Invalid
    );
    let mut ids: Vec<i64> = stored[setting.key()]["selected_repository_ids"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let added = match change {
        SelectionChange::Set(next) => {
            ids = next.clone();
            next.clone()
        }
        SelectionChange::Add(id) => {
            ids.push(*id);
            vec![*id]
        }
        SelectionChange::Remove(id) => {
            ids.retain(|value| value != id);
            Vec::new()
        }
    };
    ids.sort_unstable();
    ids.dedup();
    ensure!(
        ids.len() <= 10_000 && ids.iter().all(|id| *id > 0),
        Refusal::Invalid
    );
    stored[setting.key()]["selected_repository_ids"] = json!(ids);
    Ok(added)
}
