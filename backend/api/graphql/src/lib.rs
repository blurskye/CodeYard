#![recursion_limit = "256"]
#![allow(
    clippy::result_large_err,
    reason = "Page handlers return the original response error by value."
)]
//! GitHub's compile-time GraphQL schema and domain-backed resolver families.
use crate::arguments::Arguments;
use async_graphql::Error;
use async_graphql::ErrorExtensions;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use axum::Json;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_app::app::AppState;
use codeyard_model::graphql_error::error;
use serde_json::json;
use serde_json::Value;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
mod arguments;
mod bindings;
mod checks;
mod collaboration;
mod collaboration_mutations;
mod comment_mutations;
mod commit_comment;
mod connection;
mod discussions;
mod gists;
mod git;
mod git_objects;
mod issue;
mod metadata_mutations;
mod milestone;
mod mutations;
mod node_objects;
mod projects;
mod pull_request;
mod reaction_mutations;
mod release;
mod repository;
mod repository_list;
mod repository_metadata;
mod repository_mutations;
mod request;
mod review_mutations;
mod review_requests;
mod rulesets;
mod schema;
mod team;
mod user;

type Result<T> = async_graphql::Result<T>;

struct RequestContext {
    state: Arc<AppState>,
    auth: codeyard_identity::github_auth::Authentication,
    actor: codeyard_db_core::queries::credentials::Actor,
    /// Scopes granted to an OAuth token; other credentials have none.
    oauth_scopes: Option<String>,
    /// Admission distinguishes personal tokens from integrations, which GraphQL does not accept.
    personal: Option<codeyard_identity::fine_grained::Grant>,
    nodes: AtomicUsize,
    /// Bytes of text the request has loaded or returned, bounded by `MAX_RESPONSE_BYTES`.
    bytes: AtomicUsize,
    diffs: tokio::sync::Mutex<
        std::collections::HashMap<(i64, String, String), Arc<codeyard_git::DiffInfo>>,
    >,
    mergeability: tokio::sync::Mutex<std::collections::HashMap<(i64, String, String), bool>>,
    rate: codeyard_db_repos::graphql::RateLimit,
}

/// Field counts do not bound memory: one large text can be loaded under many aliases.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

impl RequestContext {
    /// Counts `bytes` of text toward the request's limit.
    fn charge(&self, bytes: usize) -> Result<()> {
        let total = self
            .bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(bytes);
        if total > MAX_RESPONSE_BYTES {
            return Err(async_graphql::Error::new(
                "Query response size limit exceeded",
            ));
        }
        Ok(())
    }

    fn charge_value(&self, value: &Value) -> Result<()> {
        match value {
            Value::String(text) => self.charge(text.len()),
            Value::Array(values) => {
                self.charge(values.len().saturating_mul(std::mem::size_of::<Value>()))?;
                for value in values {
                    self.charge_value(value)?;
                }
                Ok(())
            }
            Value::Object(values) => {
                for (key, value) in values {
                    self.charge(key.len().saturating_add(64))?;
                    self.charge_value(value)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn authorize(&self, repo: i64, scope: &str, write: bool) -> Result<()> {
        let pool = &self.state.pool;
        let live = codeyard_db_identity::users::tokens::token_user(pool, self.auth.token.id, write)
            .await
            .map_err(unavailable)?;
        if live.is_none_or(|user| user.id != self.auth.user.id) {
            return Err(token_denied());
        }
        if self.auth.user.is_stub {
            self.auth
                .repository_scope(pool, repo, scope, write)
                .await
                .map_err(|error| codeyard_api_github::auth::scope_error(error).graphql())?;
        }
        // Integration credentials were excluded at admission. Do not rediscover Apps for
        // every scalar; personal bounds and job permissions still have to be live.
        if self.personal.is_some() {
            let grant = codeyard_identity::fine_grained::grant(pool, self.auth.token.id)
                .await
                .map_err(unavailable)?
                .ok_or_else(token_denied)?;
            let permissions = grant
                .repository(pool, repo)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            if !codeyard_model::github_apps::allows(&permissions, &scope.replace('-', "_"), write) {
                return Err(token_denied());
            }
        } else if self.auth.is_job() {
            self.auth
                .repository_scope(pool, repo, scope, write)
                .await
                .map_err(|error| codeyard_api_github::auth::scope_error(error).graphql())?;
        } else if codeyard_db_identity::fine_grained_tokens::classic_refused_repository(
            pool,
            self.auth.token.id,
            repo,
        )
        .await
        .map_err(unavailable)?
        {
            return Err(token_denied());
        }
        // Check the live ACL even for cached scalars: ordinary PAT scope is not a read permit.
        if codeyard_db_repos::repositories::access::readable_repository_ids(
            pool,
            Some(self.auth.user.id),
            &[repo],
        )
        .await
        .map_err(unavailable)?
        .is_empty()
        {
            return Err(missing());
        }
        if !write
            && self
                .oauth_scopes
                .as_ref()
                .is_some_and(|scopes| !codeyard_model::oauth_apps::has_scope(scopes, "repo"))
        {
            if codeyard_db_repos::repositories::access::readable_repository_ids(pool, None, &[repo])
                .await
                .map_err(unavailable)?
                .is_empty()
            {
                return Err(token_denied());
            }
            return Ok(());
        }
        self.require_oauth_scope("repo")
    }

    /// Each resolver checks the grant it needs, as the REST routes check theirs.
    fn require_oauth_scope(&self, scope: &str) -> Result<()> {
        if let Some(grant) = &self.personal {
            // Repository reads and writes were checked by `authorize`, and cross-repository
            // lists stop at `bounded`.
            return match scope {
                "repo" => Ok(()),
                "workflow"
                    if codeyard_model::github_apps::allows(
                        &Value::Object(
                            grant.group(codeyard_model::fine_grained::Group::Repository),
                        ),
                        "workflows",
                        true,
                    ) =>
                {
                    Ok(())
                }
                _ => Err(token_denied()),
            };
        }
        match &self.oauth_scopes {
            Some(scopes) if !codeyard_model::oauth_apps::has_scope(scopes, scope) => {
                Err(token_denied())
            }
            _ => Ok(()),
        }
    }

    /// Identity may expose unpublished repository data only with an explicit live grant,
    /// never through the anonymous-equivalent public fallback.
    async fn private_viewer(&self, repo: i64, scope: &str) -> Result<Option<i64>> {
        self.authorize(repo, scope, false).await?;
        if self
            .oauth_scopes
            .as_ref()
            .is_some_and(|scopes| !codeyard_model::oauth_apps::has_scope(scopes, "repo"))
        {
            return Ok(None);
        }
        if self.personal.is_some() {
            let grant =
                codeyard_identity::fine_grained::grant(&self.state.pool, self.auth.token.id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(token_denied)?;
            let row = codeyard_db_repos::repositories::records::get_repository_by_id(
                &self.state.pool,
                repo,
            )
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
            if !grant
                .reaches(&self.state.pool, &row)
                .await
                .map_err(unavailable)?
                || !codeyard_model::fine_grained::allows(
                    &grant.group(codeyard_model::fine_grained::Group::Repository),
                    &scope.replace('-', "_"),
                    false,
                )
            {
                return Ok(None);
            }
        }
        Ok(Some(self.auth.user.id))
    }

    async fn account_permission(&self, key: &str, oauth_scope: &str) -> Result<bool> {
        if self.auth.is_job()
            || codeyard_db_identity::users::tokens::token_user(
                &self.state.pool,
                self.auth.token.id,
                false,
            )
            .await
            .map_err(unavailable)?
            .is_none_or(|user| user.id != self.auth.user.id)
        {
            return Ok(false);
        }
        if self.personal.is_some() {
            return Ok(codeyard_identity::fine_grained::grant(
                &self.state.pool,
                self.auth.token.id,
            )
            .await
            .map_err(unavailable)?
            .is_some_and(|grant| grant.account(self.auth.user.id, key, false)));
        }
        Ok(self.require_oauth_scope(oauth_scope).is_ok())
    }

    async fn authorize_organization(&self, org: i64) -> Result<()> {
        self.require_oauth_scope("read:org")?;
        if codeyard_db_identity::users::tokens::token_user(
            &self.state.pool,
            self.auth.token.id,
            false,
        )
        .await
        .map_err(unavailable)?
        .is_none_or(|user| user.id != self.auth.user.id)
            || codeyard_db_identity::fine_grained_tokens::classic_refused(
                &self.state.pool,
                self.auth.token.id,
                org,
            )
            .await
            .map_err(unavailable)?
        {
            return Err(token_denied());
        }
        Ok(())
    }

    fn is_oauth(&self) -> bool {
        self.oauth_scopes.is_some()
    }

    /// Job and fine-grained tokens are bounded to repositories: they reach no account,
    /// organization, team or search fields.
    fn bounded(&self) -> bool {
        self.auth.is_job() || self.personal.is_some()
    }
}

fn token_denied() -> Error {
    Error::new("Resource not accessible by token").extend_with(|_, e| e.set("type", "FORBIDDEN"))
}

fn missing() -> Error {
    Error::new("Could not resolve to a node").extend_with(|_, e| {
        e.set("type", "NOT_FOUND");
    })
}

/// GitHub's answer when the viewer can see the object but may not run this mutation.
fn forbidden(field: &str, request: &RequestContext) -> Error {
    let (first, rest) = field.split_at(field.len().min(1));
    error(
        format!(
            "{} does not have the correct permissions to execute `{}{rest}`",
            request.auth.user.username,
            first.to_ascii_uppercase()
        ),
        "FORBIDDEN",
    )
}

fn error_kind(error: &Error) -> Option<&str> {
    match error.extensions.as_ref()?.get("type")? {
        async_graphql::Value::String(kind) => Some(kind),
        _ => None,
    }
}

fn is_not_found(error: &Error) -> bool {
    error_kind(error) == Some("NOT_FOUND")
}

fn unavailable<E>(_: E) -> Error {
    error("Service unavailable", "INTERNAL")
}

fn url(path: &str) -> String {
    format!("{}{path}", codeyard_model::http_config::link_origin())
}

fn arg(ctx: &Arguments, name: &str) -> Result<String> {
    Ok(ctx.args.try_get(name)?.string()?.to_owned())
}

async fn validate_node_root(id: &str, request: &RequestContext) -> Result<()> {
    if let Some(expected) = codeyard_model::node_id::scoped_root(id) {
        let row = codeyard_model::node_id::routed(id).ok_or_else(missing)?;
        if !codeyard_db_repos::graphql::node_root_matches(
            &request.state.pool,
            row.kind,
            row.id,
            expected,
        )
        .await
        .map_err(unavailable)?
        {
            return Err(missing());
        }
    }
    Ok(())
}

async fn node(id: &str, request: &RequestContext) -> Result<Value> {
    validate_node_root(id, request).await?;
    if id.starts_with("D_") || id.starts_with("DIC_") || id.starts_with("DC_") {
        return crate::discussions::node(id, request).await;
    }
    if id.starts_with("RR_") || id.starts_with("RRT_") {
        return crate::review_requests::node(id, request).await;
    }
    if id.starts_with("GIST_") {
        return crate::gists::node(id, request).await;
    }
    // Numeric team IDs and content-addressed tree IDs share T but have disjoint byte shapes.
    if codeyard_model::node_id::decode(id).is_none()
        && id.split_once('_').is_some_and(|(kind, _)| {
            matches!(
                kind,
                "C" | "B" | "TAG" | "REF" | "T" | "RR" | "SCR" | "STAT"
            )
        })
    {
        return crate::git_objects::node(id, request).await;
    }
    let (mut kind, mut id) = codeyard_model::node_id::decode(id).ok_or_else(missing)?;
    if kind == "IC" {
        kind = if id % 2 == 0 { "IC" } else { "PC" };
        id /= 2;
    }
    let pool = &request.state.pool;
    if request.bounded()
        && (matches!(kind, "O" | "T") || (kind == "U" && id != request.auth.user.id))
    {
        return Err(token_denied());
    }
    let value = match kind {
        "U" => crate::user::actor_by_id(id, request).await,
        "O" => {
            request.require_oauth_scope("read:org")?;
            crate::user::organization_by_id(id, request).await
        }
        "T" => crate::team::by_id(id, request).await,
        "R" => {
            let repo = crate::repository::by_id(id, request).await?;
            Ok(crate::repository::object(&repo))
        }
        "I" => {
            let issue = codeyard_db_repos::issues::get_issue_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            crate::repository::by_id(issue.repo_id, request).await?;
            Ok(crate::issue::object(&issue))
        }
        "PR" => {
            let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            crate::repository::by_id(pull.repo_id, request).await?;
            Ok(crate::issue::pull_object(&pull))
        }
        "CRU" | "CSU" => crate::checks::node(kind, id, request).await,
        "IT" => crate::issue::metadata::type_node(id, request).await,
        "LB" => crate::issue::linked_branch_node(id, request).await,
        "RRS" => crate::rulesets::node(id, request).await,
        "L" => {
            let label = codeyard_db_repos::labels::get_label_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            crate::repository::by_id(label.repo_id, request).await?;
            let scope = crate::repository::shared_read_scope(
                label.repo_id,
                &["issues", "pull-requests", "discussions"],
                request,
            )
            .await?;
            let mut value = crate::repository::label(&label);
            value["_scope"] = json!(scope);
            Ok(value)
        }
        "IC" => {
            let comment = codeyard_db_repos::issues::get_issue_comment(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            let parent = codeyard_db_repos::issues::get_issue_by_id(pool, comment.issue_id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            crate::repository::by_id(parent.repo_id, request).await?;
            crate::issue::comment(&comment, request).await
        }
        "PC" => {
            // The body and its review's visibility are read together, so a draft edited and
            // submitted meanwhile cannot show its unpublished text.
            let comment =
                codeyard_db_repos::pull_requests::reviews::get_visible_pull_request_comment(
                    pool,
                    id,
                    request
                        .private_viewer(
                            codeyard_db_repos::graphql::node_owner(pool, "PRRC", id)
                                .await
                                .map_err(unavailable)?
                                .ok_or_else(missing)?,
                            "pull-requests",
                        )
                        .await?
                        .unwrap_or(-1),
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            let parent = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
                pool,
                comment.pull_request_id,
            )
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
            crate::repository::by_id(parent.repo_id, request).await?;
            crate::issue::pull_comment(&comment, request).await
        }
        "M" | "REA" | "RE" | "RA" | "CS" | "PRR" | "PRRC" | "IE" | "S" | "ART" | "CC" => {
            crate::node_objects::database(kind, id, request).await
        }
        "PVT" | "PVTF" | "PVTSSF" | "PVTIF" | "PVTI" | "DI" => {
            crate::projects::node(kind, id, request).await
        }
        _ => Err(missing()),
    }?;
    if let Some(repo) = value["_repo"]
        .as_i64()
        .or_else(|| (kind == "R").then_some(id))
    {
        request
            .authorize(
                repo,
                match kind {
                    "I" | "IC" | "IT" => "issues",
                    "L" | "M" => value["_scope"].as_str().unwrap_or("issues"),
                    "PR" | "PC" => "pull-requests",
                    "CC" | "LB" => "contents",
                    _ => "metadata",
                },
                false,
            )
            .await?;
    }
    Ok(value)
}

fn document(result: async_graphql::Response) -> Json<Value> {
    let mut document = json!(result);
    if let Some(errors) = document.get_mut("errors").and_then(Value::as_array_mut) {
        for error in errors {
            if let Some(kind) = error
                .get_mut("extensions")
                .and_then(Value::as_object_mut)
                .and_then(|extensions| extensions.remove("type"))
            {
                error["type"] = kind;
                if error["extensions"]
                    .as_object()
                    .is_some_and(|extensions| extensions.is_empty())
                {
                    error
                        .as_object_mut()
                        .expect("GraphQL error object")
                        .remove("extensions");
                }
            }
        }
    }
    Json(document)
}

async fn execute(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: std::result::Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let authentication = match codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await
    {
        Ok(Some(auth)) => auth,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(error) => {
            if !headers.contains_key(header::AUTHORIZATION) {
                return (StatusCode::UNAUTHORIZED, Json(json!({"message":"This endpoint requires you to be authenticated.","documentation_url":"https://docs.github.com/graphql/guides/forming-calls-with-graphql#authenticating-with-graphql"}))).into_response();
            }
            return error.into_response();
        }
    };
    let personal = match codeyard_identity::github_apps::authority(&state.pool, authentication.token.id).await {
        Ok(Some(bounds)) if bounds.personal().is_some() => bounds.personal().cloned(),
        Ok(Some(_)) => return (StatusCode::FORBIDDEN, Json(json!({"message":"Integration credentials require repository-scoped REST or Git access"}))).into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Ok(None) => None,
    };
    let oauth_scopes =
        match codeyard_db_identity::oauth_apps::token(&state.pool, authentication.token.id).await {
            Ok(token) => token.map(|token| token.scopes),
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let mut body = match crate::request::parse(body) {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"message":"Invalid GraphQL request"})),
            )
                .into_response()
        }
    };
    let actor = authentication.database_actor();
    let rate = match codeyard_db_repos::graphql::admit_query(&state.pool, &actor).await {
        Ok(Some(rate)) => rate,
        Ok(None) => {
            return document(async_graphql::Response::from_errors(vec![
                async_graphql::ServerError::new("API rate limit exceeded", None),
            ]))
            .into_response()
        }
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                document(async_graphql::Response::from_errors(vec![unavailable(
                    error,
                )
                .into_server_error(async_graphql::Pos::default())])),
            )
                .into_response()
        }
    };
    let read = body.parsed_query().is_ok_and(|document| {
        document.operations.iter().all(|(_, operation)| {
            operation.node.ty == async_graphql::parser::types::OperationType::Query
        })
    });
    let routing_state = state.clone();
    let query_timeout = state.graphql_query_timeout;
    let request = body.data(RequestContext {
        state,
        auth: authentication,
        actor,
        oauth_scopes: oauth_scopes.clone(),
        personal,
        nodes: AtomicUsize::new(0),
        bytes: AtomicUsize::new(0),
        diffs: Default::default(),
        mergeability: Default::default(),
        rate,
    });
    codeyard_api_http::read_routing::respond(&routing_state, &headers, read, async {
        let result =
            match tokio::time::timeout(query_timeout, crate::schema::schema().execute(request))
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    async_graphql::Response::from_errors(vec![async_graphql::ServerError::new(
                        "Query timed out; no success was confirmed",
                        None,
                    )])
                }
            };
        let mut response = document(result).into_response();
        if let Some(scopes) = oauth_scopes {
            response.headers_mut().insert(
                "x-oauth-scopes",
                scopes.parse().expect("validated OAuth scopes"),
            );
        }
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        response
    })
    .await
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/api/graphql", post(execute))
        .layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::bounded_form_body,
        ))
}
