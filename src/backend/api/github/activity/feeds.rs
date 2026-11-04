use crate::backend::api::github::activity::auth;

use crate::backend::api::github::activity::Error;
use crate::backend::api::github::activity::Result;
use crate::backend::app::AppState;

use codeyard_db_repos::events::EventScope;
use codeyard_db_repos::events::EventViewer;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json, Router,
};
use codeyard_webui::{components::escape_html, encode_component};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, fmt::Write, sync::Arc};

fn origin() -> String {
    codeyard_model::http_config::link_origin()
}
async fn discovery(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Response> {
    let viewer = auth::authenticate(&state, &headers, false).await?;
    let timeline = format!("{}/api/activity-feeds/public.atom", origin());
    let user = format!("{}/api/activity-feeds/users/{{user}}", origin());
    let mut value = json!({"timeline_url":timeline,"user_url":user,"_links":{"timeline":{"href":timeline,"type":"application/atom+xml"},"user":{"href":user,"type":"application/atom+xml"}}});
    if let Some(viewer) = viewer {
        let token =
            crate::backend::identity::tokens::activity_feed_token(&state.pool, viewer.token.id)
                .await
                .map_err(|e| match e {
                    crate::backend::identity::tokens::TokenError::Denied => auth::required(),
                    _ => Error::unavailable(()),
                })?;
        let username = encode_component(&viewer.user.username);
        for (key, url) in [
            (
                "current_user_public",
                format!("{}/api/activity-feeds/users/{username}", origin()),
            ),
            (
                "current_user",
                format!(
                    "{}/api/activity-feeds/private/{username}/received?token={token}",
                    origin()
                ),
            ),
            (
                "current_user_actor",
                format!(
                    "{}/api/activity-feeds/private/{username}/actor?token={token}",
                    origin()
                ),
            ),
            (
                "current_user_organization",
                format!(
                    "{}/api/activity-feeds/private/{username}/orgs/{{org}}?token={token}",
                    origin()
                ),
            ),
        ] {
            value[format!("{key}_url")] = json!(url);
            value["_links"][key] = json!({"href":url,"type":"application/atom+xml"});
        }
        let mut organizations = Vec::new();
        let mut after = 0;
        loop {
            let rows = crate::backend::api::github::orgs::outcome(
                codeyard_db_orgs::organizations::profiles::list_actor_organizations(
                    &state.pool,
                    viewer.user.id,
                    codeyard_db_orgs::organizations::OrganizationPage::new(after, 100)
                        .map_err(Error::unavailable)?,
                )
                .await
                .map_err(Error::unavailable)?,
            )?;
            let more = rows.len() == 100;
            if let Some(row) = rows.last() {
                after = row.id;
            }
            organizations.extend(rows);
            if !more {
                break;
            }
        }
        {
            let urls: Vec<_> = organizations
                .into_iter()
                .map(|org| {
                    format!(
                        "{}/api/activity-feeds/private/{username}/orgs/{}?token={token}",
                        origin(),
                        encode_component(&org.name)
                    )
                })
                .collect();
            value["current_user_organization_urls"] = json!(urls);
            value["_links"]["current_user_organizations"] = json!(urls
                .into_iter()
                .map(|href| json!({"href":href,"type":"application/atom+xml"}))
                .collect::<Vec<_>>());
        }
    }
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    Ok(response)
}
#[derive(Default, Deserialize)]
struct FeedQuery {
    token: Option<String>,
}

fn xml(value: &str) -> String {
    escape_html(
        &value
            .chars()
            .filter(|c| {
                matches!(c, '\t' | '\n' | '\r')
                    || (*c >= ' ' && *c != '\u{fffe}' && *c != '\u{ffff}')
            })
            .collect::<String>(),
    )
}
fn atom(title: &str, id: &str, updated: &str, values: &[Value]) -> String {
    let mut output=format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><feed xmlns=\"http://www.w3.org/2005/Atom\"><id>{}</id><title>{}</title><updated>{}</updated>",xml(id),xml(title),xml(values.first().and_then(|v|v["created_at"].as_str()).unwrap_or(updated)));
    for value in values {
        let actor = value["actor"]["login"].as_str().unwrap_or("");
        let name = value["repo"]["name"].as_str().unwrap_or("");
        let kind = value["type"].as_str().unwrap_or("");
        let action = value["payload"]["action"].as_str().unwrap_or("");
        let title = format!("{actor}: {kind} {action} — {name}");
        let target = ["comment", "pull_request", "issue", "release", "forkee"]
            .iter()
            .find_map(|key| value["payload"][key]["html_url"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}/{name}", origin()));
        let content = ["comment", "pull_request", "issue", "release"]
            .iter()
            .find_map(|key| value["payload"][key]["body"].as_str())
            .unwrap_or("");
        // Atom IDs are permanent identifiers, not advertised event-detail resources.
        let entry_id = format!(
            "{}/api/activity-feeds/public.atom#event-{}",
            origin(),
            value["id"].as_str().unwrap_or("")
        );
        write!(output,"<entry><id>{}</id><title>{}</title><updated>{}</updated><author><name>{}</name></author><link rel=\"alternate\" href=\"{}\"/><content type=\"text\">{}</content></entry>",xml(&entry_id),xml(&title),xml(value["created_at"].as_str().unwrap_or(updated)),xml(actor),xml(&target),xml(content)).unwrap();
    }
    output.push_str("</feed>");
    output
}
async fn feed(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    params: Option<Path<HashMap<String, String>>>,
    Query(query): Query<FeedQuery>,
) -> Result<Response> {
    let params = params.map(|Path(p)| p).unwrap_or_default();
    let config = codeyard_db_cluster::node_config::get_node_configuration(&state.pool)
        .await
        .map_err(Error::unavailable)?
        .filter(|c| c.setup_completed)
        .ok_or_else(Error::missing)?;
    let (viewer, scope, public, title, id) = if let Some(kind) =
        params.get("kind").or_else(|| params.get("org"))
    {
        let token = query.token.as_deref().ok_or_else(Error::missing)?;
        let feed = crate::backend::identity::tokens::activity_feed_user(&state.pool, token)
            .await
            .map_err(|e| {
                if e == crate::backend::identity::tokens::TokenError::Unavailable {
                    Error::unavailable(())
                } else {
                    Error::missing()
                }
            })?;
        let user = &feed.user;
        if params.get("username") != Some(&user.username) {
            return Err(Error::missing());
        }
        let scope = if let Some(org) = params.get("org") {
            let org = codeyard_db_orgs::organizations::profiles::get_public_organization(
                &state.pool,
                org,
            )
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
            if !matches!(
                codeyard_db_orgs::organizations::profiles::get_organization_metadata(
                    &state.pool,
                    user.id,
                    org.id
                )
                .await
                .map_err(Error::unavailable)?,
                codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(_)
            ) {
                return Err(Error::missing());
            }
            EventScope::Organization(org.id)
        } else {
            match kind.as_str() {
                "actor" => EventScope::Actor(user.id),
                "received" => EventScope::Received(user.id),
                _ => return Err(Error::missing()),
            }
        };
        let username = encode_component(&user.username);
        // Never copy the secret query parameter into the feed document or links.
        let path = if params.contains_key("org") {
            format!("orgs/{}", encode_component(kind))
        } else {
            encode_component(kind)
        };
        (
            Some(EventViewer::Feed {
                user: user.id,
                token: feed.token,
                value: token,
            }),
            scope,
            false,
            format!("{} activity", user.username),
            format!("{}/api/activity-feeds/private/{username}/{path}", origin()),
        )
    } else {
        let viewer =
            auth::authenticate(&state, &headers, false)
                .await?
                .map(|v| EventViewer::Token {
                    user: v.user.id,
                    token: v.token.id,
                });
        if let Some(username) = params.get("username") {
            let user =
                codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, username)
                    .await
                    .map_err(Error::unavailable)?
                    .filter(|u| !u.is_banned)
                    .ok_or_else(Error::missing)?;
            (
                viewer,
                EventScope::Actor(user.id),
                true,
                format!("{} public activity", user.username),
                format!(
                    "{}/api/activity-feeds/users/{}",
                    origin(),
                    encode_component(username)
                ),
            )
        } else {
            (
                viewer,
                EventScope::All,
                true,
                "Public activity".into(),
                format!("{}/api/activity-feeds/public.atom", origin()),
            )
        }
    };
    let (values, _) = crate::backend::api::github::activity::events::documents(
        &state, viewer, None, scope, public, 0, 100,
    )
    .await?;
    let body = atom(&title, &id, &config.updated_at.to_rfc3339(), &values);
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/atom+xml; charset=utf-8".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    Ok(response)
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("activity/get-feeds", get(discovery))]
}
pub(in crate::backend::api::github) fn resources() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/activity-feeds/public.atom", get(feed))
        .route("/api/activity-feeds/users/:username", get(feed))
        .route("/api/activity-feeds/private/:username/:kind", get(feed))
        .route("/api/activity-feeds/private/:username/orgs/:org", get(feed))
}
