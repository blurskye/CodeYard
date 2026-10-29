//! GitHub's compile-time GraphQL schema and domain-backed resolver families.
mod arguments;
mod bindings;
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

use super::github::{auth, node_id};
use crate::backend::{app::AppState, db};
use arguments::Arguments;
use async_graphql::{Error, ErrorExtensions};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{atomic::AtomicUsize, Arc};

type Result<T> = async_graphql::Result<T>;

struct RequestContext {
    state: Arc<AppState>,
    auth: auth::Authentication,
    /// Scopes granted to an OAuth token; other credentials have none.
    oauth_scopes: Option<String>,
    /// A fine-grained token's repository permissions. Like a job token, it reaches
    /// repositories through `authorize` only, and no account or organization fields.
    personal: Option<Value>,
    nodes: AtomicUsize,
    /// Bytes of text the request has loaded or returned, bounded by `MAX_RESPONSE_BYTES`.
    bytes: AtomicUsize,
    rate: db::graphql::RateLimit,
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

    async fn authorize(&self, repo: i64, scope: &str, write: bool) -> Result<()> {
        self.auth
            .repository_scope(&self.state.pool, repo, scope, write)
            .await
            .map_err(|error| match error {
                auth::ScopeError::Missing => missing(),
                auth::ScopeError::Denied => token_denied(),
                auth::ScopeError::Unavailable => unavailable(()),
            })?;
        // Every repository read and write needs the repo grant, as the REST repository routes do.
        self.require_oauth_scope("repo")
    }

    /// Each resolver checks the grant it needs, as the REST routes check theirs.
    fn require_oauth_scope(&self, scope: &str) -> Result<()> {
        if let Some(permissions) = &self.personal {
            // Repository reads and writes were checked by `authorize`, and cross-repository
            // lists stop at `bounded`.
            return match scope {
                "repo" => Ok(()),
                "workflow"
                    if crate::backend::identity::github_apps::allows(
                        permissions,
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
            Some(scopes) if !crate::backend::identity::oauth_apps::has_scope(scopes, scope) => {
                Err(token_denied())
            }
            _ => Ok(()),
        }
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

fn unavailable<E>(_: E) -> Error {
    Error::new("Service unavailable")
}

fn url(path: &str) -> String {
    format!("{}{path}", super::http::config::link_origin())
}

fn arg(ctx: &Arguments, name: &str) -> Result<String> {
    Ok(ctx.args.try_get(name)?.string()?.to_owned())
}

async fn node(id: &str, request: &RequestContext) -> Result<Value> {
    if id.starts_with("D_") || id.starts_with("DIC_") || id.starts_with("DC_") {
        return discussions::node(id, request).await;
    }
    if id.starts_with("RR_") {
        return review_requests::node(id, request).await;
    }
    if let Some(key) = id.strip_prefix("GIST_") {
        return gists::node(key, request).await;
    }
    // Numeric team IDs and content-addressed tree IDs share T but have disjoint byte shapes.
    if node_id::decode(id).is_none()
        && id.split_once('_').is_some_and(|(kind, _)| {
            matches!(
                kind,
                "C" | "B" | "TAG" | "REF" | "T" | "RR" | "SCR" | "STAT"
            )
        })
    {
        return git_objects::node(id, request).await;
    }
    let (mut kind, mut id) = node_id::decode(id).ok_or_else(missing)?;
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
        "U" => user::actor_by_id(id, request).await,
        "O" => {
            request.require_oauth_scope("read:org")?;
            user::organization_by_id(id, request).await
        }
        "T" => team::by_id(id, request).await,
        "R" => {
            let repo = repository::by_id(id, request).await?;
            Ok(repository::object(&repo))
        }
        "I" => {
            let issue = db::issues::get_issue_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            repository::by_id(issue.repo_id, request).await?;
            Ok(issue::object(&issue))
        }
        "PR" => {
            let pull = db::pull_requests::records::get_pull_request_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            repository::by_id(pull.repo_id, request).await?;
            Ok(issue::pull_object(&pull))
        }
        "L" => {
            let label = db::labels::get_label_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            repository::by_id(label.repo_id, request).await?;
            Ok(repository::label(&label))
        }
        "IC" => {
            let comment = db::issues::get_issue_comment(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            let parent = db::issues::get_issue_by_id(pool, comment.issue_id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            repository::by_id(parent.repo_id, request).await?;
            issue::comment(&comment, request).await
        }
        "PC" => {
            // The body and its review's visibility are read together, so a draft edited and
            // submitted meanwhile cannot show its unpublished text.
            let comment = db::pull_requests::reviews::get_visible_pull_request_comment(
                pool,
                id,
                request.auth.user.id,
            )
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
            let parent =
                db::pull_requests::records::get_pull_request_by_id(pool, comment.pull_request_id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
            repository::by_id(parent.repo_id, request).await?;
            issue::pull_comment(&comment, request).await
        }
        "M" | "REA" | "RE" | "RA" | "CS" | "PRR" | "PRRC" | "IE" | "S" | "ART" | "CC" => {
            node_objects::database(kind, id, request).await
        }
        "PVT" | "PVTF" | "PVTSSF" | "PVTIF" | "PVTI" | "DI" => {
            projects::node(kind, id, request).await
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
                    "I" | "IC" | "L" => "issues",
                    "PR" | "PC" => "pull-requests",
                    "CC" => "contents",
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
    let authentication = match auth::authenticate(&state, &headers, true).await {
        Ok(Some(auth)) => auth,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(error) => {
            if !headers.contains_key(header::AUTHORIZATION) {
                return (StatusCode::UNAUTHORIZED, Json(json!({"message":"This endpoint requires you to be authenticated.","documentation_url":"https://docs.github.com/graphql/guides/forming-calls-with-graphql#authenticating-with-graphql"}))).into_response();
            }
            return error.into_response();
        }
    };
    let personal = match crate::backend::identity::github_apps::authority(&state.pool, authentication.token.id).await {
        Ok(Some(bounds)) if bounds.personal().is_some() => Some(bounds.permissions),
        Ok(Some(_)) => return (StatusCode::FORBIDDEN, Json(json!({"message":"Integration credentials require repository-scoped REST or Git access"}))).into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Ok(None) => None,
    };
    let oauth_scopes = match db::oauth_apps::token(&state.pool, authentication.token.id).await {
        Ok(token) => token.map(|token| token.scopes),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let body = match request::parse(body) {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"message":"Invalid GraphQL request"})),
            )
                .into_response()
        }
    };
    let rate = match db::graphql::admit_query(&state.pool, authentication.token.id).await {
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
    let request = body.data(RequestContext {
        state,
        auth: authentication,
        oauth_scopes: oauth_scopes.clone(),
        personal,
        nodes: AtomicUsize::new(0),
        bytes: AtomicUsize::new(0),
        rate,
    });
    let result = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        schema::schema().execute(request),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => async_graphql::Response::from_errors(vec![async_graphql::ServerError::new(
            "Query timed out; no success was confirmed",
            None,
        )]),
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
}

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/graphql", post(execute))
        .layer(axum::middleware::from_fn(
            super::http::middleware::bounded_form_body,
        ))
}
