//! Branch comparison: the compare page, and the comparison that pull request forms share with it.
use crate::backend::api::repos::pulls::diff::diff_model;
use crate::backend::{
    api::{
        http::context::PageContext,
        repos::discussion::{branches, page, repository, RepositoryData, ResultPage},
    },
    app::AppState,
    db,
    repositories::{reads::same_fork_network, store::repo_store},
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use codeyard_git::error::ErrorKind;
use codeyard_webui::pages::repos::ref_path::compare_url;
use codeyard_webui::pages::repos::{
    issues::{self as view, NewDraft},
    pulls,
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
        .map_err(|error| {
            if error.kind == ErrorKind::NotFound {
                ctx.error(StatusCode::BAD_REQUEST, "Base branch not found.")
            } else {
                crate::backend::api::repos::chrome::store_error(ctx, Some(&repo.view.chrome), error)
            }
        })?
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

async fn comparison_side(
    ctx: &PageContext,
    repo: &RepositoryData,
    owner: &str,
    reference: &str,
) -> Result<(db::models::Repository, String), Response> {
    let (source_owner, reference) = reference.split_once(':').unwrap_or((owner, reference));
    // Owner names are exact, as in the repository lookup: alice and Alice are two accounts.
    let source = if source_owner == owner {
        repo.record.clone()
    } else {
        repository(ctx, source_owner, &repo.record.name)
            .await?
            .record
    };
    let related = same_fork_network(&ctx.state.pool, &repo.record, &source)
        .await
        .map_err(|_| {
            ctx.repo_error(
                &repo.view.chrome,
                StatusCode::SERVICE_UNAVAILABLE,
                "Comparison is unavailable.",
            )
        })?;
    if !related {
        return Err(ctx.repo_error(
            &repo.view.chrome,
            StatusCode::NOT_FOUND,
            "Comparison not found.",
        ));
    }
    Ok((source, reference.to_owned()))
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
        let mut target = compare_url(&repo.view.url(), &pick("base"), &pick("head"));
        if ctx.query.get("view").is_some_and(|view| view == "split") {
            target.push_str("?view=split");
        }
        return Ok(ctx.redirect(&target));
    } else {
        (default.clone(), default.clone())
    };
    let (names, _) = branches(&ctx, &repo.record).await?;
    let (base_repo, base_ref) = comparison_side(&ctx, &repo, &params["owner"], &base).await?;
    let (head_repo, head_ref) = comparison_side(&ctx, &repo, &params["owner"], &head).await?;
    // `alice:topic` in alice's own repository names the local branch `topic`.
    let base = if base_repo.id == repo.record.id {
        base_ref.clone()
    } else {
        base
    };
    let head = if head_repo.id == repo.record.id {
        head_ref.clone()
    } else {
        head
    };
    // A branch of either side is named in full, so one called `refs/topic` is not read as the
    // exact ref `refs/topic`.
    let mut sides = Vec::with_capacity(2);
    for (side, name) in [(&base_repo, base_ref), (&head_repo, head_ref)] {
        let branch = if side.id == repo.record.id {
            names.contains(&name)
        } else {
            branches(&ctx, side).await?.0.contains(&name)
        };
        sides.push(if branch {
            format!("refs/heads/{name}")
        } else {
            name
        });
    }
    let (base_ref, head_ref) = (sides.remove(0), sides.remove(0));
    let base_store = repo_store(&base_repo, ctx.state.cluster.as_ref());
    let head_store = repo_store(&head_repo, ctx.state.cluster.as_ref());
    let storage_error = |error| {
        crate::backend::api::repos::chrome::store_error(&ctx, Some(&repo.view.chrome), error)
    };
    let base_sha = base_store
        .commit(&base_ref)
        .await
        .map_err(storage_error)?
        .sha;
    let head_sha = head_store
        .commit(&head_ref)
        .await
        .map_err(storage_error)?
        .sha;
    let diff = if base == head && base_repo.id == head_repo.id {
        None
    } else {
        Some(diff_model(
            base_store
                .diff_from(&head_store, &base_sha, &head_sha)
                .await
                .map_err(storage_error)?,
            None,
        ))
    };
    // Only branch pairs of this repository have the native new-pull form as their target.
    let branch_pair = base_repo.id == repo.record.id
        && head_repo.id == repo.record.id
        && names.contains(&base)
        && names.contains(&head);
    let open_pull = if !branch_pair || base == head {
        None
    } else {
        db::pull_requests::records::find_open_pull_request_from_branches(
            &ctx.state.pool,
            repo.record.id,
            &base,
            &head,
        )
        .await
        .map_err(|_| {
            ctx.repo_error(
                &repo.view.chrome,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Pull requests are unavailable.",
            )
        })?
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
