//! Repository creation and edits use native provisioning and settings transactions.
use crate::missing;
use crate::mutations::target;
use crate::mutations::text;
use crate::token_denied;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use async_graphql::Error;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::creation::RepositoryTarget;
use codeyard_db_repos::repositories::records;
use codeyard_db_repos::repositories::topics;
use codeyard_model::node_id;
use serde_json::json;
use serde_json::Value;

pub(super) const FIELDS: &[&str] = &[
    "createRepository",
    "cloneTemplateRepository",
    "updateRepository",
];
fn settings(error: anyhow::Error) -> Error {
    codeyard_api_github::repos::settings_error(error).graphql()
}
fn provision(error: codeyard_repos_core::repositories::lifecycle::StorageError) -> Error {
    codeyard_app::api_shared::github::repos::lifecycle_error(error).graphql()
}
fn partial(message: &str) -> Error {
    use async_graphql::ErrorExtensions;
    Error::new(message).extend_with(|_, e| e.set("type", "PARTIAL_UPDATE"))
}
fn capabilities(input: &Value) -> Result<()> {
    for (name, enabled) in [
        ("hasPullRequestsEnabled", true),
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
        codeyard_repos_core::repositories::lifecycle::validate_description(value)
            .map_err(provision)?;
    }
    if let Some(name) = input["name"].as_str() {
        codeyard_repos_core::repositories::lifecycle::validate_name(name).map_err(provision)?;
    }
    let saved = if matches!(field, "createRepository" | "cloneTemplateRepository") {
        if request.auth.is_job() {
            return Err(token_denied());
        }
        // Checked before provisioning, which commits the repository.
        request.require_oauth_scope("repo")?;
        let (private, internal) = match text(input, "visibility")? {
            "PUBLIC" => (false, false),
            "PRIVATE" => (true, false),
            "INTERNAL" => (true, true),
            _ => return Err(Error::new("visibility must be PUBLIC, PRIVATE or INTERNAL")),
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
        if internal && matches!(owner, RepositoryTarget::Personal(_)) {
            return Err(settings(
                records::RepositorySettingsRefusal::InternalNeedsOrganization.into(),
            ));
        }
        if request.personal.is_some() {
            let owner = match owner {
                RepositoryTarget::Personal(id) => codeyard_identity::fine_grained::Owner::User(id),
                RepositoryTarget::Organization(id) => {
                    codeyard_identity::fine_grained::Owner::Organization(id)
                }
            };
            let grant = codeyard_identity::fine_grained::grant(pool, request.auth.token.id)
                .await
                .map_err(unavailable)?
                .ok_or_else(token_denied)?;
            if !grant.creates_repository(owner) {
                return Err(token_denied());
            }
        }
        if let RepositoryTarget::Organization(org) = owner {
            if codeyard_db_identity::fine_grained_tokens::classic_refused(
                pool,
                request.auth.token.id,
                org,
            )
            .await
            .map_err(unavailable)?
            {
                return Err(token_denied());
            }
        }
        let team = if let Some(id) = input["teamId"].as_str() {
            let (org, slug) = codeyard_db_repos::graphql::team_locator(
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
            let team =
                codeyard_db_orgs::organizations::teams::get_public_team(pool, actor, org, &slug)
                    .await
                    .map_err(unavailable)?
                    .filter(|v| v.manage)
                    .ok_or_else(missing)?;
            Some(team.team)
        } else {
            None
        };
        let authority = request.auth.database_actor();
        let creation = RepositoryCreationOwner {
            actor_id: actor,
            target: owner,
        };
        let row = if field == "cloneTemplateRepository" {
            let source =
                crate::repository::by_id(target(input, "repositoryId", "R")?, request).await?;
            request.authorize(source.id, "contents", false).await?;
            codeyard_repos_core::repositories::lifecycle::generate(
                (pool, request.state.cluster.as_ref()),
                &request.state.repos_path,
                creation,
                source.id,
                codeyard_repos_core::repositories::lifecycle::TemplateOptions {
                    name: text(input, "name")?,
                    description,
                    private,
                    all_branches: input["includeAllBranches"].as_bool().unwrap_or(false),
                    properties: &[],
                    authority: Some(&authority),
                },
            )
            .await
        } else {
            codeyard_repos_core::repositories::lifecycle::provision_with_properties(
                (pool, request.state.cluster.as_ref()),
                &request.state.repos_path,
                codeyard_repos_core::repositories::lifecycle::CreationInput {
                    owner: creation,
                    properties: &[],
                    admission: codeyard_db_repos::repositories::creation::CreationAdmission {
                        authority: Some(&authority),
                        advisory: None,
                    },
                },
                text(input, "name")?,
                description,
                private,
                None,
            )
            .await
        }
        .map_err(provision)?;
        records::patch_repository(
            pool,
            actor,
            row.id,
            &records::RepositoryChanges {
                token: Some(request.auth.token.id),
                wiki: Some(input["hasWikiEnabled"].as_bool().unwrap_or(false)),
                is_template: input["template"].as_bool(),
                website: website.as_ref().map(|value| value.as_deref()),
                internal: internal.then_some(true),
                features: records::FeatureUpdate {
                    has_issues: input["hasIssuesEnabled"].as_bool(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .map_err(|_| partial("Repository created; settings were not confirmed"))?;
        if let Some(team) = team {
            use codeyard_db_core::queries::organizations::OrganizationOutcome;
            let granted =
                codeyard_db_orgs::organizations::teams::grant_organization_team_repository(
                    pool,
                    actor,
                    team.org_id,
                    team.id,
                    row.id,
                    codeyard_db_orgs::organizations::teams::TeamRepositoryPermission::new(3)
                        .map_err(unavailable)?,
                )
                .await
                .map_err(|_| partial("Repository created; team grant was not confirmed"))?;
            if !matches!(granted, OrganizationOutcome::Applied(_)) {
                return Err(partial("Repository created; team grant was not confirmed"));
            }
        }
        crate::repository::by_id(row.id, request).await?
    } else {
        let row = crate::repository::by_id(target(input, "repositoryId", "R")?, request).await?;
        request.authorize(row.id, "administration", true).await?;
        records::patch_repository(
            pool,
            actor,
            row.id,
            &records::RepositoryChanges {
                token: Some(request.auth.token.id),
                name: input["name"].as_str(),
                description: description.map(Some),
                website: website.as_ref().map(|value| value.as_deref()),
                wiki: input["hasWikiEnabled"].as_bool(),
                discussions: input["hasDiscussionsEnabled"].as_bool(),
                is_template: input["template"].as_bool(),
                features: records::FeatureUpdate {
                    has_issues: input["hasIssuesEnabled"].as_bool(),
                    has_projects: input["hasProjectsEnabled"].as_bool(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .map_err(settings)?
    };
    Ok(
        json!({"clientMutationId":input["clientMutationId"],"repository":crate::repository::object(&saved)}),
    )
}
