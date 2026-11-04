use super::{chrome, forms};
use crate::backend::{
    api::http::context::PageContext, app::AppState, db::pages, repositories::store::repo_store,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use codeyard_webui::{
    app::Page,
    pages::repos::{
        page::Page as RepoPage,
        pages::{Build, Page as PagesPage, Settings},
    },
};
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/settings/pages", get(page))
        .route("/:owner/:repo/_forms/pages", post(submit))
}
async fn render(
    ctx: &PageContext,
    loaded: chrome::Loaded,
    submitted: Option<Settings>,
    error: Option<String>,
    status: StatusCode,
) -> Response {
    let session = crate::backend::api::http::session::extract_session_id(&ctx.headers);
    let reader = crate::backend::db::repositories::credentials::Reader {
        user: ctx.user.as_ref().map(|user| user.id),
        credential: session
            .as_deref()
            .map(crate::backend::db::users::accounts::AccountCredential::Session),
    };
    let site = match pages::site(&ctx.state.pool, &reader, loaded.repository.id).await {
        Ok(site) => site,
        Err(_) => return chrome::unavailable(ctx),
    };
    let exists = site.is_some();
    let settings = submitted.unwrap_or_else(|| match site {
        Some(site) => Settings {
            build_type: site.build_type,
            branch: site.branch.unwrap_or_default(),
            path: site.path.unwrap_or_else(|| "/".into()),
            cname: site.cname.unwrap_or_default(),
            https_enforced: site.https_enforced,
            public: site.public,
        },
        None => Settings {
            build_type: "legacy".into(),
            branch: loaded.default_branch,
            path: "/".into(),
            ..Default::default()
        },
    });
    let builds = match pages::builds(&ctx.state.pool, &reader, loaded.repository.id, 0, 30).await {
        Ok(rows) => rows
            .into_iter()
            .map(|row| Build {
                commit: row.commit_sha,
                status: row.status,
                error: row.error.unwrap_or_default(),
            })
            .collect(),
        Err(_) => return chrome::unavailable(ctx),
    };
    forms::with_status(
        ctx.page_model(
            "Pages",
            Page::Repositories(RepoPage::Pages {
                repo: loaded.chrome,
                page: PagesPage {
                    exists,
                    settings,
                    builds,
                    error,
                },
            }),
        ),
        status,
    )
}
async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match chrome::load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if !loaded.chrome.can_admin {
        return ctx.error(StatusCode::FORBIDDEN, "Pages administration denied.");
    }
    render(&ctx, loaded, None, None, StatusCode::OK).await
}
async fn submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = forms::authorize_post(&ctx, &form) {
        return response;
    }
    let loaded = match chrome::load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if !loaded.chrome.can_admin
        || form
            .get("repository_id")
            .and_then(|id| id.parse::<i64>().ok())
            != Some(loaded.repository.id)
    {
        return ctx.error(StatusCode::FORBIDDEN, "Pages administration denied.");
    }
    let actor = ctx.user.as_ref().unwrap().id;
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let credential = crate::backend::db::users::accounts::AccountCredential::Session(&session);
    let reader = crate::backend::db::repositories::credentials::Reader {
        user: Some(actor),
        credential: Some(crate::backend::db::users::accounts::AccountCredential::Session(&session)),
    };
    let settings = Settings {
        build_type: forms::field(&form, "build_type"),
        branch: forms::field(&form, "branch"),
        path: forms::field(&form, "path"),
        cname: forms::field(&form, "cname"),
        https_enforced: forms::checkbox(&form, "https_enforced"),
        public: forms::checkbox(&form, "public"),
    };
    let operation=async {
        match forms::field(&form,"op").as_str() {
            "delete"=>pages::delete(&ctx.state.pool,actor,&credential,loaded.repository.id).await?,
            "build"=>{
                let site=pages::site(&ctx.state.pool,&reader,loaded.repository.id).await?.ok_or(pages::Refusal::Missing)?;
                let branch=site.branch.ok_or(pages::Refusal::Invalid)?;
                let commit=repo_store(&loaded.repository,ctx.state.cluster.as_ref()).commit(&format!("refs/heads/{branch}")).await?;
                pages::request_build(&ctx.state.pool,actor,&credential,loaded.repository.id,&commit.sha).await?;
            }
            "save"=>{
                let create=pages::site(&ctx.state.pool,&reader,loaded.repository.id).await?.is_none();
                let mut value=json!({"build_type":settings.build_type,"cname":if settings.cname.is_empty(){None}else{Some(&settings.cname)},"https_enforced":settings.https_enforced,"public":settings.public});
                if settings.build_type == "legacy" && !settings.branch.is_empty() {value["source"]=json!({"branch":settings.branch,"path":settings.path});}
                let update=pages::Update::parse(&value,create)?;
                if let Some(source)=&update.source {repo_store(&loaded.repository,ctx.state.cluster.as_ref()).commit(&format!("refs/heads/{}",source.branch)).await?;}
                pages::save(&ctx.state.pool,actor,&credential,loaded.repository.id,&update,create).await?;
            }
            _=>anyhow::bail!(pages::Refusal::Invalid),
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    match operation {
        Ok(()) => axum::response::IntoResponse::into_response(Redirect::to(&format!(
            "{}/settings/pages",
            loaded.chrome.base()
        ))),
        Err(error) => {
            let status = match error.downcast_ref::<pages::Refusal>() {
                Some(pages::Refusal::Denied) => StatusCode::FORBIDDEN,
                Some(pages::Refusal::Missing) => StatusCode::NOT_FOUND,
                Some(_) => StatusCode::UNPROCESSABLE_ENTITY,
                None if error.is::<codeyard_support::credentials::AccountRestricted>() => {
                    StatusCode::FORBIDDEN
                }
                None => match error.downcast_ref::<codeyard_git::error::RepoError>() {
                    Some(error)
                        if matches!(
                            error.kind,
                            codeyard_git::error::ErrorKind::NotFound
                                | codeyard_git::error::ErrorKind::Invalid
                        ) =>
                    {
                        StatusCode::UNPROCESSABLE_ENTITY
                    }
                    _ => StatusCode::SERVICE_UNAVAILABLE,
                },
            };
            render(
                &ctx,
                loaded,
                (forms::field(&form, "op") == "save").then_some(settings),
                Some(chrome::safe_error(status).into()),
                status,
            )
            .await
        }
    }
}
