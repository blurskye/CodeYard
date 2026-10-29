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
        if request.auth.is_job() { return Err(token_denied()); }
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
        if request.personal.is_some() {
            use crate::backend::identity::fine_grained;
            let owner = match owner {
                RepositoryTarget::Personal(id) => fine_grained::Owner::User(id),
                RepositoryTarget::Organization(id) => fine_grained::Owner::Organization(id),
            };
            let grant = fine_grained::grant(pool, request.auth.token.id).await.map_err(unavailable)?.ok_or_else(token_denied)?;
            if !grant.create_repository(owner) { return Err(token_denied()); }
        }
        if let RepositoryTarget::Organization(org) = owner {
            if db::fine_grained_tokens::classic_refused(pool, request.auth.token.id, org).await.map_err(unavailable)? {
                return Err(token_denied());
            }
        }
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
        records::patch_repository(pool, actor, row.id, &records::RepositoryChanges {
            token: Some(request.auth.token.id),
            wiki: Some(input["hasWikiEnabled"].as_bool().unwrap_or(false)),
            is_template: input["template"].as_bool(),
            website: website.as_ref().map(|value| value.as_deref()),
            ..Default::default()
        }).await.map_err(|_| partial("Repository created; settings were not confirmed"))?;
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
        records::patch_repository(pool, actor, row.id, &records::RepositoryChanges {
            token: Some(request.auth.token.id),
            name: input["name"].as_str(),
            description: description.map(Some),
            website: website.as_ref().map(|value| value.as_deref()),
            wiki: input["hasWikiEnabled"].as_bool(),
            discussions: input["hasDiscussionsEnabled"].as_bool(),
            is_template: input["template"].as_bool(),
            ..Default::default()
        }).await.map_err(settings)?
    };
    Ok(
        json!({"clientMutationId":input["clientMutationId"],"repository":repository::object(&saved)}),
    )
}
