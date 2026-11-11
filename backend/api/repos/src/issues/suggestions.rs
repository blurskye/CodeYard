use crate::discussion::repository;
use crate::discussion::unavailable;
use crate::discussion::ResultPage;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_app::app::AppState;
use codeyard_db_repos::suggestions::Action;
use codeyard_db_repos::suggestions::ListFilter;
use codeyard_db_repos::suggestions::Proposal;
use codeyard_db_repos::suggestions::Refusal;
use codeyard_webui::pages::repos::issues::DetailDraft;
use codeyard_webui::pages::repos::issues::IssueSuggestion;
use codeyard_webui::pages::repos::issues::IssueSuggestions;
use codeyard_webui::pages::repos::issues::Kind;
use codeyard_webui::pages::repos::issues::SuggestionDraft;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

fn failure(error: &anyhow::Error) -> (StatusCode, String) {
    match codeyard_db_repos::suggestions::refusal(error) {
        Some(Refusal::Missing) => (
            StatusCode::NOT_FOUND,
            "Issue or suggestion not found, or suggestions disabled.".into(),
        ),
        Some(Refusal::Denied) => (
            StatusCode::FORBIDDEN,
            "Issue suggestion operation denied.".into(),
        ),
        Some(_) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Suggestion service unavailable.".into(),
        ),
    }
}
pub(crate) async fn load(
    ctx: &PageContext,
    repo: &codeyard_database::models::Repository,
    issue: i64,
    locked: bool,
    admin: bool,
) -> ResultPage<Option<Box<IssueSuggestions>>> {
    let Some(actor) = &ctx.user else {
        return Ok(None);
    };
    let enabled = codeyard_db_repos::suggestions::enabled(&ctx.state.pool, repo.id)
        .await
        .map_err(|_| unavailable(ctx))?;
    if !enabled && !admin {
        return Ok(None);
    }
    let state = ctx
        .query
        .get("suggestion_state")
        .cloned()
        .unwrap_or_else(|| "pending".into());
    let page = ctx
        .query
        .get("suggestion_page")
        .map(|s| s.parse::<usize>())
        .transpose()
        .ok()
        .flatten()
        .unwrap_or(1);
    let offset = page
        .checked_sub(1)
        .and_then(|n| n.checked_mul(20))
        .and_then(|n| i64::try_from(n).ok())
        .ok_or_else(|| ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid suggestion page."))?;
    if ctx
        .query
        .get("suggestion_page")
        .is_some_and(|s| s.parse::<usize>().is_err())
        || !codeyard_db_repos::suggestions::valid_state(&state)
    {
        return Err(ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid suggestion filters.",
        ));
    }
    let triage = codeyard_db_repos::suggestions::can_triage(&ctx.state.pool, repo.id, actor.id)
        .await
        .map_err(|_| unavailable(ctx))?;
    let mut rows = if enabled {
        codeyard_db_repos::suggestions::list(
            &ctx.state.pool,
            repo.id,
            actor.id,
            issue,
            &ListFilter {
                state: &state,
                action: None,
                own: !triage,
                descending: true,
                offset,
                limit: 21,
            },
        )
        .await
        .map_err(|error| {
            let (status, message) = failure(&error);
            ctx.error(status, &message)
        })?
    } else {
        Vec::new()
    };
    let has_more = rows.len() > 20;
    rows.truncate(20);
    let ids = rows
        .iter()
        .flat_map(|s| [s.actor_id, s.resolved_by])
        .flatten()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let people = codeyard_db_identity::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(|u| (u.id, u.username))
        .collect::<HashMap<_, _>>();
    let rows = rows
        .into_iter()
        .map(|s| {
            Ok(IssueSuggestion {
                id: s.id,
                action: s.action,
                target_name: s.target_name,
                value: serde_json::from_str(&s.value_json).map_err(|_| unavailable(ctx))?,
                state: s.state,
                rationale: s.rationale,
                confidence: s.confidence,
                actor: s.actor_id.and_then(|id| people.get(&id).cloned()),
                resolved_by: s.resolved_by.and_then(|id| people.get(&id).cloned()),
                created: s.created_at.to_rfc3339(),
                updated: s.updated_at.to_rfc3339(),
            })
        })
        .collect::<ResultPage<Vec<_>>>()?;
    let mut can_propose = enabled && !repo.is_archived && !locked;
    if !actor.is_admin {
        if let Some(owner) = repo.personal_owner_id() {
            if owner != actor.id {
                can_propose &= !codeyard_db_identity::users::blocks::is_blocked(
                    &ctx.state.pool,
                    owner,
                    actor.id,
                )
                .await
                .map_err(|_| unavailable(ctx))?
                    && !codeyard_db_identity::users::blocks::is_blocked(
                        &ctx.state.pool,
                        actor.id,
                        owner,
                    )
                    .await
                    .map_err(|_| unavailable(ctx))?;
            }
        }
    }
    if let Some(org) = repo.owner_org_id {
        can_propose &= codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(
            &ctx.state.pool,
            org,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        .is_some();
    }
    Ok(Some(Box::new(IssueSuggestions {
        enabled,
        can_review: triage && !repo.is_archived,
        can_propose,
        rows,
        page,
        has_more,
        state,
    })))
}
async fn native_propose(
    ctx: &PageContext,
    repo: &codeyard_database::models::Repository,
    (owner, name): (&str, &str),
    issue: i64,
    form: &HashMap<String, String>,
) -> ResultPage<()> {
    let invalid = || {
        ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Select a valid change and target.",
        )
    };
    let actor = ctx.require_user()?.id;
    let action = Action::parse(&field(form, "change")).ok_or_else(invalid)?;
    let target = field(form, "target");
    let raw = field(form, "value");
    let mut value = Value::Null;
    let target_id = match action {
        Action::SetType if target.is_empty() => None,
        Action::SetType | Action::AddField => {
            let kind = if action == Action::SetType {
                "type"
            } else {
                "field"
            };
            let catalog = codeyard_db_repos::issue_attributes::catalog(
                &ctx.state.pool,
                repo.id,
                Some(actor),
                kind,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            let a = catalog
                .iter()
                .find(|a| a.name.to_lowercase() == target.to_lowercase())
                .ok_or_else(invalid)?;
            if action == Action::AddField {
                value = match a.data_type.as_deref() {
                    Some("number") => serde_json::from_str(&raw).map_err(|_| invalid())?,
                    Some("multi_select") => Value::Array(
                        raw.lines()
                            .filter(|s| !s.is_empty())
                            .map(|s| Value::String(s.into()))
                            .collect(),
                    ),
                    _ => Value::String(raw),
                };
            }
            Some(a.id)
        }
        Action::AddLabel => {
            let labels = codeyard_db_repos::labels::list_repo_labels(&ctx.state.pool, repo.id)
                .await
                .map_err(|_| unavailable(ctx))?;
            // The exact name wins; names that differ only in case are distinct labels.
            let label = labels
                .iter()
                .find(|l| l.name == target)
                .or_else(|| {
                    labels
                        .iter()
                        .find(|l| l.name.to_lowercase() == target.to_lowercase())
                })
                .ok_or_else(invalid)?;
            Some(label.id)
        }
        Action::AddAssignee => Some(
            codeyard_db_identity::users::accounts::get_user_by_username(
                &ctx.state.pool,
                target.trim_start_matches('@'),
            )
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(invalid)?
            .id,
        ),
        Action::CloseIssue => {
            value = if raw.is_empty() {
                Value::Null
            } else {
                Value::String(raw)
            };
            if target.is_empty() {
                None
            } else {
                Some(crate::discussion::issue_reference(ctx, (owner, name), &target).await?)
            }
        }
    };
    let p = Proposal {
        action,
        target_id,
        value,
        rationale: Some(field(form, "rationale")).filter(|s| !s.is_empty()),
        confidence: Some(field(form, "confidence")).filter(|s| !s.is_empty()),
    };
    codeyard_db_repos::suggestions::propose(
        &ctx.state.pool,
        repo.id,
        actor,
        &codeyard_database::queries::users::accounts::AccountCredential::Session(
            &codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
        ),
        issue,
        &p,
        None,
    )
    .await
    .map_err(|error| {
        let (status, message) = failure(&error);
        ctx.error(status, &message)
    })?;
    Ok(())
}
pub(crate) async fn native_change(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let actor = ctx.require_user()?;
    let shown = crate::discussion::item(&ctx, &repo, Kind::Issue, number).await?;
    let draft = DetailDraft {
        suggestion: Some(Box::new(SuggestionDraft {
            repository_id: field(&form, "repository_id").parse().ok(),
            issue_id: field(&form, "issue_id").parse().ok(),
            change: field(&form, "change"),
            target: field(&form, "target"),
            value: field(&form, "value"),
            rationale: field(&form, "rationale"),
            confidence: field(&form, "confidence"),
        })),
        ..Default::default()
    };
    if field(&form, "repository_id").parse::<i64>().ok() != Some(repo.record.id)
        || field(&form, "issue_id").parse::<i64>().ok() != Some(shown.id)
    {
        return crate::discussion::render_detail(&ctx, &repo, Kind::Issue, &shown, &draft,
            Some("The suggestion target changed. Your draft is preserved; return to the original issue before submitting."), StatusCode::CONFLICT).await;
    }
    match field(&form, "action").as_str() {
        "set_enabled" => codeyard_db_repos::suggestions::set_enabled(
            &ctx.state.pool,
            repo.record.id,
            actor.id,
            &codeyard_database::queries::users::accounts::AccountCredential::Session(
                &codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
            ),
            field(&form, "enabled") == "true",
        )
        .await
        .map_err(|e| {
            let (status, message) = failure(&e);
            ctx.error(status, &message)
        })?,
        "propose" => {
            if let Err(error) =
                native_propose(&ctx, &repo.record, (&owner, &name), shown.id, &form).await
            {
                return crate::discussion::render_detail(&ctx,&repo,Kind::Issue,&shown,&draft,Some("Suggestion not saved. Review your preserved draft and check the target and value."),error.status()).await;
            }
        }
        action @ ("approve" | "dismiss") => {
            let id = field(&form, "suggestion_id")
                .parse()
                .map_err(|_| ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid suggestion."))?;
            codeyard_repositories::issues::suggestions::resolve(
                &ctx.state.pool,
                &ctx.state.notification_broadcaster,
                actor,
                &codeyard_database::queries::users::accounts::AccountCredential::Session(
                    &codeyard_api_http::session::extract_session_id(&ctx.headers)
                        .unwrap_or_default(),
                ),
                (&repo.record, &owner),
                (shown.id, id),
                action == "approve",
            )
            .await
            .map_err(|e| {
                let (status, message) = failure(&e);
                ctx.error(status, &message)
            })?;
        }
        _ => {
            return Err(ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid suggestion action.",
            ))
        }
    }
    let suffix = if field(&form, "action") == "propose" {
        "?suggestion_state=all#issue-suggestions"
    } else {
        "#issue-suggestions"
    };
    Ok(ctx.redirect(&format!(
        "/{}/{}/issues/{number}{suffix}",
        codeyard_types::paths::encode_component(&owner),
        codeyard_types::paths::encode_component(&name)
    )))
}
#[derive(Deserialize)]
pub struct AgentInput {
    action: String,
    target_id: Option<i64>,
    #[serde(default)]
    target_value: Value,
    rationale: Option<String>,
    confidence: Option<String>,
}
/// Codeyard's agent submission API; GitHub's suggestions API intentionally only reviews proposals.
pub async fn agent_propose(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Json(input): Json<AgentInput>,
) -> Result<(StatusCode, Json<Value>), Response> {
    let (repo, auth) =
        codeyard_app::api_shared::github::repository(&state, &headers, &owner, &name, true)
            .await
            .map_err(IntoResponse::into_response)?;
    let auth = auth.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ApiError::new("unauthorized", "Requires authentication")),
        )
            .into_response()
    })?;
    // OAuth and GitHub App tokens keep their scopes and permissions here, as on the GitHub API.
    let unavailable = |_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::new("unavailable", "Token policy unavailable")),
        )
            .into_response()
    };
    auth.repository_scope(&state.pool, repo.id, "issues", true)
        .await
        .map_err(|error| {
            use codeyard_identity::github_auth::ScopeError;
            let status = match error {
                ScopeError::Missing => StatusCode::NOT_FOUND,
                ScopeError::Denied => StatusCode::FORBIDDEN,
                ScopeError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            };
            (
                status,
                Json(ApiError::new(
                    "token_scope",
                    "Resource not accessible by token",
                )),
            )
                .into_response()
        })?;
    let scoped = codeyard_db_identity::oauth_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(unavailable)?
        .map(|token| {
            codeyard_types::oauth_apps::allows(
                &token.scopes,
                &format!("/repos/{owner}/{name}/issues/{number}/comments"),
                true,
            )
        });
    if scoped == Some(false) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "forbidden",
                "Resource not accessible by token",
            )),
        )
            .into_response());
    }
    let issue = codeyard_db_repos::issues::get_issue(&state.pool, repo.id, number)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new("unavailable", "Issue lookup unavailable")),
            )
                .into_response()
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Issue not found")),
            )
                .into_response()
        })?;
    let action = Action::parse(&input.action).ok_or_else(|| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError::new(
                "invalid_suggestion",
                "Invalid suggestion action",
            )),
        )
            .into_response()
    })?;
    let target_repository = if action == Action::CloseIssue {
        if let Some(id) = input.target_id {
            let missing = || {
                (
                    StatusCode::NOT_FOUND,
                    Json(ApiError::new("not_found", "Issue not found")),
                )
                    .into_response()
            };
            let target = codeyard_db_repos::issues::get_issue_by_id(&state.pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            auth.repository_scope(&state.pool, target.repo_id, "issues", false)
                .await
                .map_err(|error| match error {
                    codeyard_identity::github_auth::ScopeError::Unavailable => {
                        unavailable(anyhow::anyhow!("Token policy unavailable"))
                    }
                    _ => missing(),
                })?;
            let readable = codeyard_db_repos::repositories::access::readable_repository_ids(
                &state.pool,
                Some(auth.user.id),
                &[target.repo_id],
            )
            .await
            .map_err(unavailable)?;
            if !readable.contains(&target.repo_id) {
                return Err(missing());
            }
            Some(target.repo_id)
        } else {
            None
        }
    } else {
        None
    };
    let row = codeyard_db_repos::suggestions::propose(
        &state.pool,
        repo.id,
        auth.user.id,
        &auth.credential(),
        issue.id,
        &Proposal {
            action,
            target_id: input.target_id,
            value: input.target_value,
            rationale: input.rationale,
            confidence: input.confidence,
        },
        target_repository,
    )
    .await
    .map_err(|error| {
        let (status, message) = failure(&error);
        (status, Json(ApiError::new("suggestion_refused", &message))).into_response()
    })?;
    let value = codeyard_webhooks::payloads::issue_suggestion(&row).map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::new("unavailable", "Suggestion unavailable")),
        )
            .into_response()
    })?;
    Ok((StatusCode::CREATED, Json(value)))
}
