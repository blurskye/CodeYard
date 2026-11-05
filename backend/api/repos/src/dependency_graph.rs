use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::Json;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::dependency_graph::Dependency;
use codeyard_webui::pages::repos::dependency_graph::Page as GraphPage;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use std::sync::Arc;

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/:owner/:repo/dependency-graph", get(page))
        .cell_route(
            "/:owner/:repo/dependency-graph/sbom",
            get(download).layer(axum::middleware::from_fn(
                codeyard_api_http::middleware::private_api_response,
            )),
        )
}

fn inventory_failure(
    ctx: &PageContext,
    loaded: &codeyard_api_http::shared::repos::chrome::Loaded,
    error: RepoError,
) -> Response {
    if error.kind == ErrorKind::Invalid {
        return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &error.message);
    }
    codeyard_api_http::shared::repos::chrome::store_error(ctx, Some(&loaded.chrome), error)
}

async fn respond(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    owner: String,
    name: String,
    download: bool,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let reader = codeyard_db_security::dependency_graph::Reader::Viewer(
        ctx.user.as_ref().map(|user| ctx.database_actor(user.id)),
    );
    let reference = format!("refs/heads/{}", loaded.default_branch);
    if download {
        // The same typed document as the REST export, so submitted relationships survive.
        let snapshots =
            match codeyard_repos_pulls::repositories::dependency_graph::pinned_snapshots(
                &ctx.state,
                &reader,
                &loaded.repository,
                &reference,
            )
            .await
            {
                Ok(snapshots) => snapshots,
                Err(error) => return inventory_failure(&ctx, &loaded, error),
            };
        if let Err((status, _)) = codeyard_api_http::shared::repos::access::check_repo_read_access(
            &ctx.state.pool,
            &loaded.repository,
            &ctx.headers,
        )
        .await
        {
            return ctx.error(status, "Repository not found.");
        }
        let mut document =
            codeyard_model::dependency_graph::sbom(&format!("{owner}/{name}"), &snapshots);
        return Json(document["sbom"].take()).into_response();
    }
    let (sha, packages) = match codeyard_repos_pulls::repositories::dependency_graph::inventory(
        &ctx.state,
        &reader,
        &loaded.repository,
        &reference,
    )
    .await
    {
        Ok(inventory) => inventory,
        // A repository without commits has no dependencies yet.
        Err(error) if error.kind == ErrorKind::NotFound => (String::new(), Vec::new()),
        Err(error) => return inventory_failure(&ctx, &loaded, error),
    };
    if let Err((status, _)) = codeyard_api_http::shared::repos::access::check_repo_read_access(
        &ctx.state.pool,
        &loaded.repository,
        &ctx.headers,
    )
    .await
    {
        return ctx.error(status, "Repository not found.");
    }
    let number = match crate::paging::number(&ctx, "page", 1, 1..=crate::paging::MAX_PAGE) {
        Ok(number) => number,
        Err(response) => return response,
    };
    let per_page = crate::paging::DEFAULT_PER_PAGE;
    let offset = (number - 1) * per_page;
    let link = |page| format!("{}/dependency-graph?page={page}", loaded.chrome.base());
    let previous = (number > 1).then(|| link(number - 1));
    let next = (offset + per_page < packages.len()).then(|| link(number + 1));
    let packages = packages
        .into_iter()
        .skip(offset)
        .take(per_page)
        .map(|package| Dependency {
            name: package.name,
            version: package.version,
            manifest: package.manifest,
            ecosystem: package.ecosystem,
            scope: package.scope,
        })
        .collect();
    ctx.page_model(
        "Dependency graph",
        Page::Repositories(RepoPage::DependencyGraph {
            repo: loaded.chrome,
            page: GraphPage {
                sha,
                packages,
                previous,
                next,
            },
        }),
    )
}

async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    respond(state, headers, uri, owner, name, false).await
}
async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    respond(state, headers, uri, owner, name, true).await
}
