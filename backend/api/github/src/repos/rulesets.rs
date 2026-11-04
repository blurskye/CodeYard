//! Organization and repository rule documents share validation, storage and history.
use crate::orgs::outcome;
use crate::orgs::target;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::rulesets::records::ListFilter;
use codeyard_db_core::queries::rulesets::records::Owner;
use codeyard_db_core::queries::rulesets::records::Page;
use codeyard_db_core::queries::rulesets::records::Ruleset;
use codeyard_db_core::queries::rulesets::records::Version;
use codeyard_db_core::queries::rulesets::Definition;
use codeyard_db_core::queries::rulesets::Invalid;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::api_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/get-org-rulesets", get(list)),
        ("repos/create-org-ruleset", post(create)),
        ("repos/get-org-ruleset", get(get_one)),
        ("repos/update-org-ruleset", put(update)),
        ("repos/delete-org-ruleset", delete(remove)),
        ("orgs/get-org-ruleset-history", get(history)),
        ("orgs/get-org-ruleset-version", get(version)),
        ("repos/get-repo-rulesets", get(list)),
        ("repos/create-repo-ruleset", post(create)),
        ("repos/get-repo-ruleset", get(get_one)),
        ("repos/update-repo-ruleset", put(update)),
        ("repos/delete-repo-ruleset", delete(remove)),
        ("repos/get-repo-ruleset-history", get(history)),
        ("repos/get-repo-ruleset-version", get(version)),
    ]
}
pub(crate) fn error(error: anyhow::Error) -> Error {
    if let Some(Invalid { field, message }) = error.downcast_ref::<Invalid>() {
        Error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Validation Failed".into(),
            Some(json!([
                {"resource":"RepositoryRuleset","field":field,"code":"invalid","message":message}
            ])),
        )
    } else {
        Error::unavailable(error)
    }
}
pub(crate) fn page(input: &Pagination) -> Result<Page> {
    let (offset, limit) = input.window()?;
    Page::new(offset as i64, (limit + 1) as i64).map_err(|e| error(e.into()))
}
struct Scope {
    owner: Owner,
    actor: Option<i64>,
    source: String,
    path: String,
}
impl Scope {
    async fn resolve(
        state: &AppState,
        headers: &HeaderMap,
        params: &HashMap<String, String>,
        write: bool,
    ) -> Result<Self> {
        if let Some(name) = params.get("org") {
            let actor = codeyard_app::api_shared::github::auth::authenticate(state, headers, true)
                .await?
                .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
            if write {
                actor.require_write()?;
            }
            let org = target(state, name).await?;
            Ok(Self {
                owner: Owner::Organization(org.id),
                actor: Some(actor.user.id),
                path: format!("/orgs/{}", codeyard_webui::encode_component(&org.name)),
                source: org.name,
            })
        } else {
            let owner = &params["owner"];
            let name = &params["repo"];
            let (repo, actor) = repository(state, headers, owner, name, write).await?;
            Ok(Self {
                owner: Owner::Repository(repo.id),
                actor: actor.map(|a| a.user.id),
                source: format!("{owner}/{}", repo.name),
                path: format!(
                    "/repos/{}/{}",
                    codeyard_webui::encode_component(owner),
                    codeyard_webui::encode_component(&repo.name)
                ),
            })
        }
    }
    fn actor(&self) -> Result<i64> {
        self.actor
            .ok_or_else(codeyard_app::api_shared::github::auth::required)
    }
    async fn source_for(&self, state: &AppState, owner: Owner) -> Result<(String, String)> {
        if owner == self.owner {
            return Ok((self.source.clone(), self.path.clone()));
        }
        let Owner::Organization(org) = owner else {
            return Err(Error::missing());
        };
        let org = codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(
            &state.pool,
            org,
        )
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        Ok((
            org.name.clone(),
            format!("/orgs/{}", codeyard_webui::encode_component(&org.name)),
        ))
    }
    async fn document(&self, state: &AppState, rule: &Ruleset, editable: bool) -> Result<Value> {
        let (source, path) = self.source_for(state, rule.owner).await?;
        let mut value =
            definition_document(&source, rule.owner, rule.id, &rule.definition, editable)?;
        value["created_at"] = json!(rule.created_at);
        value["updated_at"] = json!(rule.updated_at);
        value["_links"] = json!({"self":{"href":api_url(&format!("{path}/rulesets/{}",rule.id))}});
        Ok(value)
    }
}
fn definition_document(
    source: &str,
    owner: Owner,
    id: i64,
    definition: &Definition,
    editable: bool,
) -> Result<Value> {
    let mut value = serde_json::to_value(definition).map_err(Error::unavailable)?;
    value["id"] = json!(id);
    value["source"] = json!(source);
    value["source_type"] = json!(match owner {
        Owner::Organization(_) => "Organization",
        Owner::Repository(_) => "Repository",
    });
    if !editable {
        value.as_object_mut().unwrap().remove("bypass_actors");
    }
    Ok(value)
}
fn version_document(version: &Version) -> Value {
    let mut actor = json!({"type":version.actor_type});
    if let Some(id) = version.actor_id {
        actor["id"] = json!(id);
    }
    json!({"version_id":version.id,"actor":actor,"updated_at":version.created_at})
}
#[derive(Deserialize)]
struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    targets: Option<String>,
    includes_parents: Option<bool>,
}
impl List {
    fn filter(&self) -> Result<ListFilter> {
        let mut targets = Vec::new();
        for target in self.targets.iter().flat_map(|s| s.split(',')) {
            match target {
                "branch" => targets.push(codeyard_db_core::queries::rulesets::Target::Branch),
                "tag" => targets.push(codeyard_db_core::queries::rulesets::Target::Tag),
                "push" => targets.push(codeyard_db_core::queries::rulesets::Target::Push),
                // The documented repository target has no native policy type yet.
                "repository" => {}
                _ => return Err(Error::validation("RepositoryRuleset", "targets", "invalid")),
            }
        }
        Ok(ListFilter {
            includes_parents: self.includes_parents.unwrap_or(true),
            targets: self.targets.as_ref().map(|_| targets),
        })
    }
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let scope = Scope::resolve(&state, &headers, &params, false).await?;
    let filter = query.filter()?;
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let rows = outcome(
        codeyard_db_repos::rulesets::operations::list_rulesets(
            &state.pool,
            scope.actor,
            scope.owner,
            &filter,
            page(&pagination)?,
        )
        .await
        .map_err(error)?,
    )?;
    let (_, limit) = pagination.window()?;
    let has_next = rows.len() > limit;
    let mut values = Vec::new();
    for row in rows {
        values.push(scope.document(&state, &row.value, row.can_edit).await?);
    }
    pagination.response(&uri, values, has_next)
}
#[derive(Deserialize)]
struct Detail {
    includes_parents: Option<bool>,
}
fn id(params: &HashMap<String, String>, key: &str) -> Result<i64> {
    params
        .get(key)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::invalid("Invalid ruleset identifier"))
}
async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<Detail>,
) -> Result<Json<Value>> {
    let scope = Scope::resolve(&state, &headers, &params, false).await?;
    let row = outcome(
        codeyard_db_repos::rulesets::operations::get_ruleset(
            &state.pool,
            scope.actor,
            scope.owner,
            id(&params, "ruleset_id")?,
            query.includes_parents.unwrap_or(true),
        )
        .await
        .map_err(error)?,
    )?;
    Ok(Json(
        scope.document(&state, &row.value, row.can_edit).await?,
    ))
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>)> {
    let scope = Scope::resolve(&state, &headers, &params, true).await?;
    let Json(input) = input.map_err(codeyard_model::github_error::json_error)?;
    let input = Definition::parse(&input).map_err(|e| error(e.into()))?;
    let row = outcome(
        codeyard_db_repos::rulesets::operations::create_ruleset(
            &state.pool,
            scope.actor()?,
            scope.owner,
            &input,
        )
        .await
        .map_err(error)?,
    )?;
    Ok((
        StatusCode::CREATED,
        Json(scope.document(&state, &row, true).await?),
    ))
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>> {
    let scope = Scope::resolve(&state, &headers, &params, true).await?;
    let input = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(codeyard_model::github_error::json_parse_error)?
    };
    let row = outcome(
        codeyard_db_repos::rulesets::operations::change_ruleset(
            &state.pool,
            scope.actor()?,
            scope.owner,
            id(&params, "ruleset_id")?,
            &input,
        )
        .await
        .map_err(error)?,
    )?;
    Ok(Json(scope.document(&state, &row, true).await?))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<StatusCode> {
    let scope = Scope::resolve(&state, &headers, &params, true).await?;
    outcome(
        codeyard_db_repos::rulesets::operations::remove_ruleset(
            &state.pool,
            scope.actor()?,
            scope.owner,
            id(&params, "ruleset_id")?,
        )
        .await
        .map_err(error)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
async fn history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(pagination): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let scope = Scope::resolve(&state, &headers, &params, false).await?;
    let rows = outcome(
        codeyard_db_repos::rulesets::operations::ruleset_versions(
            &state.pool,
            scope.actor,
            scope.owner,
            id(&params, "ruleset_id")?,
            page(&pagination)?,
        )
        .await
        .map_err(error)?,
    )?;
    let (_, limit) = pagination.window()?;
    let has_next = rows.value.len() > limit;
    pagination.response(
        &uri,
        rows.value.iter().map(version_document).collect(),
        has_next,
    )
}
async fn version(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let scope = Scope::resolve(&state, &headers, &params, false).await?;
    let ruleset = id(&params, "ruleset_id")?;
    let row = outcome(
        codeyard_db_repos::rulesets::operations::ruleset_version(
            &state.pool,
            scope.actor,
            scope.owner,
            ruleset,
            id(&params, "version_id")?,
        )
        .await
        .map_err(error)?,
    )?;
    let mut value = version_document(&row.value);
    value["state"] = definition_document(
        &scope.source,
        scope.owner,
        ruleset,
        &row.value.definition,
        row.can_edit,
    )?;
    Ok(Json(value))
}
