//! Small discovery endpoints reuse the same templates and Markdown renderer as repository pages.
use codeyard_model::urls::api_url;

use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;
use crate::backend::repositories::store::repo_store;

use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) async fn root(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    crate::backend::api::github::auth::authenticate(&state, &headers, false).await?;
    let mut value = json!({});
    for (key, path) in [
        ("current_user_url", "/user"),
        ("current_user_authorizations_html_url", "/settings/tokens"),
        ("authorizations_url", "/authorizations"),
        (
            "code_search_url",
            "/search/code?q={query}{&page,per_page,sort,order}",
        ),
        (
            "commit_search_url",
            "/search/commits?q={query}{&page,per_page,sort,order}",
        ),
        ("emails_url", "/user/emails"),
        ("emojis_url", "/emojis"),
        ("events_url", "/events"),
        ("feeds_url", "/feeds"),
        ("followers_url", "/user/followers"),
        ("following_url", "/user/following{/target}"),
        ("gists_url", "/gists{/gist_id}"),
        ("hub_url", "/hub"),
        (
            "topic_search_url",
            "/search/topics?q={query}{&page,per_page}",
        ),
        (
            "issue_search_url",
            "/search/issues?q={query}{&page,per_page,sort,order}",
        ),
        ("issues_url", "/issues"),
        ("keys_url", "/user/keys"),
        (
            "label_search_url",
            "/search/labels?q={query}&repository_id={repository_id}{&page,per_page}",
        ),
        ("notifications_url", "/notifications"),
        ("organization_url", "/orgs/{org}"),
        (
            "organization_repositories_url",
            "/orgs/{org}/repos{?type,page,per_page,sort}",
        ),
        ("organization_teams_url", "/orgs/{org}/teams"),
        ("public_gists_url", "/gists/public"),
        ("rate_limit_url", "/rate_limit"),
        ("repository_url", "/repos/{owner}/{repo}"),
        (
            "repository_search_url",
            "/search/repositories?q={query}{&page,per_page,sort,order}",
        ),
        (
            "current_user_repositories_url",
            "/user/repos{?type,page,per_page,sort}",
        ),
        ("starred_url", "/user/starred{/owner}{/repo}"),
        ("starred_gists_url", "/gists/starred"),
        ("user_url", "/users/{user}"),
        ("user_organizations_url", "/user/orgs"),
        (
            "user_repositories_url",
            "/users/{user}/repos{?type,page,per_page,sort}",
        ),
        (
            "user_search_url",
            "/search/users?q={query}{&page,per_page,sort,order}",
        ),
    ] {
        value[key] = json!(api_url(path));
    }
    value["current_user_authorizations_html_url"] = json!(format!(
        "{}/settings/applications{{/client_id}}",
        codeyard_model::http_config::link_origin()
    ));
    Ok(Json(value))
}

async fn meta(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Value>> {
    crate::backend::api::github::auth::authenticate(&state, &headers, false).await?;
    Ok(Json(
        json!({"verifiable_password_authentication":false,"installed_version":env!("CARGO_PKG_VERSION")}),
    ))
}
async fn versions() -> Json<Value> {
    Json(json!(["2022-11-28"]))
}
async fn zen() -> &'static str {
    "Keep it logically awesome."
}
async fn octocat(Query(query): Query<HashMap<String, String>>) -> String {
    let saying = query
        .get("s")
        .map(String::as_str)
        .unwrap_or("Hello from Codeyard");
    let saying: String = saying
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect();
    format!("{saying}\n  /\\_/\\\n ( o.o )\n  > ^ <\n")
}

pub(super) fn license_document(
    template: &crate::backend::repositories::templates::Template,
    full: bool,
) -> Value {
    let key = template.name.to_ascii_lowercase();
    let url = api_url(&format!("/licenses/{key}"));
    let mut value = json!({"key":key,"name":template.name,"spdx_id":template.name,"url":url,"node_id":node_id::git("LIC",&key)});
    if full {
        let (description, conditions, limitations) = match template.name {
            "MIT" => (
                "A short permissive license with attribution and no warranty.",
                vec!["include-copyright"],
                vec!["liability", "warranty"],
            ),
            "Apache-2.0" => (
                "A permissive license with an express patent grant.",
                vec!["include-copyright", "document-changes"],
                vec!["liability", "trademark-use", "warranty"],
            ),
            "BSD-3-Clause" => (
                "A permissive license that prohibits using contributors' names for endorsement.",
                vec!["include-copyright"],
                vec!["liability", "warranty"],
            ),
            "GPL-3.0" => (
                "A copyleft license requiring source availability for distributed works.",
                vec![
                    "include-copyright",
                    "document-changes",
                    "disclose-source",
                    "same-license",
                ],
                vec!["liability", "warranty"],
            ),
            "MPL-2.0" => (
                "A file-level copyleft license requiring source availability for modified files.",
                vec!["include-copyright", "disclose-source", "same-license--file"],
                vec!["liability", "trademark-use", "warranty"],
            ),
            _ => (
                "A public-domain dedication with no warranty.",
                vec![],
                vec!["liability", "warranty"],
            ),
        };
        let mut permissions = vec![
            "commercial-use",
            "distribution",
            "modifications",
            "private-use",
        ];
        if matches!(template.name, "Apache-2.0" | "GPL-3.0" | "MPL-2.0") {
            permissions.push("patent-use");
        }
        value["html_url"] = json!(url);
        value["description"] = json!(description);
        value["implementation"] = json!("Copy this text to LICENSE in the repository root and fill in the copyright placeholders where present.");
        value["permissions"] = json!(permissions);
        value["conditions"] = json!(conditions);
        value["limitations"] = json!(limitations);
        value["body"] = json!(template.text);
        value["featured"] = json!(true);
    }
    value
}

async fn licenses(
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let rows = crate::backend::repositories::templates::LICENSE_TEMPLATES
        .iter()
        .skip(offset)
        .take(limit)
        .map(|template| license_document(template, false))
        .collect();
    page.response(
        &uri,
        rows,
        crate::backend::repositories::templates::LICENSE_TEMPLATES.len() > offset + limit,
    )
}
async fn license(Path(key): Path<String>) -> Result<Json<Value>> {
    let template = crate::backend::repositories::templates::LICENSE_TEMPLATES
        .iter()
        .find(|template| template.name.eq_ignore_ascii_case(&key))
        .ok_or_else(Error::missing)?;
    Ok(Json(license_document(template, true)))
}

async fn repository_license(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let reference = query.get("ref").unwrap_or(&repo.default_branch);
    let store = repo_store(&repo, state.cluster.as_ref());
    let revision = store
        .commit(reference)
        .await
        .map_err(crate::backend::api::github::contents::store_error)?
        .sha;
    let mut entries = store
        .tree_metadata(&revision, "")
        .await
        .map_err(crate::backend::api::github::contents::store_error)?;
    entries.sort_by_key(|entry| entry.name.to_ascii_lowercase());
    let path = entries
        .iter()
        .find(|entry| {
            entry.entry_type == "file"
                && crate::backend::repositories::templates::is_license_file(&entry.name)
        })
        .ok_or_else(Error::missing)?
        .path
        .clone();
    let blob = store
        .blob_bytes(&revision, &path)
        .await
        .map_err(crate::backend::api::github::contents::store_error)?;
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/vnd.github.raw"))
    {
        return crate::backend::api::github::contents::blob_response(
            &headers, &owner, &name, reference, &revision, &path, blob,
        )
        .await;
    }
    let mut value = crate::backend::api::github::contents::entry(
        &owner, &name, reference, &revision, &path, &blob.id, blob.size, "file",
    );
    value["encoding"] = json!("base64");
    value["content"] = json!(STANDARD.encode(&blob.bytes));
    value["license"] = crate::backend::repositories::templates::detect_license(&blob.bytes)
        .map(|template| license_document(template, false))
        .unwrap_or(Value::Null);
    Ok(Json(value).into_response())
}

fn conduct(full: bool) -> Value {
    let mut value = json!({"key":"contributor_covenant","name":"Contributor Covenant","url":api_url("/codes_of_conduct/contributor_covenant"),"html_url":api_url("/codes_of_conduct/contributor_covenant")});
    if full {
        value["body"] = json!(include_str!(
            "../../repositories/templates/conduct/contributor_covenant.txt"
        ));
    }
    value
}
async fn conducts() -> Json<Value> {
    Json(json!([conduct(false)]))
}
async fn conduct_code(Path(key): Path<String>) -> Result<Json<Value>> {
    if key != "contributor_covenant" {
        return Err(Error::missing());
    }
    Ok(Json(conduct(true)))
}
async fn ignores() -> Json<Value> {
    Json(json!(
        crate::backend::repositories::templates::GITIGNORE_TEMPLATES
            .iter()
            .map(|template| template.name)
            .collect::<Vec<_>>()
    ))
}
async fn ignore(Path(name): Path<String>, headers: HeaderMap) -> Result<Response> {
    let template =
        crate::backend::repositories::templates::gitignore(&name).ok_or_else(Error::missing)?;
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/vnd.github.raw"))
    {
        return Ok(template.text.into_response());
    }
    Ok(Json(json!({"name":template.name,"source":template.text})).into_response())
}

#[derive(Deserialize)]
struct Markdown {
    text: String,
    mode: Option<String>,
    context: Option<String>,
}
async fn markdown(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Markdown>,
) -> Result<Response> {
    if input.text.len() > 400 * 1024
        || input
            .mode
            .as_deref()
            .is_some_and(|mode| !matches!(mode, "markdown" | "gfm"))
    {
        return Err(Error::validation("Markdown", "text", "invalid"));
    }
    let mut references = codeyard_webui::components::references::References::default();
    if let Some(context) = input.context {
        let (owner, name) = context
            .split_once('/')
            .filter(|(_, name)| !name.contains('/'))
            .ok_or_else(|| Error::validation("Markdown", "context", "invalid"))?;
        let (repo, authentication) = repository(&state, &headers, owner, name, false).await?;
        if let Some(authentication) = authentication.as_ref() {
            authentication
                .repository_scope(&state.pool, repo.id, "contents", false)
                .await
                .map_err(crate::backend::api::github::auth::scope_error)?;
        }
        if input.mode.as_deref() == Some("gfm") {
            references = resolve_references(
                &state,
                authentication.as_ref(),
                &repo,
                owner,
                name,
                &[&input.text],
                false,
            )
            .await?;
        }
    }
    render(input.text, references).await
}
pub(super) async fn resolve_references(
    state: &AppState,
    authentication: Option<&crate::backend::api::github::auth::Authentication>,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    name: &str,
    texts: &[&str],
    document: bool,
) -> Result<codeyard_webui::components::references::References> {
    crate::backend::api::repos::refs::references::resolve_references_with(
        &state.pool,
        state.cluster.as_ref(),
        repo,
        owner,
        name,
        texts,
        document,
        |records| async move {
            let ids: Vec<_> = records.iter().map(|repo| repo.id).collect();
            crate::backend::api::github::auth::readable_repositories(
                state,
                authentication,
                &ids,
                "contents",
            )
            .await
            .map_err(|error| anyhow::anyhow!("Reference audience could not be verified: {error:?}"))
        },
    )
    .await
    .map_err(Error::unavailable)
}

async fn raw_markdown(body: String) -> Result<Response> {
    if body.len() > 400 * 1024 {
        return Err(Error::validation("Markdown", "text", "invalid"));
    }
    render(
        body,
        codeyard_webui::components::references::References::default(),
    )
    .await
}
pub(super) async fn render(
    text: String,
    references: codeyard_webui::components::references::References,
) -> Result<Response> {
    let html = tokio::task::spawn_blocking(move || {
        codeyard_webui::components::parse_markdown(&text, &references)
    })
    .await
    .map_err(Error::unavailable)?;
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response())
}

async fn emojis() -> Json<Value> {
    Json(
        codeyard_webui::components::EMOJI
            .iter()
            .map(|(name, _)| (name.to_string(), json!(api_url(&format!("/emojis/{name}")))))
            .collect(),
    )
}
async fn emoji(Path(name): Path<String>) -> Result<Response> {
    let (_, emoji) = codeyard_webui::components::EMOJI
        .iter()
        .find(|(key, _)| *key == name)
        .ok_or_else(Error::missing)?;
    Ok(([(header::CONTENT_TYPE,"image/svg+xml")],format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\"><text x=\"2\" y=\"52\" font-size=\"52\">{emoji}</text></svg>")).into_response())
}
pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new().route("/emojis/:name", get(emoji))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("meta/root", get(root)),
        ("meta/get", get(meta)),
        ("meta/get-all-versions", get(versions)),
        ("meta/get-zen", get(zen)),
        ("meta/get-octocat", get(octocat)),
        ("licenses/get-all-commonly-used", get(licenses)),
        ("licenses/get", get(license)),
        ("licenses/get-for-repo", get(repository_license)),
        ("codes-of-conduct/get-all-codes-of-conduct", get(conducts)),
        ("codes-of-conduct/get-conduct-code", get(conduct_code)),
        ("gitignore/get-all-templates", get(ignores)),
        ("gitignore/get-template", get(ignore)),
        ("markdown/render", post(markdown)),
        ("markdown/render-raw", post(raw_markdown)),
        ("emojis/get", get(emojis)),
    ]
}
