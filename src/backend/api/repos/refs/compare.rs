//! Branch comparison: the compare page, and the comparison that pull request forms share with it.
use crate::backend::api::repos::pulls::diff::diff_model;
use crate::backend::{
    api::{
        http::context::PageContext,
        repos::pulls::pull::{branches, page, repository, RepositoryData, ResultPage},
    },
    app::AppState,
    db,
    repositories::store::repo_store,
};
use crate::webui::pages::repos::ref_path::compare_url;
use crate::webui::pages::repos::{
    issues::{self as view, NewDraft},
    pulls,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};

pub(crate) async fn comparison(
    ctx: &PageContext,
    repo: &RepositoryData,
    head_repo: &db::models::Repository,
    refs: &HashMap<String, String>,
    draft: &NewDraft,
) -> Result<pulls::Diff, Response> {
    if head_repo.id == repo.record.id && draft.head == draft.base {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Base and compare branch must differ.",
        ));
    }
    let head = refs
        .get(&format!("refs/heads/{}", draft.head))
        .cloned()
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Compare branch not found."))?;
    let store = repo_store(&repo.record, ctx.state.cluster.as_ref());
    let base = store
        .commit(&format!("refs/heads/{}", draft.base))
        .await
        .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Base branch not found."))?
        .sha;
    let value = if head_repo.id == repo.record.id {
        store.diff(&base, &head).await
    } else {
        store
            .diff_from(
                &repo_store(head_repo, ctx.state.cluster.as_ref()),
                &base,
                &head,
            )
            .await
    }
    .map_err(|error| {
        crate::backend::api::repos::chrome::store_error(ctx, Some(&repo.view.chrome), error)
    })?;
    Ok(diff_model(value, None))
}

/// `/:owner/:repo/compare/BASE...HEAD`. The bare address compares the default branch with itself.
/// Its form sends `?base=&head=`, which redirects to the address that names both branches.
pub(crate) async fn native_compare_page(
    State(state): State<Arc<AppState>>,
    Path(params): Path<HashMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &params["owner"], &params["repo"]).await?;
    let default = repo.record.default_branch.clone();
    let spec = params.get("spec").map_or("", String::as_str);
    let (base, head) = if !spec.is_empty() {
        match spec.split_once("...") {
            Some((base, head)) if !base.is_empty() && !head.is_empty() => {
                (base.to_owned(), head.to_owned())
            }
            _ => {
                return Err(ctx.repo_error(
                    &repo.view.chrome,
                    StatusCode::NOT_FOUND,
                    "Branch not found.",
                ))
            }
        }
    } else if ctx.query.contains_key("base") || ctx.query.contains_key("head") {
        let pick = |key: &str| {
            ctx.query
                .get(key)
                .filter(|name| !name.is_empty())
                .unwrap_or(&default)
                .clone()
        };
        return Ok(ctx.redirect(&compare_url(&repo.view.url(), &pick("base"), &pick("head"))));
    } else {
        (default.clone(), default.clone())
    };
    let (names, refs) = branches(&ctx, &repo).await?;
    if !names.contains(&base) || !names.contains(&head) {
        return Err(ctx.repo_error(
            &repo.view.chrome,
            StatusCode::NOT_FOUND,
            "Branch not found.",
        ));
    }
    let diff = if base == head {
        None
    } else {
        let draft = NewDraft {
            base: base.clone(),
            head: head.clone(),
            ..NewDraft::default()
        };
        Some(comparison(&ctx, &repo, &repo.record, &refs, &draft).await?)
    };
    // An open pull request for this pair is linked to in place of a new one.
    let open_pull = if base == head {
        None
    } else {
        db::pull_requests::records::list_open_pull_requests_from_branches(
            &ctx.state.pool,
            repo.record.id,
        )
        .await
        .map_err(|_| {
            ctx.repo_error(
                &repo.view.chrome,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Pull requests are unavailable.",
            )
        })?
        .into_iter()
        .find(|pull| pull.base_branch == base && pull.head_branch == head)
        .map(|pull| pulls::PullSummary {
            number: pull.number,
            title: pull.title,
        })
    };
    Ok(ctx.page_model(
        "Comparing changes",
        page(view::Page::Compare {
            repository: repo.view,
            base,
            head,
            branches: names,
            diff,
            open_pull,
            split: ctx.query.get("view").is_some_and(|view| view == "split"),
        }),
    ))
}
