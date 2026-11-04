use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde_json::json;
use serde_json::Value;

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
    if !crate::actions::get_actions_settings(pool).await?.enabled
        || !crate::actions::get_repo_actions_settings(pool, repo)
            .await?
            .enabled
    {
        return Ok(false);
    }
    let (_, org) = effective(pool, repo).await?;
    Ok(organization_allows(&org, Setting::Permissions, repo))
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

/// Response defaults must not become persisted overrides of inherited settings.
pub(crate) fn resolved(
    mut stored: Value,
    parent: &Value,
    site: &crate::actions::ActionsSettings,
    private: bool,
    maximum: i64,
) -> Value {
    let days = retention(&stored, parent, site.artifact_retention_days, private);
    let write = workflow_write(
        &stored,
        parent,
        site.default_workflow_permissions == "permissive",
    );
    let approve = can_approve_reviews(&stored, parent);
    stored[Setting::Retention.key()] = json!({"days":days,"maximum_allowed_days":maximum});
    if stored.get(Setting::Workflow.key()).is_none() {
        stored[Setting::Workflow.key()] = json!({});
    }
    stored[Setting::Workflow.key()]["default_workflow_permissions"] =
        json!(if write { "write" } else { "read" });
    stored[Setting::Workflow.key()]["can_approve_pull_request_reviews"] = json!(approve);
    // Older unsupported contributor policies have always executed conservatively. Report
    // that actual behavior, and reject new attempts to select an unenforced policy.
    if stored[Setting::ContributorApproval.key()]["approval_policy"]
        .as_str()
        .is_some_and(|policy| policy != "all_external_contributors")
    {
        stored[Setting::ContributorApproval.key()] = Setting::ContributorApproval.defaults();
    }
    stored
}

native_operation!(crate::postgres::action_settings::settings,
    crate::sqlite::action_settings::settings,
    fn settings(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, scope: &Scope, setting: Setting, update: Option<&Value>) -> Result<Value>);
native_operation!(crate::postgres::action_settings::effective,
    crate::sqlite::action_settings::effective,
    fn effective(pool: &DbPool, repo: i64) -> Result<(Value, Value)>);
native_operation!(crate::postgres::action_settings::policies_page,
    crate::sqlite::action_settings::policies_page,
    fn policies_page(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, scope: &Scope, has_parents: bool, offset: i64, limit: i64) -> Result<(i64, Vec<ListedPolicy>)>);
native_operation!(crate::postgres::action_settings::policies,
    crate::sqlite::action_settings::policies,
    fn policies(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, scope: &Scope, id: Option<i64>, change: Option<Option<&Value>>) -> Result<Vec<Policy>>);

native_operation!(codeyard_db_core::queries::postgres::action_settings::repositories,
    codeyard_db_core::queries::sqlite::action_settings::repositories,
    fn repositories(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, scope: &Scope, setting: Setting, change: Option<&SelectionChange>, window: (i64, i64)) -> Result<(i64, Vec<codeyard_db_core::models::Repository>)>);

#[derive(sqlx::FromRow)]
pub struct Policy {
    pub id: i64,
    pub data: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
pub struct ListedPolicy {
    #[sqlx(flatten)]
    pub policy: Policy,
    pub organization: Option<String>,
}
#[derive(sqlx::FromRow)]
pub(crate) struct PolicySelector {
    pub id: i64,
    pub own: bool,
    pub conditions: Option<String>,
}

impl PolicySelector {
    pub(crate) fn applies(
        &self,
        repository: &codeyard_db_core::queries::rulesets::matching::RepositoryContext<'_>,
        budget: &mut codeyard_model::workflow_glob::WorkflowMatchBudget,
    ) -> Result<bool> {
        if self.own {
            return Ok(true);
        }
        match policy_conditions(self.conditions.as_deref())? {
            Some(conditions) => Ok(conditions.matches_repository(repository, budget)?),
            None => Ok(true),
        }
    }
}

pub(crate) fn policy_conditions(
    stored: Option<&str>,
) -> Result<Option<codeyard_db_core::queries::rulesets::Conditions>> {
    let Some(stored) = stored else {
        return Ok(None);
    };
    let mut value: Value = serde_json::from_str(stored)?;
    if value.is_null() {
        return Ok(None);
    }
    let fields = value.as_object_mut().ok_or(Refusal::Invalid)?;
    fields.remove("workflow_path");
    if fields.is_empty() {
        return Ok(None);
    }
    let conditions: codeyard_db_core::queries::rulesets::Conditions =
        serde_json::from_value(value).map_err(|_| Refusal::Invalid)?;
    ensure!(
        conditions.ref_name.is_none()
            && usize::from(conditions.repository_name.is_some())
                + usize::from(conditions.repository_id.is_some())
                + usize::from(conditions.repository_property.is_some())
                == 1,
        Refusal::Invalid
    );
    Ok(Some(conditions))
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
    policy_conditions(result.get("conditions").map(Value::to_string).as_deref())?;
    // Restrictive rules are stored but no CI admission or claiming path evaluates
    // them yet, so an active policy with rules would claim enforcement that does not run.
    ensure!(
        result["enforcement"] != "active" || result["rules"].as_array().is_some_and(Vec::is_empty),
        Refusal::Invalid
    );
    Ok(result)
}

use codeyard_db_core::queries::action_settings::SelectionChange;

use codeyard_db_core::queries::action_settings::can_approve_reviews;

use codeyard_db_core::queries::action_settings::Setting;

use codeyard_db_core::queries::action_settings::Refusal;

use codeyard_db_core::queries::action_settings::Scope;

use codeyard_db_core::queries::action_settings::retention;

#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_db_core::queries::action_settings::fork_workflows_allowed;

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
