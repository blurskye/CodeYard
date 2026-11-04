use std::collections::HashMap;
/// Current native repository metadata, including typed effective property values/defaults.
/// Callers must scope the repository before constructing this read projection.
pub struct RepositoryContext<'a> {
    pub id: i64,
    pub name: &'a str,
    pub default_branch: &'a str,
    pub is_private: bool,
    pub is_archived: bool,
    pub is_fork: bool,
    pub properties: &'a HashMap<String, Vec<String>>,
}

use crate::queries::rulesets::invalid;
use crate::queries::rulesets::Conditions;
use crate::queries::rulesets::Definition;
use crate::queries::rulesets::Invalid;
use crate::queries::rulesets::Property;
use crate::queries::rulesets::PropertySource;
use crate::queries::rulesets::Target;
use codeyard_model::workflow_glob::branch_pattern_matches;
use codeyard_model::workflow_glob::WorkflowMatchBudget;

fn matches_any(
    patterns: &[String],
    value: &str,
    default: Option<&str>,
    budget: &mut WorkflowMatchBudget,
) -> Result<bool, Invalid> {
    for pattern in patterns {
        let matched = match pattern.as_str() {
            "~ALL" => true,
            "~DEFAULT_BRANCH" => default == Some(value),
            _ => branch_pattern_matches(pattern, value, budget).map_err(|_| {
                invalid(
                    "conditions",
                    "Ruleset matching exceeded its native pattern budget",
                )
            })?,
        };
        if matched {
            return Ok(true);
        }
    }
    Ok(false)
}
fn matches_property(property: &Property, repository: &RepositoryContext<'_>) -> bool {
    if property.source == PropertySource::Custom {
        return repository
            .properties
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&property.name))
            .is_some_and(|(_, values)| {
                property
                    .property_values
                    .iter()
                    .any(|expected| values.contains(expected))
            });
    }
    let value = match property.name.as_str() {
        "visibility" => {
            if repository.is_private {
                "private"
            } else {
                "public"
            }
        }
        "name" => repository.name,
        "default_branch" => repository.default_branch,
        "archived" => {
            if repository.is_archived {
                "true"
            } else {
                "false"
            }
        }
        "fork" => {
            if repository.is_fork {
                "true"
            } else {
                "false"
            }
        }
        _ => return false,
    };
    property
        .property_values
        .iter()
        .any(|expected| expected == value)
}
impl Conditions {
    pub fn matches_repository(
        &self,
        repository: &RepositoryContext<'_>,
        budget: &mut WorkflowMatchBudget,
    ) -> Result<bool, Invalid> {
        let conditions = self;
        if let Some(names) = &conditions.repository_name {
            return Ok(matches_any(&names.include, repository.name, None, budget)?
                && !matches_any(&names.exclude, repository.name, None, budget)?);
        }
        if let Some(ids) = &conditions.repository_id {
            return Ok(ids.repository_ids.contains(&repository.id));
        }
        if let Some(properties) = &conditions.repository_property {
            return Ok(properties
                .include
                .iter()
                .all(|property| matches_property(property, repository))
                && !properties
                    .exclude
                    .iter()
                    .any(|property| matches_property(property, repository)));
        }
        // A persisted unknown selector is not an absent condition.
        Err(invalid("conditions", "The repository selector is invalid"))
    }
}
impl Definition {
    pub fn matches_repository(
        &self,
        repository: &RepositoryContext<'_>,
        budget: &mut WorkflowMatchBudget,
    ) -> Result<bool, Invalid> {
        self.conditions.as_ref().map_or(Ok(true), |conditions| {
            conditions.matches_repository(repository, budget)
        })
    }

    pub fn matches_reference(
        &self,
        reference: &str,
        default_branch: &str,
        budget: &mut WorkflowMatchBudget,
    ) -> Result<bool, Invalid> {
        let applicable = match self.target {
            Target::Branch => reference.starts_with("refs/heads/"),
            Target::Tag => reference.starts_with("refs/tags/"),
            Target::Push => {
                reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/")
            }
        };
        if !applicable {
            return Ok(false);
        }
        let Some(refs) = self
            .conditions
            .as_ref()
            .and_then(|conditions| conditions.ref_name.as_ref())
        else {
            return Ok(true);
        };
        let default = format!("refs/heads/{default_branch}");
        Ok(
            matches_any(&refs.include, reference, Some(&default), budget)?
                && !matches_any(&refs.exclude, reference, Some(&default), budget)?,
        )
    }
}
