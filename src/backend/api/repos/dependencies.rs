//! Confidential dependency review; the domain and transactional DAL remain authoritative.
use super::{
    chrome::{self, Loaded},
    forms::{authorize_post, checkbox, field, with_status},
};
use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    db::{
        self,
        repositories::dependencies::{self as records, AlertState, Feature, Refusal},
    },
    git::error::ErrorKind,
    repositories::{
        access::AccessError,
        dependencies::{self, DependencyError},
    },
};
use axum::{
    extract::{DefaultBodyLimit, Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use codeyard_webui::{
    app::Page,
    pages::repos::{
        dependencies::{Alert, Draft, Package, Page as DependencyPage, Proposal},
        page::Page as RepoPage,
    },
};
use std::{collections::HashMap, sync::Arc};
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/security/dependencies", get(index))
        .route(
            "/:owner/:repo/security/dependencies/:feature/:intent",
            post(configure),
        )
        .layer(DefaultBodyLimit::max(256 * 1024))
}
fn failure(error: &anyhow::Error) -> (StatusCode, String) {
    if let Some(
        graph @ (db::dependency_graph::GraphError::Limit
        | db::dependency_graph::GraphError::Disabled),
    ) = error.downcast_ref::<db::dependency_graph::GraphError>()
    {
        return (StatusCode::UNPROCESSABLE_ENTITY, graph.to_string());
    }
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return (
            StatusCode::FORBIDDEN,
            "A live unrestricted credential is required".into(),
        );
    }
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied | Refusal::Missing) => (
            StatusCode::NOT_FOUND,
            "Dependency review unavailable to this account".into(),
        ),
        Some(Refusal::AlertsDisabled) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Enable dependency alerts under current administration first".into(),
        ),
        Some(Refusal::Stale) => (
            StatusCode::CONFLICT,
            "The installed version or authorization changed; review current values before retrying"
                .into(),
        ),
        Some(Refusal::Invalid(message)) => (StatusCode::UNPROCESSABLE_ENTITY, (*message).into()),
        Some(Refusal::Limit) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Dependency review limit reached".into(),
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Dependency storage unavailable; retry later".into(),
        ),
    }
}
fn domain_failure(error: DependencyError) -> (StatusCode, String) {
    match error {
        DependencyError::Access(AccessError::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Repository storage unavailable".into(),
        ),
        DependencyError::Access(_) => (
            StatusCode::NOT_FOUND,
            "Dependency administration unavailable to this account".into(),
        ),
        DependencyError::Store(error) => {
            let status = if error.is_write_denied() {
                StatusCode::FORBIDDEN
            } else {
                match error.kind {
                    ErrorKind::NotFound | ErrorKind::MissingPath => StatusCode::NOT_FOUND,
                    ErrorKind::Invalid => StatusCode::UNPROCESSABLE_ENTITY,
                    ErrorKind::Conflict => StatusCode::CONFLICT,
                    ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                }
            };
            (
                status,
                if status == StatusCode::SERVICE_UNAVAILABLE {
                    "Repository storage unavailable".into()
                } else {
                    error.message
                },
            )
        }
    }
}
fn page_number(ctx: &PageContext, key: &str) -> Result<usize, Response> {
    super::paging::number(ctx, key, 1, 1..=usize::MAX / 20)
}
fn state_name(state: &AlertState) -> &'static str {
    match state {
        AlertState::Open => "open",
        AlertState::Dismissed => "dismissed",
        AlertState::Resolved => "resolved",
    }
}
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
async fn model(ctx: &PageContext, loaded: Loaded, error: Option<String>, draft: Draft) -> Response {
    let Some(actor) = ctx.user.as_ref().map(|u| u.id) else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "Sign in to administer dependency security",
        );
    };
    let page = match page_number(ctx, "page") {
        Ok(p) => p,
        Err(r) => return r,
    };
    let packages_page = match page_number(ctx, "packages_page") {
        Ok(p) => p,
        Err(r) => return r,
    };
    let filter = ctx.query.get("state").map(String::as_str).unwrap_or("open");
    if !matches!(filter, "open" | "dismissed" | "resolved" | "all") {
        return ctx.error(StatusCode::BAD_REQUEST, "Invalid review state");
    }
    let pool = &ctx.state.pool;
    let repo = loaded.repository.id;
    let alerts = match dependencies::status(pool, actor, repo, Feature::Alerts).await {
        Ok(v) => v,
        Err(e) => {
            let (s, m) = domain_failure(e);
            return ctx.error(s, &m);
        }
    };
    let updates = match dependencies::status(pool, actor, repo, Feature::Updates).await {
        Ok(v) => v,
        Err(e) => {
            let (s, m) = domain_failure(e);
            return ctx.error(s, &m);
        }
    };
    let data = match records::page_snapshot(pool, ctx.database_actor(actor), repo).await {
        Ok(v) => v,
        Err(e) => {
            let (s, m) = failure(&e);
            return ctx.error(s, &m);
        }
    };
    let state = data.state;
    let snapshot = data.snapshot;
    let package_count = snapshot.packages.len();
    let mut packages: Vec<_> = snapshot
        .packages
        .iter()
        .skip((packages_page - 1) * 20)
        .take(21)
        .map(|p| Package {
            name: p.name.clone(),
            version: p.version.clone(),
            path: p.path.clone(),
        })
        .collect();
    let packages_more = packages.len() > 20;
    packages.truncate(20);
    let mut rows = Vec::new();
    for (key, alert) in snapshot
        .alerts
        .into_iter()
        .filter(|(_, a)| filter == "all" || state_name(&a.state) == filter)
        .skip((page - 1) * 20)
        .take(21)
    {
        let proposal = alert
            .update
            .as_ref()
            .and_then(|update| update.pull_request_id)
            .and_then(|id| data.proposals.get(&id))
            .map(|(number, state, merged)| Proposal {
                number: *number,
                state: if *merged {
                    "merged".into()
                } else {
                    state.clone()
                },
            });
        let pending = alert.update.is_some() && proposal.is_none();
        let review_state = state_name(&alert.state).into();
        let f = alert.finding;
        rows.push(Alert {
            key,
            package: Package {
                name: f.package.name,
                version: f.package.version,
                path: f.package.path,
            },
            advisory: f.advisory_id,
            summary: f.summary,
            details: f.details.chars().take(1024).collect(),
            severity: f.severity,
            url: f.url,
            fixed: f.fixed_versions,
            state: review_state,
            dismissal: alert.dismissal,
            manual: alert.manual_reason,
            proposal,
            pending,
        });
    }
    let more = rows.len() > 20;
    rows.truncate(20);
    let data = DependencyPage {
        alerts_enabled: alerts.enabled,
        updates_enabled: updates.enabled,
        paused: alerts.paused || updates.paused,
        status: state
            .as_ref()
            .map(|s| s.last_status.clone())
            .unwrap_or_else(|| "Not configured".into()),
        scan_sha: state.as_ref().and_then(|s| s.last_scan_sha.clone()),
        scan_date: state
            .as_ref()
            .and_then(|s| s.last_scan_at.as_ref().map(|d| d.to_rfc3339())),
        scan_error: state.and_then(|s| s.last_error),
        unsupported: snapshot.unsupported,
        package_count,
        packages,
        alerts: rows,
        page,
        packages_page,
        more,
        packages_more,
        filter: filter.into(),
        error,
        draft,
    };
    ctx.page_model(
        "Dependency security",
        Page::Repositories(RepoPage::Dependencies {
            repo: loaded.chrome,
            page: Box::new(data),
        }),
    )
}
async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    model(&ctx, loaded, None, Draft::default()).await
}
async fn configure(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, feature, intent)): Path<(String, String, String, String)>,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = authorize_post(&ctx, &form) {
        return r;
    }
    if field(&form, "repository_id") != loaded.repository.id.to_string() {
        return ctx.error(
            StatusCode::BAD_REQUEST,
            "This form belongs to another repository",
        );
    }
    let kind = match feature.as_str() {
        "alerts" => Feature::Alerts,
        "updates" => Feature::Updates,
        _ => return ctx.error(StatusCode::NOT_FOUND, "Unknown dependency feature"),
    };
    let enabled = match intent.as_str() {
        "enable" => true,
        "disable" => false,
        _ => return ctx.error(StatusCode::NOT_FOUND, "Unknown dependency action"),
    };
    let draft = Draft {
        intent: format!("{feature}-{intent}"),
        confirmation: field(&form, "confirmation"),
        consent: checkbox(&form, "consent"),
        ..Draft::default()
    };
    let result = if draft.confirmation != loaded.repository.name {
        Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Type the repository name exactly to confirm".into(),
        ))
    } else if enabled && loaded.repository.is_private && !draft.consent {
        Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Private dependency export consent is required".into(),
        ))
    } else {
        dependencies::set_enabled(
            &ctx.state.pool,
            ctx.state.cluster.as_ref(),
            ctx.user.as_ref().unwrap().id,
            db::users::accounts::OwnedAccountCredential::Session(
                crate::backend::api::http::session::extract_session_id(&ctx.headers)
                    .unwrap_or_default(),
            ),
            loaded.repository.id,
            kind,
            enabled,
            draft.consent,
        )
        .await
        .map_err(domain_failure)
    };
    match result {
        Ok(()) => {
            Redirect::to(&format!("{}/security/dependencies", loaded.chrome.base())).into_response()
        }
        Err((status, message)) => {
            with_status(model(&ctx, loaded, Some(message), draft).await, status)
        }
    }
}
