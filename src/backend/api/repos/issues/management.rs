use crate::backend::{
    api::{
        http::context::PageContext,
        repos::{
            discussion::{repository, unavailable, RepositoryData, ResultPage},
            forms::{authorize_post, field},
        },
    },
    app::AppState,
    db::{self, issues::management as store},
};
use crate::webui::pages::repos::issues::{IssueManagement, RelatedIssue};
use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    match error.downcast_ref::<store::Refusal>() {
        Some(store::Refusal::Denied) => {
            ctx.error(StatusCode::FORBIDDEN, "Repository permission is required.")
        }
        Some(store::Refusal::Missing) => ctx.error(StatusCode::NOT_FOUND, "Issue not found."),
        Some(refusal) => ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &refusal.to_string()),
        None => unavailable(ctx),
    }
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
    let actor = ctx.require_user()?.id;
    let authority = || ctx.database_actor(actor);
    let issue = db::issues::get_issue(&ctx.state.pool, repo.record.id, number)
        .await
        .map_err(|_| unavailable(&ctx))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
    // A form from before a rename or transfer names another issue now.
    if form
        .get("issue_id")
        .is_some_and(|id| *id != issue.id.to_string())
    {
        return Err(ctx.error(
            StatusCode::CONFLICT,
            "This issue has changed since the page loaded. Reload it and try again.",
        ));
    }
    let base = format!("/{owner}/{name}/issues");
    let operation = field(&form, "operation");
    let result = match operation.as_str() {
        "pin" | "unpin" => {
            store::pin(
                &ctx.state.pool,
                authority(),
                repo.record.id,
                issue.id,
                operation == "pin",
            )
            .await
        }
        "delete" => {
            if field(&form, "confirm") != number.to_string() {
                return Err(ctx.error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Type the issue number to confirm deletion.",
                ));
            }
            store::delete(&ctx.state.pool, authority(), repo.record.id, issue.id)
                .await
                .map_err(|error| failure(&ctx, error))?;
            return Ok(ctx.redirect(&base));
        }
        "transfer" => {
            let target = db::repositories::records::get_repository(
                &ctx.state.pool,
                &owner,
                &field(&form, "destination"),
            )
            .await
            .map_err(|_| unavailable(&ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?;
            let moved = store::transfer(
                &ctx.state.pool,
                authority(),
                repo.record.id,
                issue.id,
                target.id,
            )
            .await
            .map_err(|error| failure(&ctx, error))?;
            return Ok(ctx.redirect(&format!("/{owner}/{}/issues/{}", target.name, moved.number)));
        }
        "duplicate" => {
            let duplicate = crate::backend::issues::references::resolve(
                &ctx.state.pool,
                actor,
                (&owner, &name),
                &field(&form, "duplicate"),
            )
            .await
            .map_err(|_| {
                ctx.error(
                    StatusCode::NOT_FOUND,
                    "Issue not found. Use a number or owner/repository#number.",
                )
            })?;
            crate::backend::issues::mutations::close_duplicate(
                &ctx.state.pool,
                &ctx.state.notification_broadcaster,
                authority(),
                repo.record.id,
                issue.id,
                duplicate.id,
            )
            .await
            .map_err(|error| {
                crate::backend::api::repos::discussion::api_error(
                    &ctx,
                    super::operation_error(error),
                )
            })?;
            Ok(())
        }
        "branch" => {
            let oid = crate::backend::repositories::store::repo_store(
                &repo.record,
                ctx.state.cluster.as_ref(),
            )
            .refs()
            .await
            .map_err(|_| unavailable(&ctx))?
            .get(&format!("refs/heads/{}", repo.record.default_branch))
            .cloned()
            .ok_or_else(|| {
                ctx.error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "The repository needs a default branch.",
                )
            })?;
            crate::backend::issues::branches::create(
                &ctx.state,
                authority(),
                &issue,
                &repo.record,
                &field(&form, "name"),
                &oid,
            )
            .await
            .map_err(|error| ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string()))?;
            return Ok(ctx.redirect(&format!("{base}/{number}")));
        }
        _ => {
            return Err(ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Choose an issue operation.",
            ))
        }
    };
    result.map_err(|error| failure(&ctx, error))?;
    Ok(ctx.redirect(&format!("{base}/{number}")))
}

pub(crate) async fn pinned(
    ctx: &PageContext,
    repo: &RepositoryData,
) -> ResultPage<Vec<RelatedIssue>> {
    Ok(store::pinned(&ctx.state.pool, repo.record.id)
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(|issue| RelatedIssue {
            id: issue.id,
            kind: "pinned".into(),
            title: issue.title,
            state: issue.state,
            reference: format!("#{}", issue.number),
            href: format!(
                "/{}/{}/issues/{}",
                repo.view.owner, repo.record.name, issue.number
            ),
        })
        .collect())
}

pub(crate) async fn load(
    ctx: &PageContext,
    repo: &RepositoryData,
    issue: &db::models::Issue,
) -> ResultPage<Box<IssueManagement>> {
    let pool = &ctx.state.pool;
    let mut controls = IssueManagement {
        pinned: store::pinned(pool, repo.record.id)
            .await
            .map_err(|_| unavailable(ctx))?
            .iter()
            .any(|p| p.id == issue.id),
        ..Default::default()
    };
    let linked = store::linked_branches(pool, issue.id)
        .await
        .map_err(|_| unavailable(ctx))?;
    let mut wanted: HashMap<i64, HashSet<String>> = HashMap::new();
    for branch in &linked {
        wanted
            .entry(branch.repo_id)
            .or_default()
            .insert(format!("refs/heads/{}", branch.name));
    }
    let mut targets = HashMap::from([(
        repo.record.id,
        Some((repo.record.clone(), repo.view.owner.clone())),
    )]);
    let mut references = HashMap::new();
    let mut branches = Vec::new();
    let mut closing_pulls = Vec::new();
    let mut duplicate_link = None;
    for branch in linked {
        let Some((target, owner)) = cached_target(ctx, branch.repo_id, &mut targets).await? else {
            continue;
        };
        if let std::collections::hash_map::Entry::Vacant(slot) = references.entry(target.id) {
            let refs =
                crate::backend::repositories::store::repo_store(target, ctx.state.cluster.as_ref())
                    .refs()
                    .await
                    .map_err(|_| unavailable(ctx))?;
            let wanted = wanted.remove(&target.id).unwrap_or_default();
            let available: HashSet<_> = refs
                .into_keys()
                .filter(|reference| wanted.contains(reference))
                .collect();
            slot.insert(available);
        }
        if !references[&target.id].contains(&format!("refs/heads/{}", branch.name)) {
            continue;
        }
        branches.push((
            target.id,
            (
                branch.name.clone(),
                crate::webui::pages::repos::ref_path::file_url(
                    &format!("/{owner}/{}", target.name),
                    "tree",
                    &branch.name,
                    "",
                ),
            ),
        ));
    }
    for pull in crate::backend::issues::closing::references(
        pool,
        ctx.user.as_ref(),
        issue,
        &repo.view.owner,
        &repo.record.name,
        &crate::backend::api::http::config::link_origin(),
    )
    .await
    .map_err(|_| unavailable(ctx))?
    {
        let Some((target, owner)) = cached_target(ctx, pull.repo_id, &mut targets).await? else {
            continue;
        };
        closing_pulls.push((
            target.id,
            (
                format!("{} #{}", pull.title, pull.number),
                format!("/{owner}/{}/pulls/{}", target.name, pull.number),
            ),
        ));
    }
    if let Some(duplicate) = store::duplicate(pool, issue.id)
        .await
        .map_err(|_| unavailable(ctx))?
    {
        if let Some((target, owner)) = cached_target(ctx, duplicate.repo_id, &mut targets).await? {
            duplicate_link = Some((
                target.id,
                (
                    format!("#{}", duplicate.number),
                    format!("/{owner}/{}/issues/{}", target.name, duplicate.number),
                ),
            ));
        }
    }
    // Cached policy is not a publication permit. Recheck each distinct target after
    // all Git and cross-repository enrichment, including initially public targets.
    let mut visible = HashSet::new();
    for (id, target) in &targets {
        if let Some((target, _)) = target {
            match crate::backend::api::repos::access::check_repo_read_access(
                pool,
                target,
                &ctx.headers,
            )
            .await
            {
                Ok(_) => {
                    visible.insert(*id);
                }
                Err((status, _)) if status.is_server_error() => return Err(unavailable(ctx)),
                Err(_) => {}
            }
        }
    }
    controls.branches = branches
        .into_iter()
        .filter_map(|(id, link)| visible.contains(&id).then_some(link))
        .collect();
    controls.closing_pulls = closing_pulls
        .into_iter()
        .filter_map(|(id, link)| visible.contains(&id).then_some(link))
        .collect();
    controls.duplicate =
        duplicate_link.and_then(|(id, link)| visible.contains(&id).then_some(link));
    Ok(Box::new(controls))
}

type TargetCache = HashMap<i64, Option<(db::models::Repository, String)>>;

async fn cached_target<'a>(
    ctx: &PageContext,
    id: i64,
    targets: &'a mut TargetCache,
) -> ResultPage<Option<&'a (db::models::Repository, String)>> {
    if let std::collections::hash_map::Entry::Vacant(slot) = targets.entry(id) {
        let target = if let Some(target) = readable_target(ctx, id).await? {
            let owner = db::repositories::records::repository_owner_name(&ctx.state.pool, id)
                .await
                .map_err(|_| unavailable(ctx))?
                .ok_or_else(|| unavailable(ctx))?;
            Some((target, owner))
        } else {
            None
        };
        slot.insert(target);
    }
    Ok(targets.get(&id).and_then(Option::as_ref))
}

async fn readable_target(ctx: &PageContext, id: i64) -> ResultPage<Option<db::models::Repository>> {
    let target = db::repositories::records::get_repository_by_id(&ctx.state.pool, id)
        .await
        .map_err(|_| unavailable(ctx))?;
    if let Some(target) = target {
        if crate::backend::repositories::access::can_read(
            &ctx.state.pool,
            ctx.user.as_ref(),
            &target,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        {
            return Ok(Some(target));
        }
    }
    Ok(None)
}
