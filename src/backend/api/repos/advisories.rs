use super::{chrome, forms};
use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    db::{
        self,
        advisories::{self, Advisory, Data, Listing, Scope},
    },
    repositories::{access, advisories as workflow},
};
use crate::webui::{
    app::Page,
    pages::repos::{
        advisories::{Detail, Draft, Page as AdvisoryPage, Row, Vulnerability},
        page::Page as RepoPage,
    },
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/security/advisories", get(index))
        .route("/:owner/:repo/security/advisories/:ghsa", get(detail))
        .route("/:owner/:repo/_forms/advisory", post(submit))
}
fn row(record: &Advisory, data: &Data) -> Row {
    Row {
        ghsa: record.ghsa_id.clone(),
        summary: data.summary.clone(),
        state: record.state.clone(),
        severity: data
            .severity
            .clone()
            .unwrap_or_else(|| "unspecified".into()),
    }
}
fn draft(data: &Data) -> Draft {
    Draft {
        summary: data.summary.clone(),
        description: data.description.clone(),
        severity: if data.cvss_vector_string.is_some() {
            String::new()
        } else {
            data.severity.clone().unwrap_or_default()
        },
        vector: data.cvss_vector_string.clone().unwrap_or_default(),
        cve: data.cve_id.clone().unwrap_or_default(),
        ecosystem: "npm".into(),
        vulnerabilities_json: serde_json::to_string_pretty(&data.vulnerabilities).unwrap_or_else(|_| "[]".into()),
        ..Default::default()
    }
}
async fn render(
    ctx: &PageContext,
    loaded: chrome::Loaded,
    ghsa: Option<&str>,
    submitted: Option<Draft>,
    error: Option<String>,
    status: StatusCode,
) -> Response {
    let mut page = AdvisoryPage {
        can_administer: false,
        rows: Vec::new(),
        detail: None,
        draft: Draft {
            ecosystem: "npm".into(),
            ..Default::default()
        },
        error,
        more: false,
        page: 1,
    };
    let actor = ctx.user.as_ref().map(|u| u.id);
    let data = async {
        if let Some(actor) = actor {
            page.can_administer = advisories::can_administer(&ctx.state.pool, actor, loaded.repository.id).await?;
        }
        if let Some(ghsa) = ghsa {
            let advisory = advisories::get(&ctx.state.pool, actor, loaded.repository.id, ghsa)
                .await?
                .ok_or(advisories::Refusal::Missing)?;
            let data = Data::from_row(&advisory)?;
            page.draft = draft(&data);
            let fork_url = if let Some(id) = advisory.private_fork_id {
                if let Some(fork) =
                    db::repositories::records::get_repository_by_id(&ctx.state.pool, id).await?
                {
                    if access::can_read(&ctx.state.pool, ctx.user.as_ref(), &fork).await? {
                        db::repositories::records::repository_owner_name(&ctx.state.pool, id)
                            .await?
                            .map(|owner| crate::webui::repository_path(&owner, &fork.name))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let report_url = advisory
                .report_id
                .filter(|_| {
                    loaded.chrome.can_admin || (actor.is_some() && actor == advisory.author_id)
                })
                .map(|id| format!("{}/security/reports/{id}", loaded.chrome.base()));
            page.detail = Some(Detail {
                row: row(&advisory, &data),
                description: data.description.clone(),
                cve: data.cve_id.clone(),
                cve_requested: advisory.cve_requested_at.is_some() && data.cve_id.is_none(),
                fork_url,
                report_url,
                vulnerabilities: data
                    .vulnerabilities
                    .into_iter()
                    .map(|v| Vulnerability {
                        ecosystem: v.package.ecosystem,
                        name: v.package.name.unwrap_or_default(),
                        range: v.vulnerable_version_range.unwrap_or_default(),
                        patched: v.patched_versions.unwrap_or_default(),
                    })
                    .collect(),
            });
        } else {
            let number = ctx
                .query
                .get("page")
                .map(|v| v.parse::<usize>())
                .transpose()
                .map_err(|_| advisories::Refusal::Invalid)?
                .unwrap_or(1);
            anyhow::ensure!((1..=5000).contains(&number), advisories::Refusal::Invalid);
            page.page = number;
            let mut rows = advisories::list(
                &ctx.state.pool,
                actor,
                Scope::Repository(loaded.repository.id),
                &Listing {
                    limit: 21,
                    offset: ((number - 1) * 20) as i64,
                    ..Default::default()
                },
            )
            .await?;
            page.more = rows.len() > 20;
            rows.truncate(20);
            for advisory in rows {
                page.rows.push(row(&advisory, &Data::from_row(&advisory)?));
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = data {
        return match error.downcast_ref::<advisories::Refusal>() {
            Some(advisories::Refusal::Missing) => {
                ctx.error(StatusCode::NOT_FOUND, "Advisory not found.")
            }
            Some(advisories::Refusal::Invalid) => {
                ctx.error(StatusCode::BAD_REQUEST, "Invalid advisory page.")
            }
            _ => chrome::unavailable(ctx),
        };
    }
    if let Some(submitted) = submitted {
        page.draft = submitted;
    }
    forms::with_status(
        ctx.page_model(
            "Security advisories",
            Page::Repositories(RepoPage::Advisories {
                repo: loaded.chrome,
                page: Box::new(page),
            }),
        ),
        status,
    )
}
async fn index(
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
    render(&ctx, loaded, None, None, None, StatusCode::OK).await
}
async fn detail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name, ghsa)): Path<(String, String, String)>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match chrome::load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    render(&ctx, loaded, Some(&ghsa), None, None, StatusCode::OK).await
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
    let actor = ctx.user.as_ref().unwrap().id;
    let can_administer = match advisories::can_administer(&ctx.state.pool, actor, loaded.repository.id).await {
        Ok(allowed) => allowed,
        Err(_) => return chrome::unavailable(&ctx),
    };
    if !can_administer
        || forms::field(&form, "repository_id").parse::<i64>().ok() != Some(loaded.repository.id)
    {
        return ctx.error(StatusCode::FORBIDDEN, "Advisory administration denied.");
    }
    let authority = ctx.database_actor(actor);
    let op = forms::field(&form, "op");
    let ghsa = forms::field(&form, "ghsa");
    let draft = Draft {
        summary: forms::field(&form, "summary"),
        description: forms::field(&form, "description"),
        severity: forms::field(&form, "severity"),
        vector: forms::field(&form, "vector"),
        cve: forms::field(&form, "cve"),
        package: forms::field(&form, "package"),
        ecosystem: forms::field(&form, "ecosystem"),
        range: forms::field(&form, "range"),
        vulnerabilities_json: forms::field(&form, "vulnerabilities_json"),
    };
    let effect=async {
        let optional=|s:&String|if s.is_empty(){None}else{Some(s.clone())};
        let mut value=json!({"summary":draft.summary,"description":draft.description,"severity":optional(&draft.severity),"cvss_vector_string":optional(&draft.vector),"cve_id":optional(&draft.cve)});
        if op=="create"||(op=="save"&&form.contains_key("package")) {
            value["vulnerabilities"]=if draft.package.is_empty(){json!([])}else{json!([{"package":{"ecosystem":draft.ecosystem,"name":draft.package},"vulnerable_version_range":draft.range}])};
        }
        if op == "save" && form.contains_key("vulnerabilities_json") {
            value["vulnerabilities"] = serde_json::from_str::<serde_json::Value>(&draft.vulnerabilities_json).map_err(|_| advisories::Refusal::Invalid)?;
        }
        match op.as_str() {
            "create"=>{
                return Ok(advisories::create(&ctx.state.pool,authority.clone(),loaded.repository.id,&value,false).await?.ghsa_id);
            }
            "save"=>{advisories::update(&ctx.state.pool,authority.clone(),loaded.repository.id,&ghsa,&value).await?;}
            "publish"|"close"|"draft"=>{let state=match op.as_str(){"publish"=>"published","close"=>"closed",_=>"draft"};advisories::update(&ctx.state.pool,authority.clone(),loaded.repository.id,&ghsa,&json!({"state":state})).await?;}
            "cve"=>advisories::request_cve(&ctx.state.pool,authority.clone(),loaded.repository.id,&ghsa).await?,
            "fork"=>{workflow::private_fork(&ctx.state,authority.clone(),&loaded.repository,&ghsa).await?;}
            _=>anyhow::bail!(advisories::Refusal::Invalid),
        }
        Ok::<_,anyhow::Error>(ghsa.clone())
    }.await;
    match effect {
        Ok(ghsa) => Redirect::to(&format!(
            "{}/security/advisories/{ghsa}",
            loaded.chrome.base()
        ))
        .into_response(),
        Err(error) => {
            let status = if error.is::<db::users::accounts::AccountRestricted>() {
                StatusCode::FORBIDDEN
            } else { match error.downcast_ref::<advisories::Refusal>() {
                Some(advisories::Refusal::Denied) => StatusCode::FORBIDDEN,
                Some(advisories::Refusal::Missing) => StatusCode::NOT_FOUND,
                Some(_) => StatusCode::UNPROCESSABLE_ENTITY,
                None => StatusCode::SERVICE_UNAVAILABLE,
            } };
            render(
                &ctx,
                loaded,
                if ghsa.is_empty() { None } else { Some(&ghsa) },
                if matches!(op.as_str(), "save" | "create") {
                    Some(draft)
                } else {
                    None
                },
                Some(chrome::safe_error(status).into()),
                status,
            )
            .await
        }
    }
}
