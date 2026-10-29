//! Self-hosted runner administration. Registration uses the existing cluster pairing path.
use super::DbPool;
use anyhow::Result;
use serde::{Deserialize, Serialize};

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
#[derive(Clone, Debug, Default, Deserialize)]
pub struct GroupInput {
    pub name: Option<String>,
    pub visibility: Option<String>,
    pub allows_public_repositories: Option<bool>,
    pub restricted_to_workflows: Option<bool>,
    pub selected_workflows: Option<Vec<String>>,
    pub selected_repository_ids: Option<Vec<i64>>,
    pub runners: Option<Vec<i64>>,
    pub network_configuration_id: Option<String>,
}
impl GroupInput {
    /// Whether the edit can take access away from runs already on the group's runners. A
    /// rename alone cannot; runners joining or leaving are handled one by one.
    pub fn changes_access(&self, existing: &Group) -> bool {
        self.visibility.as_ref().is_some_and(|value| value != &existing.visibility)
            || self.allows_public_repositories.is_some_and(|value| value != existing.allows_public_repositories)
            || self.restricted_to_workflows.is_some_and(|value| value != existing.restricted_to_workflows)
            || self.selected_workflows.as_ref().is_some_and(|values| {
                let previous = serde_json::from_str::<Vec<String>>(&existing.selected_workflows);
                previous.map_or(true, |previous| values.iter().collect::<std::collections::BTreeSet<_>>() != previous.iter().collect())
            })
            || self.selected_repository_ids.as_ref().is_some_and(|values| values.iter().collect::<std::collections::BTreeSet<_>>() != existing.repositories.iter().collect())
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.name.as_ref().is_none_or(|name| valid_group_name(name)),
            Refusal::Invalid("name")
        );
        anyhow::ensure!(
            self.visibility
                .as_deref()
                .is_none_or(|v| matches!(v, "all" | "private" | "selected")),
            Refusal::Invalid("visibility")
        );
        anyhow::ensure!(
            self.network_configuration_id.is_none(),
            Refusal::Invalid("network_configuration_id")
        );
        if let Some(workflows) = &self.selected_workflows {
            anyhow::ensure!(
                workflows.len() <= 100
                    && workflows.iter().all(|w| w.len() <= 1024
                        && !w.chars().any(char::is_control)
                        && w.split_once('@').is_some_and(|(path, reference)| {
                            let parts: Vec<_> = path.split('/').collect();
                            parts.len() == 5
                                && !parts[0].is_empty()
                                && !parts[1].is_empty()
                                && parts[2] == ".github"
                                && parts[3] == "workflows"
                                && !parts[4].is_empty()
                                && !parts[4].contains("..")
                                && parts[4]
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                                && (parts[4].ends_with(".yml") || parts[4].ends_with(".yaml"))
                                && !reference.is_empty()
                                && !reference.contains("${{")
                                && git2::Reference::is_valid_name(
                                    &if reference.starts_with("refs/") {
                                        reference.to_owned()
                                    } else {
                                        format!("refs/heads/{reference}")
                                    },
                                )
                        })),
                Refusal::Invalid("selected_workflows")
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub enum Selection {
    Replace(Vec<i64>),
    Add(i64),
    Remove(i64),
}
impl Selection {
    pub fn apply(&self, current: Vec<i64>) -> Result<Vec<i64>> {
        let mut ids = match self {
            Self::Replace(ids) => ids.clone(),
            _ => current,
        };
        match self {
            Self::Add(id) => ids.push(*id),
            Self::Remove(id) => ids.retain(|v| v != id),
            _ => {}
        }
        ids.sort_unstable();
        ids.dedup();
        anyhow::ensure!(
            ids.len() <= 1000 && ids.iter().all(|id| *id > 0),
            Refusal::Invalid("ids")
        );
        Ok(ids)
    }
}
#[derive(Clone, Debug)]
pub enum Labels {
    Add(Vec<String>),
    Replace(Vec<String>),
    Remove(String),
    Clear,
}
/// Keep JIT names out of the operator's ordinary node-name namespace. Only a hash
/// prefix is exposed; the pairing secret remains inside the credential.
pub(crate) fn jit_node_name(id: i64, hash: &str) -> String {
    format!("jit-{id}-{}", hash.chars().take(32).collect::<String>())
}
pub fn valid_group_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 100 && !name.chars().any(char::is_control)
}
pub fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 128
        && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}
pub fn normalize_labels(labels: &[String]) -> Result<Vec<String>> {
    anyhow::ensure!(labels.len() <= 100, Refusal::Invalid("labels"));
    let mut labels: Vec<_> = labels.iter().map(|s| s.to_ascii_lowercase()).collect();
    anyhow::ensure!(
        labels.iter().all(|label| valid_label(label)),
        Refusal::Invalid("labels")
    );
    labels.retain(|label| label != "self-hosted");
    labels.sort();
    labels.dedup();
    Ok(labels)
}
#[derive(sqlx::FromRow)]
pub(crate) struct RegistrationBinding {
    pub actor_id: i64,
    pub org_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub group_id: Option<i64>,
    pub runner_id: Option<i64>,
}
#[derive(sqlx::FromRow)]
pub(crate) struct Candidate {
    pub org_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub group_id: Option<i64>,
    pub labels: String,
    pub used: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Scheduling {
    pub labels: Vec<String>,
    pub group: Option<String>,
    pub concurrency: Option<super::action_concurrency::Configuration>,
}

#[derive(Clone, Debug)]
pub struct Registration<'a> {
    pub session: Option<&'a str>,
    pub hash: &'a str,
    pub expires: chrono::DateTime<chrono::Utc>,
    pub name: Option<&'a str>,
    pub group: Option<i64>,
    pub labels: &'a [String],
}

pub(crate) fn workflow_allowed(
    allowed: &[String],
    workflow: &str,
    reference: &str,
    sha: Option<&str>,
) -> bool {
    let prefix = format!("{workflow}@");
    allowed.iter().any(|rule| {
        let Some(selected) = rule.strip_prefix(&prefix) else {
            return false;
        };
        if crate::backend::git::is_full_commit_id(selected) {
            sha.is_some_and(|sha| sha.eq_ignore_ascii_case(selected))
        } else {
            selected == reference
                || reference
                    .strip_prefix("refs/heads/")
                    .or_else(|| reference.strip_prefix("refs/tags/"))
                    .is_some_and(|short| short == selected)
        }
    })
}

native_operation!(crate::backend::db::postgres::runners::list_groups, crate::backend::db::sqlite::runners::list_groups,
    fn list_groups(pool:&DbPool,actor:i64,org:i64)->Result<Vec<Group>>);
native_operation!(crate::backend::db::postgres::runners::save_group, crate::backend::db::sqlite::runners::save_group,
    fn save_group(pool:&DbPool,actor:i64,org:i64,id:Option<i64>,input:&GroupInput)->Result<Group>);
native_operation!(crate::backend::db::postgres::runners::delete_group, crate::backend::db::sqlite::runners::delete_group,
    fn delete_group(pool:&DbPool,actor:i64,org:i64,id:i64)->Result<()>);
native_operation!(crate::backend::db::postgres::runners::group_repositories, crate::backend::db::sqlite::runners::group_repositories,
    fn group_repositories(pool:&DbPool,actor:i64,org:i64,id:i64,selection:&Selection)->Result<()>);
native_operation!(crate::backend::db::postgres::runners::group_runners, crate::backend::db::sqlite::runners::group_runners,
    fn group_runners(pool:&DbPool,actor:i64,org:i64,id:i64,selection:&Selection)->Result<()>);
native_operation!(crate::backend::db::postgres::runners::list_runners, crate::backend::db::sqlite::runners::list_runners,
    fn list_runners(pool:&DbPool,actor:i64,scope:Scope)->Result<Vec<Runner>>);
native_operation!(crate::backend::db::postgres::runners::delete_runner, crate::backend::db::sqlite::runners::delete_runner,
    fn delete_runner(pool:&DbPool,actor:i64,scope:Scope,id:i64)->Result<()>);
native_operation!(crate::backend::db::postgres::runners::labels, crate::backend::db::sqlite::runners::labels,
    fn labels(pool:&DbPool,actor:i64,scope:Scope,id:i64,change:&Labels)->Result<Runner>);
native_operation!(crate::backend::db::postgres::runners::registration, crate::backend::db::sqlite::runners::registration,
    fn registration(pool:&DbPool,actor:i64,scope:Scope,input:Registration<'_>)->Result<Option<Runner>>);
native_operation!(crate::backend::db::postgres::runners::remove_token, crate::backend::db::sqlite::runners::remove_token,
    fn remove_token(pool:&DbPool,actor:i64,scope:Scope,hash:&str,expires:chrono::DateTime<chrono::Utc>)->Result<()>);
native_operation!(crate::backend::db::postgres::runners::remove_with_token, crate::backend::db::sqlite::runners::remove_with_token,
    fn remove_with_token(pool:&DbPool,id:i64,hash:&str)->Result<()>);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runner_workflow_sha_cannot_be_impersonated_by_a_branch_name() {
        let sha = "a".repeat(40);
        let workflow = "owner/repo/.github/workflows/ci.yml";
        let rules = vec![format!("{workflow}@{sha}")];
        assert!(!workflow_allowed(
            &rules,
            workflow,
            &format!("refs/heads/{sha}"),
            Some(&"b".repeat(40))
        ));
        assert!(workflow_allowed(
            &rules,
            workflow,
            "refs/pull/1/merge",
            Some(&sha)
        ));
        let rules = vec![format!("{workflow}@main")];
        assert!(workflow_allowed(
            &rules,
            workflow,
            "refs/heads/main",
            Some(&sha)
        ));
        assert!(!workflow_allowed(
            &rules,
            workflow,
            "refs/pull/1/merge",
            Some(&sha)
        ));
        let rules = vec![format!("{workflow}@feature/user@work")];
        assert!(workflow_allowed(
            &rules,
            workflow,
            "refs/heads/feature/user@work",
            None
        ));
    }
    #[test]
    fn runner_labels_preserve_builtin_and_deduplicate_custom_names() {
        assert_eq!(
            normalize_labels(&["Self-Hosted".into(), "LINUX".into(), "linux".into()]).unwrap(),
            ["linux"]
        );
        assert!(normalize_labels(&["${{ expression }}".into()]).is_err());
    }
}
