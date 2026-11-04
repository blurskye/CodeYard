use crate::discussion::branches;
use crate::discussion::expected;
use crate::discussion::item;
use crate::discussion::render_detail;
use crate::discussion::repository;
use crate::discussion::safe_api_error;
use crate::discussion::unavailable;
use crate::discussion::ResultPage;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::pull_requests::branches::refresh_open_pull_request;
use codeyard_repos_core::store_types::MergeMethod;
use codeyard_repos_pulls::repositories::pull_requests::merge::MergeRequest;
use codeyard_repos_pulls::repositories::pull_requests::merge::MergeResponse;
use codeyard_webui::pages::repos::issues::DetailDraft;
use codeyard_webui::pages::repos::issues::Kind;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn merge_pull_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(request): Json<MergeRequest>,
) -> Result<Json<MergeResponse>, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    codeyard_repos_pulls::repositories::pull_requests::merge::merge_pull_request(
        &state,
        codeyard_db_core::queries::credentials::Actor::from((
            actor.id,
            codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        )),
        (owner, repo, number),
        request,
    )
    .await
    .map(Json)
    .map_err(crate::pulls::operation_error)
}

pub(crate) async fn native_merge(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    // Catch up with the branches first, so a stale form shows the pull request as it is
    // now. Only a writer may start that refresh; everyone else is refused below.
    if repo.view.can_write {
        let pull = codeyard_db_repos::pull_requests::records::get_pull_request(
            &ctx.state.pool,
            repo.record.id,
            number,
        )
        .await
        .map_err(|_| unavailable(&ctx))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
        let _ = refresh_open_pull_request(
            &ctx.state.pool,
            &repo.record,
            &ctx.state.ci_runs,
            ctx.require_user()?.id,
            pull.id,
            Some(codeyard_db_core::models::AccessLevel::Owner),
        )
        .await;
    }
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    // A refused merge shows the form again, so it keeps the method that was submitted.
    let draft = DetailDraft {
        merge_method: field(&form, "merge_method"),
        commit_title: field(&form, "commit_title"),
        commit_message: field(&form, "commit_message"),
        ..DetailDraft::default()
    };
    if let Err(message) = expected(&form, &value) {
        return render_detail(
            &ctx,
            &repo,
            Kind::Pull,
            &value,
            &draft,
            Some(message),
            StatusCode::CONFLICT,
        )
        .await;
    }
    if value.state != "open" || field(&form, "confirm") != "merge" {
        return render_detail(
            &ctx,
            &repo,
            Kind::Pull,
            &value,
            &draft,
            Some("Confirm an open pull request before merging."),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    let merge_method = match field(&form, "merge_method").as_str() {
        "" => MergeMethod::Merge,
        method => match MergeMethod::parse(method) {
            Some(method) => method,
            None => {
                return render_detail(
                    &ctx,
                    &repo,
                    Kind::Pull,
                    &value,
                    &draft,
                    Some("Choose a merge method."),
                    StatusCode::UNPROCESSABLE_ENTITY,
                )
                .await
            }
        },
    };
    let compare = value
        .comparison
        .as_ref()
        .ok_or_else(|| ctx.error(StatusCode::CONFLICT, "Comparison data is unavailable."))?;
    let (_, refs) = branches(&ctx, &repo.record).await?;
    // Early conflict feedback only. The shared operation below independently
    // admits these submitted immutable OIDs and publishes with a ref transaction.
    if field(&form, "expected_head") != compare.head_sha
        || field(&form, "expected_base") != compare.base_sha
        || form
            .get("expected_base_branch")
            .is_some_and(|branch| branch != &compare.base)
        || form
            .get("expected_target")
            .is_some_and(|target| target != &compare.merge_target)
        || refs.get(&format!("refs/heads/{}", compare.base)) != Some(&compare.base_sha)
    {
        return render_detail(&ctx, &repo, Kind::Pull, &value, &draft, Some("A branch changed since this pull request was created. This comparison is out of date; merge was not attempted."), StatusCode::CONFLICT).await;
    }
    match codeyard_repos_pulls::repositories::pull_requests::merge::merge_pull_request_checked(
        &ctx.state,
        ctx.database_actor(ctx.require_user()?.id),
        (owner, name, number),
        codeyard_repos_pulls::repositories::pull_requests::merge::MergeRequest {
            expected_head: field(&form, "expected_head"),
            expected_base: field(&form, "expected_base"),
            expected_base_branch: form.get("expected_base_branch").cloned(),
            expected_target: form.get("expected_target").cloned(),
            merge_method,
            author_email: None,
            commit_title: form
                .get("commit_title")
                .filter(|value| !value.is_empty())
                .cloned(),
            commit_message: form
                .get("commit_message")
                .filter(|value| !value.is_empty())
                .cloned(),
        },
        Some((repo.record.id, value.id)),
    )
    .await
    .map_err(crate::pulls::operation_error)
    {
        Ok(result) if result.merged => {
            Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(Kind::Pull), number)))
        }
        Ok(_) => {
            render_detail(
                &ctx,
                &repo,
                Kind::Pull,
                &value,
                &draft,
                Some("The merge could not be confirmed. Reload before trying again."),
                StatusCode::CONFLICT,
            )
            .await
        }
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(
                &ctx,
                &repo,
                Kind::Pull,
                &value,
                &draft,
                Some(&message),
                status,
            )
            .await
        }
    }
}
