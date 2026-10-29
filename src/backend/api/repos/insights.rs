//! Repository insights pages. Each reads what is already recorded: the database's daily counts
//! and workflow runs, the contributors and code frequency Git keeps cached, and the forks.
mod traffic;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::activity::forks::visible_forks;
use crate::backend::api::repos::chrome::{self, Loaded};
use crate::backend::api::repos::code::contributors::contributor_accounts;
use crate::backend::api::repos::responses::repository_owners;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::repo_store;
use crate::webui::app::Page;
use crate::webui::components::charts::{normalize_days, DailyActivity};
use crate::webui::pages::repos::insights::{
    Contributor, DependencyRow, Fork, Page as InsightsPage, Week, WorkflowUsage, FORKS_PER_PAGE,
};
use crate::webui::pages::repos::page::Page as RepoPage;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use axum::{routing::get, Router};
use std::sync::Arc;

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/insights", get(pulse))
        .route("/:owner/:repo/insights/activity", get(ref_activity))
        .route("/:owner/:repo/insights/traffic", get(traffic::page))
        .route("/:owner/:repo/insights/contributors", get(contributors))
        .route("/:owner/:repo/insights/code-frequency", get(code_frequency))
        .route("/:owner/:repo/insights/actions", get(actions))
        .route("/:owner/:repo/insights/forks", get(forks))
        .route("/:owner/:repo/insights/dependencies", get(dependencies))
        .route("/:owner/:repo/insights/community", get(community))
}

async fn show(ctx: &PageContext, loaded: Loaded, title: &str, page: InsightsPage) -> Response {
    if let Err((status, _)) = super::access::check_repo_read_access(&ctx.state.pool, &loaded.repository, &ctx.headers).await {
        return ctx.error(status, "Repository not found.");
    }
    ctx.page_model(
        title,
        Page::Repositories(RepoPage::Insights {
            repo: loaded.chrome,
            page,
        }),
    )
}

/// The request's context and the repository, read from the database only.
async fn open(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: &Uri,
    owner: &str,
    name: &str,
) -> Result<(PageContext, Loaded), Response> {
    let ctx = PageContext::load(state, headers, uri).await?;
    let loaded = chrome::load_database(&ctx, owner, name).await?;
    Ok((ctx, loaded))
}

async fn ref_activity(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Query(input): axum::extract::Query<
        crate::backend::repositories::activity::Query,
    >,
) -> Response {
    use crate::backend::repositories::activity;
    use crate::webui::pages::repos::insights::activity::{Page as ActivityPage, Row};
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let page = match activity::page(&ctx.state.pool, loaded.repository.id, &input).await {
        Ok(page) => page,
        Err(error) => {
            return match error.downcast_ref::<activity::InvalidQuery>() {
                Some(invalid) => ctx.error(
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    &invalid.to_string(),
                ),
                None => chrome::unavailable(&ctx),
            }
        }
    };
    let base = format!("{}/insights/activity", loaded.chrome.base());
    let previous = page.previous.as_ref().map(|cursor| {
        format!(
            "{base}?{}",
            activity::query_string(&input, Some(cursor), None)
        )
    });
    let next = page.next.as_ref().map(|cursor| {
        format!(
            "{base}?{}",
            activity::query_string(&input, None, Some(cursor))
        )
    });
    let rows = page
        .rows
        .into_iter()
        .map(|row| Row {
            reference: row.reference,
            kind: row.activity_type.replace('_', " "),
            actor: row.actor,
            date: row.created_at.to_rfc3339(),
            commit: if row.after_sha.bytes().all(|ch| ch == b'0') {
                row.before_sha
            } else {
                row.after_sha
            },
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Repository activity",
        InsightsPage::Activity {
            page: Box::new(ActivityPage {
                rows,
                previous,
                next,
                reference: input.reference.unwrap_or_default(),
                actor: input.actor.unwrap_or_default(),
                kind: input.activity_type.unwrap_or_default(),
                direction: input.direction.unwrap_or_else(|| "desc".into()),
                time_period: input.time_period.unwrap_or_default(),
            }),
        },
    ).await
}

async fn dependencies(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match chrome::load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let reference = ctx.query.get("ref").cloned().unwrap_or_else(|| format!("refs/heads/{}", loaded.default_branch));
    if !reference.starts_with("refs/heads/") || !crate::backend::git::ref_names::is_canonical_ref(&reference) {
        return ctx.error(axum::http::StatusCode::BAD_REQUEST, "Invalid dependency reference.");
    }
    let revision = ctx.query.get("sha").cloned();
    if revision.as_deref().is_some_and(|sha| !crate::backend::git::ref_names::is_full_commit_id(sha)) {
        return ctx.error(axum::http::StatusCode::BAD_REQUEST, "Invalid dependency revision.");
    }
    let snapshots = match if let Some(sha) = revision.as_deref() {
        db::dependency_graph::snapshots(&ctx.state.pool, loaded.repository.id, Some(&reference), Some(&sha.to_ascii_lowercase())).await
    } else {
        db::dependency_graph::latest_snapshots(&ctx.state.pool, loaded.repository.id, &reference).await
    } {
        Ok(snapshots) => snapshots,
        Err(_) => return chrome::unavailable(&ctx),
    };
    let page = ctx
        .query
        .get("page")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, 10_000);
    let revision = revision.or_else(|| snapshots.first().map(|snapshot| snapshot.sha.clone()));
    let packages = crate::backend::dependency_graph::packages(&snapshots);
    let offset = (page as usize - 1) * 30;
    let has_more = offset + 30 < packages.len();
    let rows = packages
        .into_iter()
        .skip(offset)
        .take(30)
        .map(|p| DependencyRow {
            manifest: p.manifest,
            name: p.name,
            version: p.version,
            ecosystem: p.ecosystem,
            scope: p.scope,
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Dependency graph",
        InsightsPage::Dependencies {
            rows,
            page,
            has_more,
            reference,
            revision,
        },
    ).await
}

async fn pulse(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let days = normalize_days(ctx.query.get("days").map(String::as_str));
    let points = match db::repository_stats::list_repo_activity_points(
        &ctx.state.pool,
        loaded.repository.id,
        days,
    )
    .await
    {
        Ok(points) => points,
        Err(_) => return chrome::unavailable(&ctx),
    };
    let activity = points
        .into_iter()
        .map(|point| DailyActivity {
            date: point.day.to_string(),
            commits: point.commits,
            pulls_opened: point.pulls_opened,
            pulls_merged: point.pulls_merged,
            issues_opened: point.issues_opened,
            issues_closed: point.issues_closed,
            runs: point.runs,
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Pulse",
        InsightsPage::Pulse { days, activity },
    ).await
}

async fn community(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    use crate::webui::pages::repos::{insights::CommunityCheck, ref_path::file_url};
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let profile = match crate::backend::repositories::community::load(
        &ctx.state.pool,
        &loaded.repository,
        &owner,
        ctx.state.cluster.as_ref(),
    )
    .await
    {
        Ok(profile) => profile,
        Err(error) => return chrome::store_error(&ctx, Some(&loaded.chrome), error),
    };
    let mut checks = vec![CommunityCheck {
        name: "Description".to_owned(),
        present: profile
            .description
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty()),
        href: None,
    }];
    for (name, file, folder) in [
        ("README", profile.readme.as_ref(), false),
        ("License", profile.license.as_ref(), false),
        ("Code of conduct", profile.conduct.as_ref(), false),
        ("Contributing guide", profile.contributing.as_ref(), false),
        (
            "Issue templates",
            profile
                .issue_template
                .as_ref()
                .or(profile.issue_templates.as_ref()),
            profile.issue_template.is_none(),
        ),
        (
            "Pull request templates",
            profile
                .pull_request_template
                .as_ref()
                .or(profile.pull_request_templates.as_ref()),
            profile.pull_request_template.is_none(),
        ),
    ] {
        checks.push(CommunityCheck {
            name: name.to_owned(),
            present: file.is_some(),
            href: file.map(|file| {
                file_url(
                    &format!("/{}/{}", file.owner, file.repository),
                    if folder { "tree" } else { "blob" },
                    &file.reference,
                    &file.path,
                )
            }),
        });
    }
    show(
        &ctx,
        loaded,
        "Community standards",
        InsightsPage::Community {
            health_percentage: profile.health_percentage(),
            checks,
        },
    ).await
}

async fn contributors(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let found = match repo_store(&loaded.repository, ctx.state.cluster.as_ref())
        .contributors()
        .await
    {
        Ok(found) => found,
        Err(error) => return chrome::store_error(&ctx, Some(&loaded.chrome), error),
    };
    let Ok(accounts) = contributor_accounts(&ctx.state.pool, &found).await else {
        return chrome::unavailable(&ctx);
    };
    let mut rows: Vec<Contributor> = found
        .into_iter()
        .map(|contributor| Contributor {
            username: accounts
                .get(&contributor.email)
                .map(|user| user.username.clone()),
            name: contributor.name,
            commits: i64::from(contributor.commits),
        })
        .collect();
    rows.sort_by(|a, b| b.commits.cmp(&a.commits).then_with(|| a.name.cmp(&b.name)));
    show(
        &ctx,
        loaded,
        "Contributors",
        InsightsPage::Contributors { rows },
    ).await
}

async fn code_frequency(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    // The chart follows the default branch, so this page needs Git for its name.
    let loaded = match chrome::load(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let weeks = match repo_store(&loaded.repository, ctx.state.cluster.as_ref())
        .code_frequency(&loaded.default_branch)
        .await
    {
        Ok(weeks) => weeks,
        Err(error) => return chrome::store_error(&ctx, Some(&loaded.chrome), error),
    };
    let weeks = weeks
        .into_iter()
        .map(|week| Week {
            week: week.week,
            additions: i64::try_from(week.additions).unwrap_or(i64::MAX),
            deletions: i64::try_from(week.deletions).unwrap_or(i64::MAX),
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Code frequency",
        InsightsPage::CodeFrequency { weeks },
    ).await
}

async fn actions(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let days = normalize_days(ctx.query.get("days").map(String::as_str));
    let usage = match db::repository_stats::list_workflow_usage(
        &ctx.state.pool,
        loaded.repository.id,
        days,
    )
    .await
    {
        Ok(usage) => usage,
        Err(_) => return chrome::unavailable(&ctx),
    };
    let workflows = usage
        .into_iter()
        .map(|usage| WorkflowUsage {
            path: usage.workflow_path,
            runs: usage.runs,
            succeeded: usage.succeeded,
            failed: usage.failed,
            average_seconds: usage.average_seconds.map(|seconds| seconds.round() as i64),
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Actions usage",
        InsightsPage::Actions { days, workflows },
    ).await
}

async fn forks(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let page = ctx
        .query
        .get("page")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, 10_000);
    let viewer = ctx.user.as_ref().map(|user| user.id);
    let session = crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let offset = (page - 1) * FORKS_PER_PAGE;
    let Ok((found, has_more)) = visible_forks(
        &ctx.state.pool,
        viewer,
        &session,
        loaded.repository.id,
        FORKS_PER_PAGE,
        offset,
    )
    .await
    else {
        return chrome::unavailable(&ctx);
    };
    let ids: Vec<i64> = found.iter().map(|fork| fork.id).collect();
    let Ok(mut owners) = repository_owners(&ctx.state.pool, &ids).await else {
        return chrome::unavailable(&ctx);
    };
    let viewer = match crate::backend::api::http::session::get_optional_user(&ctx.state.pool, &ctx.headers).await {
        Ok(user) => user.map(|user| user.id),
        Err(_) => return chrome::unavailable(&ctx),
    };
    let readable = match db::repositories::access::readable_repository_ids(&ctx.state.pool, viewer, &ids).await {
        Ok(ids) => ids,
        Err(_) => return chrome::unavailable(&ctx),
    };
    let rows = found
        .into_iter()
        .filter(|fork| readable.contains(&fork.id))
        .filter_map(|fork| {
            Some(Fork {
                owner: owners.remove(&fork.id)?,
                name: fork.name,
                description: fork.description.unwrap_or_default(),
                stars: fork.stars_count,
                updated_at: fork.updated_at.to_rfc3339(),
            })
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Forks",
        InsightsPage::Forks {
            rows,
            page,
            has_more,
        },
    ).await
}
