//! Repository creation and edits use native provisioning and settings transactions.
use super::{
    db, missing,
    mutations::{target, text},
    node_id, repository, token_denied, unavailable, RequestContext, Result, Value,
};
use crate::backend::{api::github, repositories::lifecycle};
use async_graphql::Error;
use db::repositories::{
    creation::{RepositoryCreationOwner, RepositoryTarget},
    records, topics,
};
use serde_json::json;
pub(super) const FIELDS: &[&str] = &[
    "createRepository",
    "cloneTemplateRepository",
    "updateRepository",
];
fn settings(error: anyhow::Error) -> Error {
    github::repos::settings_error(error).graphql()
}
fn provision(error: lifecycle::StorageError) -> Error {
    github::repos::lifecycle_error(error).graphql()
}
fn partial(message: &str) -> Error {
    use async_graphql::ErrorExtensions;
    Error::new(message).extend_with(|_, e| e.set("type", "PARTIAL_UPDATE"))
}
fn capabilities(input: &Value) -> Result<()> {
    for (name, enabled) in [
        ("hasIssuesEnabled", true),
        ("hasPullRequestsEnabled", true),
        ("hasProjectsEnabled", true),
        ("hasSponsorshipsEnabled", false),
    ] {
        if input[name].as_bool().is_some_and(|value| value != enabled) {
            return Err(Error::new(format!(
                "Codeyard does not support {name}={}",
                !enabled
            )));
        }
    }
    Ok(())
}
pub(super) async fn resolve(field: &str, input: &Value, request: &RequestContext) -> Result<Value> {
    let allowed: &[&str] = if field == "createRepository" {
        &[
            "clientMutationId",
            "name",
            "description",
            "visibility",
            "ownerId",
            "teamId",
            "homepageUrl",
            "hasIssuesEnabled",
            "hasWikiEnabled",
            "template",
        ]
    } else if field == "cloneTemplateRepository" {
        &[
            "clientMutationId",
            "name",
            "description",
            "visibility",
            "ownerId",
            "repositoryId",
            "includeAllBranches",
        ]
    } else {
        &[
            "clientMutationId",
            "repositoryId",
            "name",
            "description",
            "homepageUrl",
            "hasIssuesEnabled",
            "hasPullRequestsEnabled",
            "hasWikiEnabled",
            "hasProjectsEnabled",
            "hasDiscussionsEnabled",
            "hasSponsorshipsEnabled",
            "template",
        ]
    };
    if let Some(key) = input
        .as_object()
        .into_iter()
        .flat_map(|v| v.keys())
        .find(|key| !allowed.contains(&key.as_str()) && !input[*key].is_null())
    {
        return Err(Error::new(format!(
            "Codeyard does not support {field}.{key} yet"
        )));
    }
    capabilities(input)?;
    let actor = request.auth.user.id;
    let pool = &request.state.pool;
    let website = input
        .get("homepageUrl")
        .map(|v| topics::parse_website(v.as_str().unwrap_or("")))
        .transpose()
        .map_err(settings)?;
    let description = input.get("description").map(|v| v.as_str().unwrap_or(""));
    if let Some(value) = description {
        lifecycle::validate_description(value).map_err(provision)?;
    }
    if let Some(name) = input["name"].as_str() {
        lifecycle::validate_name(name).map_err(provision)?;
    }
    let saved = if matches!(field, "createRepository" | "cloneTemplateRepository") {
        if request.bounded() {
            return Err(token_denied());
        }
        // Checked before provisioning, which commits the repository.
        request.require_oauth_scope("repo")?;
        let private = match text(input, "visibility")? {
            "PUBLIC" => false,
            "PRIVATE" => true,
            _ => return Err(Error::new("Codeyard has no internal repository visibility")),
        };
        let owner = if let Some(id) = input["ownerId"].as_str() {
            match node_id::decode(id).ok_or_else(missing)? {
                ("U", id) if id == actor => RepositoryTarget::Personal(actor),
                ("O", id) => RepositoryTarget::Organization(id),
                _ => return Err(missing()),
            }
        } else {
            RepositoryTarget::Personal(actor)
        };
        let team = if let Some(id) = input["teamId"].as_str() {
            let (org, slug) = db::graphql::team_locator(
                pool,
                node_id::decode(id)
                    .filter(|(kind, _)| *kind == "T")
                    .map(|(_, id)| id)
                    .ok_or_else(missing)?,
            )
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
            if owner != RepositoryTarget::Organization(org) {
                return Err(Error::new(
                    "A team grant requires its organization as owner",
                ));
            }
            let team = db::organizations::teams::get_public_team(pool, actor, org, &slug)
                .await
                .map_err(unavailable)?
                .filter(|v| v.manage)
                .ok_or_else(missing)?;
            Some(team.team)
        } else {
            None
        };
        let creation = RepositoryCreationOwner {
            actor_id: actor,
            target: owner,
        };
        let row = if field == "cloneTemplateRepository" {
            let source = repository::by_id(target(input, "repositoryId", "R")?, request).await?;
            request.authorize(source.id, "contents", false).await?;
            lifecycle::generate(
                (pool, request.state.cluster.as_ref()),
                &request.state.repos_path,
                creation,
                source.id,
                lifecycle::TemplateOptions {
                    name: text(input, "name")?,
                    description,
                    private,
                    all_branches: input["includeAllBranches"].as_bool().unwrap_or(false),
                    properties: &[],
                },
            )
            .await
        } else {
            lifecycle::provision(
                (pool, request.state.cluster.as_ref()),
                &request.state.repos_path,
                creation,
                text(input, "name")?,
                description,
                private,
                None,
            )
            .await
        }
        .map_err(provision)?;
        records::set_repository_wiki(
            pool,
            actor,
            row.id,
            input["hasWikiEnabled"].as_bool().unwrap_or(true),
        )
        .await
        .map_err(|_| partial("Repository created; wiki setting was not confirmed"))?;
        if let Some(template) = input["template"].as_bool() {
            records::patch_repository(
                pool,
                actor,
                row.id,
                &records::RepositoryChanges {
                    is_template: Some(template),
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| partial("Repository created; template setting was not confirmed"))?;
        }
        if let Some(website) = website {
            topics::update_about(
                pool,
                actor,
                row.id,
                description.unwrap_or(""),
                website.as_deref(),
                &[],
            )
            .await
            .map_err(|_| partial("Repository created; About metadata was not confirmed"))?;
        }
        if let Some(team) = team {
            use db::organizations::OrganizationOutcome;
            let granted = db::organizations::teams::grant_organization_team_repository(
                pool,
                actor,
                team.org_id,
                team.id,
                row.id,
                db::organizations::teams::TeamRepositoryPermission::new(3).map_err(unavailable)?,
            )
            .await
            .map_err(|_| partial("Repository created; team grant was not confirmed"))?;
            if !matches!(granted, OrganizationOutcome::Applied(_)) {
                return Err(partial("Repository created; team grant was not confirmed"));
            }
        }
        repository::by_id(row.id, request).await?
    } else {
        let row = repository::by_id(target(input, "repositoryId", "R")?, request).await?;
        request.authorize(row.id, "administration", true).await?;
        let about = description.is_some() || website.is_some();
        if let Some(enabled) = input["hasDiscussionsEnabled"].as_bool() {
            db::repository_discussions::write(
                pool,
                actor,
                &request.auth.credential(),
                row.id,
                None,
                &db::repository_discussions::Write::Enabled(enabled),
            )
            .await
            .map_err(super::discussions::operation)?;
        }
        if let Some(enabled) = input["hasWikiEnabled"].as_bool() {
            records::set_repository_wiki(pool, actor, row.id, enabled)
                .await
                .map_err(settings)?;
        }
        if let Some(template) = input["template"].as_bool() {
            records::patch_repository(
                pool,
                actor,
                row.id,
                &records::RepositoryChanges {
                    is_template: Some(template),
                    ..Default::default()
                },
            )
            .await
            .map_err(settings)?;
        }
        // Each existing native save retains its own live authority; a later refusal is partial,
        // never a claim that an earlier metadata transaction rolled back.
        if about {
            // Only the submitted fields are written; unsubmitted values are never read back and resent.
            records::patch_repository(
                pool,
                actor,
                row.id,
                &records::RepositoryChanges {
                    description: description.map(Some),
                    website: website.as_ref().map(|value| value.as_deref()),
                    ..Default::default()
                },
            )
            .await
            .map_err(settings)?;
        }
        if let Some(name) = input["name"].as_str() {
            records::rename_repository(pool, actor, row.id, name)
                .await
                .map_err(|error| {
                    if about {
                        partial("About metadata saved; repository rename was not confirmed")
                    } else {
                        settings(error)
                    }
                })?;
        }
        repository::by_id(row.id, request).await?
    };
    Ok(
        json!({"clientMutationId":input["clientMutationId"],"repository":repository::object(&saved)}),
    )
}
