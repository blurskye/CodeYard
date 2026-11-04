use crate::backend::api::http::context::PageContext;

use crate::backend::app::AppState;

use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
    Form, Router,
};
use codeyard_db_repos::packages::Credential;
use codeyard_db_repos::packages::Listing;
use codeyard_db_repos::packages::Owner;
use codeyard_db_repos::packages::Package;
use codeyard_db_repos::packages::Permission;
use codeyard_db_repos::packages::Refusal;
use codeyard_webui::{app::Page as AppPage, pages::packages as view};
use std::{collections::HashMap, sync::Arc};

pub(in crate::backend::api) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/users/:username/packages", get(show))
        .route("/orgs/:org/packages", get(show))
        .route("/users/:username/packages/:kind/:name", get(show))
        .route("/orgs/:org/packages/:kind/:name", get(show))
        .route(
            "/users/:username/packages/:kind/:name/versions/:version",
            get(show),
        )
        .route(
            "/orgs/:org/packages/:kind/:name/versions/:version",
            get(show),
        )
        .route(
            "/users/:username/packages/:kind/:name/settings",
            get(show).post(change),
        )
        .route(
            "/orgs/:org/packages/:kind/:name/settings",
            get(show).post(change),
        )
        .route("/:owner/:repo/packages", get(show))
        .route("/explore/packages", get(show))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

fn owner_view(owner: &Owner) -> view::Owner {
    let name = codeyard_webui::encode_component(&owner.name);
    view::Owner {
        name: owner.name.clone(),
        profile: codeyard_webui::profile_path(&owner.name),
        packages: format!(
            "/{}/{name}/packages",
            if owner.kind == "org" { "orgs" } else { "users" }
        ),
        organization: owner.kind == "org",
    }
}

fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    let error = crate::backend::api::packages::Error::db(error);
    ctx.error(error.0, error.2)
}

async fn scope(
    ctx: &PageContext,
    params: &HashMap<String, String>,
) -> Result<Option<Owner>, Response> {
    let (name, kind) = if let Some(org) = params.get("org") {
        (org, Some("org"))
    } else if let Some(user) = params.get("username") {
        (user, Some("user"))
    } else if let Some(owner) = params.get("owner") {
        (owner, None)
    } else {
        return Ok(None);
    };
    codeyard_db_repos::packages::owner(&ctx.state.pool, name, kind)
        .await
        .map_err(|error| failure(ctx, error))?
        .map(Some)
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Package owner not found."))
}

async fn record_owner(ctx: &PageContext, package: &Package) -> Result<Owner, Response> {
    let name = if let Some(user) = package.owner_user_id {
        codeyard_db_identity::users::accounts::get_user_by_id(&ctx.state.pool, user)
            .await
            .map_err(|error| failure(ctx, error))?
            .map(|user| user.username)
    } else if let Some(org) = package.owner_org_id {
        codeyard_db_orgs::organizations::get_public_organization_by_id(&ctx.state.pool, org)
            .await
            .map_err(|error| failure(ctx, error))?
            .map(|org| org.name)
    } else {
        None
    }
    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Package owner not found."))?;
    Ok(Owner {
        id: package
            .owner_user_id
            .or(package.owner_org_id)
            .unwrap_or_default(),
        name,
        kind: package.owner_kind.clone(),
    })
}

async fn card(
    ctx: &PageContext,
    owner: &Owner,
    package: &Package,
) -> Result<view::Package, Response> {
    let session = ctx
        .user
        .as_ref()
        .and_then(|_| crate::backend::api::http::session::extract_session_id(&ctx.headers));
    let versions = codeyard_db_repos::packages::version_count(
        &ctx.state.pool,
        package.id,
        session.as_deref().map(Credential::Session),
    )
    .await
    .map_err(|error| failure(ctx, error))?;
    let updated = chrono::DateTime::from_timestamp(package.updated_at, 0)
        .unwrap_or_default()
        .format("%b %e, %Y")
        .to_string();
    Ok(view::Package {
        id: package.id,
        name: package.name.clone(),
        owner: owner.name.clone(),
        kind: package.package_type.clone(),
        visibility: package.visibility.clone(),
        href: crate::backend::webhooks::payloads::package_url(owner, package),
        versions,
        updated,
    })
}

fn version_card(
    base: &str,
    version: &codeyard_db_repos::packages::Version,
    tags: Vec<String>,
) -> view::Version {
    view::Version {
        id: version.id,
        name: version.name.clone(),
        digest: version.digest.clone(),
        tags,
        href: if version.deleted_at.is_some() {
            format!("{base}/settings")
        } else {
            format!("{base}/versions/{}", version.id)
        },
        created: chrono::DateTime::from_timestamp(version.created_at, 0)
            .unwrap_or_default()
            .format("%b %e, %Y")
            .to_string(),
        deleted: version.deleted_at.is_some(),
    }
}

async fn show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    match model(&ctx, &params).await {
        Ok(page) => ctx.page_model("Packages", AppPage::Packages(page)),
        Err(response) => response,
    }
}

async fn model(
    ctx: &PageContext,
    params: &HashMap<String, String>,
) -> Result<view::Page, Response> {
    let owner = scope(ctx, params).await?;
    let session = ctx
        .user
        .as_ref()
        .and_then(|_| crate::backend::api::http::session::extract_session_id(&ctx.headers));
    let credential = session.as_deref().map(Credential::Session);
    let page: i64 = match ctx.query.get("page") {
        Some(page) => page
            .parse()
            .ok()
            .filter(|page| (1..=10000).contains(page))
            .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid package page."))?,
        None => 1,
    };
    if let (Some(owner), Some(name), Some(kind)) =
        (owner.as_ref(), params.get("name"), params.get("kind"))
    {
        if !crate::backend::packages::valid_kind(kind) {
            return Err(ctx.error(StatusCode::NOT_FOUND, "Package type not found."));
        }
        let package = codeyard_db_repos::packages::find(
            &ctx.state.pool,
            owner,
            kind,
            name,
            credential,
            Permission::Read,
            false,
        )
        .await
        .map_err(|error| failure(ctx, error))?;
        let can_admin = match codeyard_db_repos::packages::find(
            &ctx.state.pool,
            owner,
            kind,
            name,
            credential,
            Permission::Admin,
            false,
        )
        .await
        {
            Ok(_) => true,
            Err(error)
                if matches!(
                    error.downcast_ref::<Refusal>(),
                    Some(Refusal::Forbidden | Refusal::Unauthorized | Refusal::Missing)
                ) =>
            {
                false
            }
            Err(error) => return Err(failure(ctx, error)),
        };
        let settings = ctx.path.ends_with("/settings");
        if settings && !can_admin {
            return Err(ctx.error(StatusCode::NOT_FOUND, "Package settings not found."));
        }
        let mut versions = codeyard_db_repos::packages::versions(
            &ctx.state.pool,
            package.id,
            credential,
            false,
            (page - 1) * 30,
            31,
        )
        .await
        .map_err(|error| failure(ctx, error))?;
        let mut more = versions.len() > 30;
        versions.truncate(30);
        // The page lists versions without metadata, so the selected one is read whole.
        let selected = match params.get("version") {
            Some(id) => Some(
                id.parse()
                    .map_err(|_| ctx.error(StatusCode::NOT_FOUND, "Package version not found."))?,
            ),
            None => versions.first().map(|version| version.id),
        };
        let selected = match selected {
            Some(id) => Some(
                codeyard_db_repos::packages::version_by_id(
                    &ctx.state.pool,
                    package.id,
                    credential,
                    id,
                )
                .await
                .map_err(|error| failure(ctx, error))?,
            ),
            None => None,
        };
        if settings {
            let mut deleted = codeyard_db_repos::packages::versions(
                &ctx.state.pool,
                package.id,
                credential,
                true,
                (page - 1) * 30,
                31,
            )
            .await
            .map_err(|error| failure(ctx, error))?;
            more |= deleted.len() > 30;
            deleted.truncate(30);
            versions.append(&mut deleted);
        }
        let card = card(ctx, owner, &package).await?;
        let mut rows = Vec::new();
        for version in versions {
            let tags = codeyard_db_repos::packages::tags(
                &ctx.state.pool,
                package.id,
                credential,
                version.id,
            )
            .await
            .map_err(|error| failure(ctx, error))?;
            rows.push(version_card(&card.href, &version, tags));
        }
        let selected_view = if params.contains_key("version") {
            if let Some(version) = &selected {
                let tags = codeyard_db_repos::packages::tags(
                    &ctx.state.pool,
                    package.id,
                    credential,
                    version.id,
                )
                .await
                .map_err(|error| failure(ctx, error))?;
                Some(version_card(&card.href, version, tags))
            } else {
                None
            }
        } else {
            None
        };
        let origin = codeyard_model::http_config::link_origin();
        let host = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .unwrap_or(&origin);
        let (command, registry) = if package.package_type == "container" {
            let namespace =
                codeyard_db_repos::packages::container_namespace(&ctx.state.pool, owner)
                    .await
                    .map_err(|error| failure(ctx, error))?;
            let reference = selected
                .as_ref()
                .map_or(":latest".into(), |version| format!("@{}", version.digest));
            (
                format!(
                    "docker pull {host}/{}/{}{reference}",
                    namespace, package.name
                ),
                None,
            )
        } else if package.package_type == "npm" {
            let metadata = selected
                .as_ref()
                .map(|version| serde_json::from_str::<serde_json::Value>(&version.manifest))
                .transpose()
                .map_err(|_| {
                    ctx.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Package metadata unavailable.",
                    )
                })?;
            let name = metadata
                .as_ref()
                .and_then(|value| value["name"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("@{}/{}", owner.name, package.name));
            let version = selected
                .as_ref()
                .map_or("latest", |version| version.name.as_str());
            let registry_url = format!(
                "{origin}/api/packages/{}/npm/",
                codeyard_webui::encode_component(&owner.name)
            );
            if name.starts_with('@') {
                (
                    format!("npm install {name}@{version}"),
                    Some(format!("@{}:registry={registry_url}", owner.name)),
                )
            } else {
                (
                    format!("npm install {name}@{version} --registry {registry_url}"),
                    None,
                )
            }
        } else {
            (String::new(), None)
        };
        let repository = match package.repo_id {
            Some(id) => {
                codeyard_db_repos::repositories::records::get_repository_by_id(&ctx.state.pool, id)
                    .await
                    .map_err(|error| failure(ctx, error))?
            }
            None => None,
        };
        let (command, registry) = match package.package_type.as_str() {
            "maven" => {
                let (group, artifact) = package.name.split_once(':').ok_or_else(|| {
                    ctx.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Package metadata unavailable.",
                    )
                })?;
                let version = selected.as_ref().map_or("VERSION", |v| v.name.as_str());
                let endpoint = repository.as_ref().map_or_else(
                    || "Link a repository in Package settings to enable Maven resolution.".into(),
                    |r| {
                        format!(
                            "{origin}/{}/{}",
                            codeyard_webui::encode_component(&owner.name),
                            codeyard_webui::encode_component(&r.name)
                        )
                    },
                );
                (format!("<dependency>\n  <groupId>{group}</groupId>\n  <artifactId>{artifact}</artifactId>\n  <version>{version}</version>\n</dependency>\n\n// Gradle\nimplementation(\"{group}:{artifact}:{version}\")"), Some(endpoint))
            }
            "nuget" => {
                let version = selected.as_ref().map_or("VERSION", |v| v.name.as_str());
                let endpoint = format!(
                    "{origin}/api/packages/{}/nuget/index.json",
                    codeyard_webui::encode_component(&owner.name)
                );
                (
                    format!(
                        "dotnet add package {} --version {version} --source {endpoint}",
                        package.name
                    ),
                    Some(endpoint),
                )
            }
            "rubygems" => {
                let version = selected
                    .as_ref()
                    .map(|v| serde_json::from_str::<serde_json::Value>(&v.manifest))
                    .transpose()
                    .map_err(|_| {
                        ctx.error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "Package metadata unavailable.",
                        )
                    })?;
                let version = version
                    .as_ref()
                    .and_then(|v| v["version"].as_str())
                    .unwrap_or("VERSION");
                let endpoint = format!(
                    "{origin}/api/packages/{}/rubygems/",
                    codeyard_webui::encode_component(&owner.name)
                );
                (format!("source \"{endpoint}\" do\n  gem \"{}\", \"{version}\"\nend\n# bundle install", package.name), Some(endpoint))
            }
            _ => (command, registry),
        };
        let base = if settings {
            format!("{}/settings", card.href)
        } else {
            card.href.clone()
        };
        return Ok(view::Page::Package(Box::new(view::Detail {
            owner: owner_view(owner),
            linked_repository: repository
                .as_ref()
                .map(|repo| repo.name.clone())
                .unwrap_or_default(),
            repository_href: repository.map(|repo| {
                format!(
                    "/{}/{}",
                    codeyard_webui::encode_component(&owner.name),
                    codeyard_webui::encode_component(&repo.name)
                )
            }),
            package: card,
            versions: rows,
            selected: selected_view,
            newer: (page > 1).then(|| format!("{base}?page={}", page - 1)),
            older: more.then(|| format!("{base}?page={}", page + 1)),
            command,
            registry,
            settings,
            can_admin,
            csrf_token: ctx.csrf_token.clone(),
        })));
    }
    let kind = ctx.query.get("type").map(String::as_str).unwrap_or("all");
    if kind != "all" && !crate::backend::packages::valid_kind(kind) {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Invalid package type."));
    }
    let repo = if let (Some(owner), Some(repo)) = (owner.as_ref(), params.get("repo")) {
        Some(
            crate::backend::api::repos::chrome::load_database(ctx, &owner.name, repo)
                .await?
                .chrome,
        )
    } else {
        None
    };
    let explore = owner.is_none();
    let mut packages = codeyard_db_repos::packages::list(
        &ctx.state.pool,
        credential,
        Listing {
            owner: owner.as_ref(),
            repo: repo.as_ref().map(|repo| repo.id),
            kind: (kind != "all").then_some(kind),
            visibility: explore.then_some("public"),
            offset: (page - 1) * 30,
            limit: 31,
        },
    )
    .await
    .map_err(|error| failure(ctx, error))?;
    let more = packages.len() > 30;
    packages.truncate(30);
    let mut cards = Vec::new();
    for package in packages {
        let owner = record_owner(ctx, &package).await?;
        cards.push(card(ctx, &owner, &package).await?);
    }
    let base = ctx.path.clone();
    let container_namespace = match owner.as_ref() {
        Some(owner) => Some(
            codeyard_db_repos::packages::container_namespace(&ctx.state.pool, owner)
                .await
                .map_err(|error| failure(ctx, error))?,
        ),
        None => None,
    };
    Ok(view::Page::List(Box::new(view::Listing {
        container_namespace,
        title: if explore {
            "Explore packages".into()
        } else {
            "Packages".into()
        },
        owner: if repo.is_none() {
            owner.as_ref().map(owner_view)
        } else {
            None
        },
        repo: repo.map(Box::new),
        explore,
        base: base.clone(),
        kind: kind.into(),
        packages: cards,
        newer: (page > 1).then(|| format!("{base}?type={kind}&page={}", page - 1)),
        older: more.then(|| format!("{base}?type={kind}&page={}", page + 1)),
    })))
}

async fn change(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<HashMap<String, String>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.require_user() {
        return response;
    }
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    match mutate(&ctx, &params, &form).await {
        Ok(location) => ctx.redirect(&location),
        Err(response) => response,
    }
}

async fn mutate(
    ctx: &PageContext,
    params: &HashMap<String, String>,
    form: &HashMap<String, String>,
) -> Result<String, Response> {
    let session = crate::backend::api::http::session::extract_session_id(&ctx.headers)
        .ok_or_else(|| ctx.error(StatusCode::UNAUTHORIZED, "Sign in to change packages."))?;
    let credential = Credential::Session(&session);
    let owner = scope(ctx, params)
        .await?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Package owner not found."))?;
    let name = params
        .get("name")
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Package not found."))?;
    let kind = params
        .get("kind")
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Package type not found."))?;
    let package = codeyard_db_repos::packages::find(
        &ctx.state.pool,
        &owner,
        kind,
        name,
        Some(credential),
        Permission::Admin,
        false,
    )
    .await
    .map_err(|error| failure(ctx, error))?;
    if form.get("package_id").and_then(|id| id.parse::<i64>().ok()) != Some(package.id) {
        return Err(ctx.error(
            StatusCode::CONFLICT,
            "Package changed. Reload its settings.",
        ));
    }
    match form.get("action").map(String::as_str) {
        Some("settings") => {
            let visibility = form
                .get("visibility")
                .filter(|v| matches!(v.as_str(), "public" | "private"))
                .ok_or_else(|| {
                    ctx.error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Choose public or private visibility.",
                    )
                })?;
            let repo = match form
                .get("repository")
                .map(|name| name.trim())
                .filter(|name| !name.is_empty())
            {
                Some(name) => Some(
                    codeyard_db_repos::repositories::records::get_repository(
                        &ctx.state.pool,
                        &owner.name,
                        name,
                    )
                    .await
                    .map_err(|error| failure(ctx, error))?
                    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?
                    .id,
                ),
                None => None,
            };
            codeyard_db_repos::packages::settings(
                &ctx.state.pool,
                package.id,
                credential,
                visibility,
                repo,
            )
            .await
            .map_err(|error| failure(ctx, error))?;
        }
        Some("delete") => {
            if form.get("confirm") != Some(&package.name) {
                return Err(ctx.error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Enter the package name to confirm deletion.",
                ));
            }
            codeyard_db_repos::packages::set_deleted(
                &ctx.state.pool,
                package.id,
                None,
                credential,
                false,
            )
            .await
            .map_err(|error| failure(ctx, error))?;
            return Ok(owner_view(&owner).packages);
        }
        Some(action @ ("delete-version" | "restore-version")) => {
            let id = form
                .get("version_id")
                .and_then(|id| id.parse().ok())
                .ok_or_else(|| {
                    ctx.error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Choose a package version.",
                    )
                })?;
            codeyard_db_repos::packages::set_deleted(
                &ctx.state.pool,
                package.id,
                Some(id),
                credential,
                action == "restore-version",
            )
            .await
            .map_err(|error| failure(ctx, error))?;
        }
        _ => return Err(ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Unknown package action.")),
    }
    Ok(ctx.path.clone())
}
