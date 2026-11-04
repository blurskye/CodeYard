use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Redirect;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Form;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_db_security::advisories::Listing;
use codeyard_db_security::advisories::Scope;
use codeyard_model::advisories::Advisory;
use codeyard_model::advisories::Data;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::advisories::Detail;
use codeyard_webui::pages::repos::advisories::Draft;
use codeyard_webui::pages::repos::advisories::Page as AdvisoryPage;
use codeyard_webui::pages::repos::advisories::Row;
use codeyard_webui::pages::repos::advisories::Vulnerability;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

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
        vulnerabilities_json: serde_json::to_string_pretty(&data.vulnerabilities)
            .unwrap_or_else(|_| "[]".into()),
        ..Default::default()
    }
}
async fn render(
    ctx: &PageContext,
    loaded: codeyard_api_http::shared::repos::chrome::Loaded,
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
            page.can_administer = codeyard_db_security::advisories::can_administer(
                &ctx.state.pool,
                actor,
                loaded.repository.id,
            )
            .await?;
        }
        if let Some(ghsa) = ghsa {
            let advisory = codeyard_db_security::advisories::get(
                &ctx.state.pool,
                actor,
                loaded.repository.id,
                ghsa,
            )
            .await?
            .ok_or(codeyard_model::advisories::Refusal::Missing)?;
            let data = Data::from_row(&advisory)?;
            page.draft = draft(&data);
            let fork_url = if let Some(id) = advisory.private_fork_id {
                if let Some(fork) = codeyard_db_repos::repositories::records::get_repository_by_id(
                    &ctx.state.pool,
                    id,
                )
                .await?
                {
                    if codeyard_identity::repository_access::can_read(
                        &ctx.state.pool,
                        ctx.user.as_ref(),
                        &fork,
                    )
                    .await?
                    {
                        codeyard_db_repos::repositories::records::repository_owner_name(
                            &ctx.state.pool,
                            id,
                        )
                        .await?
                        .map(|owner| codeyard_webui::repository_path(&owner, &fork.name))
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
                .map_err(|_| codeyard_model::advisories::Refusal::Invalid)?
                .unwrap_or(1);
            anyhow::ensure!(
                (1..=5000).contains(&number),
                codeyard_model::advisories::Refusal::Invalid
            );
            page.page = number;
            let mut rows = codeyard_db_security::advisories::list(
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
        return match error.downcast_ref::<codeyard_model::advisories::Refusal>() {
            Some(codeyard_model::advisories::Refusal::Missing) => {
                ctx.error(StatusCode::NOT_FOUND, "Advisory not found.")
            }
            Some(codeyard_model::advisories::Refusal::Invalid) => {
                ctx.error(StatusCode::BAD_REQUEST, "Invalid advisory page.")
            }
            _ => codeyard_api_http::shared::repos::chrome::unavailable(ctx),
        };
    }
    if let Some(submitted) = submitted {
        page.draft = submitted;
    }
    codeyard_api_http::shared::repos::forms::with_status(
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
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
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
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
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
    if let Err(response) = codeyard_api_http::shared::repos::forms::authorize_post(&ctx, &form) {
        return response;
    }
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let actor = ctx.user.as_ref().unwrap().id;
    let can_administer = match codeyard_db_security::advisories::can_administer(
        &ctx.state.pool,
        actor,
        loaded.repository.id,
    )
    .await
    {
        Ok(allowed) => allowed,
        Err(_) => return codeyard_api_http::shared::repos::chrome::unavailable(&ctx),
    };
    if !can_administer
        || codeyard_api_http::shared::repos::forms::field(&form, "repository_id")
            .parse::<i64>()
            .ok()
            != Some(loaded.repository.id)
    {
        return ctx.error(StatusCode::FORBIDDEN, "Advisory administration denied.");
    }
    let authority = ctx.database_actor(actor);
    let op = codeyard_api_http::shared::repos::forms::field(&form, "op");
    let ghsa = codeyard_api_http::shared::repos::forms::field(&form, "ghsa");
    let draft = Draft {
        summary: codeyard_api_http::shared::repos::forms::field(&form, "summary"),
        description: codeyard_api_http::shared::repos::forms::field(&form, "description"),
        severity: codeyard_api_http::shared::repos::forms::field(&form, "severity"),
        vector: codeyard_api_http::shared::repos::forms::field(&form, "vector"),
        cve: codeyard_api_http::shared::repos::forms::field(&form, "cve"),
        package: codeyard_api_http::shared::repos::forms::field(&form, "package"),
        ecosystem: codeyard_api_http::shared::repos::forms::field(&form, "ecosystem"),
        range: codeyard_api_http::shared::repos::forms::field(&form, "range"),
        vulnerabilities_json: codeyard_api_http::shared::repos::forms::field(
            &form,
            "vulnerabilities_json",
        ),
    };
    let effect=async {
        let optional=|s:&String|if s.is_empty(){None}else{Some(s.clone())};
        let mut value=json!({"summary":draft.summary,"description":draft.description,"severity":optional(&draft.severity),"cvss_vector_string":optional(&draft.vector),"cve_id":optional(&draft.cve)});
        if op=="create"||(op=="save"&&form.contains_key("package")) {
            value["vulnerabilities"]=if draft.package.is_empty(){json!([])}else{json!([{"package":{"ecosystem":draft.ecosystem,"name":draft.package},"vulnerable_version_range":draft.range}])};
        }
        if op == "save" && form.contains_key("vulnerabilities_json") {
            value["vulnerabilities"] = serde_json::from_str::<serde_json::Value>(&draft.vulnerabilities_json).map_err(|_| codeyard_model::advisories::Refusal::Invalid)?;
        }
        match op.as_str() {
            "create"=>{
                return Ok(codeyard_db_security::advisories::create(&ctx.state.pool,authority.clone(),loaded.repository.id,&value,false).await?.ghsa_id);
            }
            "save"=>{codeyard_db_security::advisories::update(&ctx.state.pool,authority.clone(),loaded.repository.id,&ghsa,&value).await?;}
            "publish"|"close"|"draft"=>{let state=match op.as_str(){"publish"=>"published","close"=>"closed",_=>"draft"};codeyard_db_security::advisories::update(&ctx.state.pool,authority.clone(),loaded.repository.id,&ghsa,&json!({"state":state})).await?;}
            "cve"=>codeyard_db_security::advisories::request_cve(&ctx.state.pool,authority.clone(),loaded.repository.id,&ghsa).await?,
            "fork"=>{codeyard_repos_pulls::repositories::advisories::private_fork(&ctx.state,authority.clone(),&loaded.repository,&ghsa).await?;}
            _=>anyhow::bail!(codeyard_model::advisories::Refusal::Invalid),
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
            let status = if error.is::<codeyard_support::credentials::AccountRestricted>() {
                StatusCode::FORBIDDEN
            } else {
                match error.downcast_ref::<codeyard_model::advisories::Refusal>() {
                    Some(codeyard_model::advisories::Refusal::Denied) => StatusCode::FORBIDDEN,
                    Some(codeyard_model::advisories::Refusal::Missing) => StatusCode::NOT_FOUND,
                    Some(_) => StatusCode::UNPROCESSABLE_ENTITY,
                    None => StatusCode::SERVICE_UNAVAILABLE,
                }
            };
            render(
                &ctx,
                loaded,
                if ghsa.is_empty() { None } else { Some(&ghsa) },
                if matches!(op.as_str(), "save" | "create") {
                    Some(draft)
                } else {
                    None
                },
                Some(codeyard_api_http::shared::repos::chrome::safe_error(status).into()),
                status,
            )
            .await
        }
    }
}
