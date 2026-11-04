use super::*;
use crate::sqlite::organizations::authority::site_administrator;
use crate::sqlite::organizations::rulesets::view;
use codeyard_db_core::models::Repository;
use codeyard_db_core::queries::organizations::properties::PropertyValue;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::rulesets::matching::RepositoryContext;
use codeyard_db_core::queries::rulesets::records::ListFilter;
use codeyard_db_core::queries::rulesets::records::Owner;
use codeyard_db_core::queries::sqlite::organizations::authority::active;
use codeyard_db_core::queries::sqlite::organizations::authority::authority;
use codeyard_db_core::queries::sqlite::organizations::authority::read_authority;
use codeyard_model::workflow_glob::WorkflowMatchBudget;
use sqlx::Sqlite;
use sqlx::Transaction;
use std::collections::HashMap;

pub(super) async fn authorize(
    tx: &mut Transaction<'_, Sqlite>,
    actor: Option<i64>,
    owner: Owner,
    write: bool,
) -> Result<OrganizationOutcome<bool>> {
    match owner {
        Owner::Organization(org) => {
            let Some(actor) = actor else {
                return Ok(Denied);
            };
            let authority = if write {
                authority(tx, actor, org).await?
            } else {
                read_authority(tx, actor, org).await?
            };
            Ok(authority.map(|role| role == OrgRole::Owner))
        }
        Owner::Repository(repo) => {
            if write {
                let Some(actor) = actor else {
                    return Ok(Denied);
                };
                return Ok(Applied(
                    codeyard_db_core::queries::sqlite::discussions::lock_admin_authority(
                        tx, actor, repo,
                    )
                    .await?,
                ));
            }
            let Some(repository) =
                sqlx::query_as::<_, Repository>("SELECT * FROM repositories WHERE id=?1")
                    .bind(repo)
                    .fetch_optional(&mut **tx)
                    .await?
            else {
                return Ok(Missing);
            };
            let Some(actor) = actor else {
                return Ok(if repository.is_private {
                    Missing
                } else {
                    Applied(false)
                });
            };
            if !active(tx, actor).await? {
                return Ok(Denied);
            }
            let level =
                codeyard_db_core::queries::sqlite::repositories::access::user_repo_access_in_transaction(
                    tx, actor, repo,
                )
                .await?;
            let personal_admin =
                repository.owner_kind == "user" && site_administrator(tx, actor).await?;
            if repository.is_private && !level.can_read() && !personal_admin {
                return Ok(Missing);
            }
            Ok(Applied(level.can_admin() || personal_admin))
        }
    }
}

pub async fn rows(
    tx: &mut Transaction<'_, Sqlite>,
    actor: Option<i64>,
    owner: Owner,
    editable: bool,
    filter: &ListFilter,
) -> Result<Vec<View<Ruleset>>> {
    let (org, repo) = owner.ids();
    let stored: Vec<StoredRuleset> = sqlx::query_as(
        "SELECT * FROM repository_rulesets WHERE owner_org_id=?1 OR owner_repo_id=?2 ORDER BY id",
    )
    .bind(org)
    .bind(repo)
    .fetch_all(&mut **tx)
    .await?;
    let mut rows = stored
        .into_iter()
        .map(|row| row.decode().map(|row| view(row, editable)))
        .collect::<Result<Vec<_>>>()?;
    if let Owner::Repository(repo) = owner {
        if filter.includes_parents {
            let repository: Repository = sqlx::query_as("SELECT * FROM repositories WHERE id=?1")
                .bind(repo)
                .fetch_one(&mut **tx)
                .await?;
            if let Some(org) = repository
                .owner_org_id
                .filter(|_| repository.owner_kind == "org")
            {
                let parents: Vec<StoredRuleset> = sqlx::query_as(
                    "SELECT * FROM repository_rulesets WHERE owner_org_id=?1 ORDER BY id",
                )
                .bind(org)
                .fetch_all(&mut **tx)
                .await?;
                let parents = parents
                    .into_iter()
                    .map(StoredRuleset::decode)
                    .collect::<Result<Vec<_>>>()?;
                let needs_properties = parents.iter().any(|parent| {
                    parent
                        .definition
                        .conditions
                        .as_ref()
                        .and_then(|conditions| conditions.repository_property.as_ref())
                        .is_some_and(|properties| {
                            !properties.include.is_empty() || !properties.exclude.is_empty()
                        })
                });
                let values = if needs_properties {
                    let definitions =
                        crate::sqlite::organizations::properties::definitions_on(tx, org).await?;
                    codeyard_db_core::queries::sqlite::organizations::properties::values_on(
                        tx,
                        repo,
                        &definitions,
                    )
                    .await?
                } else {
                    Vec::new()
                };
                let properties: HashMap<String, Vec<String>> = values
                    .into_iter()
                    .filter_map(|value| {
                        value.value.map(|v| {
                            (
                                value.property_name,
                                match v {
                                    PropertyValue::String(v) => vec![v],
                                    PropertyValue::Strings(v) => v,
                                },
                            )
                        })
                    })
                    .collect();
                let context = RepositoryContext {
                    id: repo,
                    name: &repository.name,
                    default_branch: &repository.default_branch,
                    is_private: repository.is_private,
                    is_archived: repository.is_archived,
                    is_fork: repository.is_fork,
                    properties: &properties,
                };
                let parent_editable = if let Some(actor) = actor {
                    matches!(
                        read_authority(tx, actor, org).await?,
                        Applied(OrgRole::Owner)
                    )
                } else {
                    false
                };
                let mut budget = WorkflowMatchBudget::new();
                for parent in parents {
                    if parent
                        .definition
                        .matches_repository(&context, &mut budget)?
                    {
                        rows.push(view(parent, parent_editable));
                    }
                }
            }
        }
    }
    rows.retain(|row| {
        filter
            .targets
            .as_ref()
            .is_none_or(|targets| targets.contains(&row.value.definition.target))
    });
    rows.sort_by_key(|row| row.value.id);
    Ok(rows)
}
