//! Docker exchanges login credentials for short-lived, repository-scoped bearer tokens.
use crate::packages::Error;
use crate::packages::Result;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use codeyard_app::api_shared::github::auth::Authentication;
use codeyard_app::app::AppState;
use codeyard_support::token_hash::token_hash;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub(super) async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    resource: &str,
    write: bool,
) -> Result<Option<Authentication>> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, value)| {
            scheme.eq_ignore_ascii_case("bearer") && value.starts_with("cyp_registry_")
        })
        .map(|(_, value)| value);
    let Some(value) = value else {
        return crate::packages::authenticate(state, headers, write).await;
    };
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return Err(Error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Invalid registry credentials",
        ));
    }
    // This marker carries no authority: every operation still uses anonymous package policy.
    // It is deliberately public and requires no per-exchange durable state.
    if value == "cyp_registry_anonymous" {
        return crate::packages::authenticate(state, &HeaderMap::new(), write).await;
    }
    let grant = codeyard_db_repos::packages::registry_token(&state.pool, &token_hash(value))
        .await
        .map_err(Error::db)?
        .ok_or(Error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Registry token expired or revoked",
        ))?;
    let resources: BTreeMap<String, BTreeSet<String>> =
        serde_json::from_str(&grant.resources).map_err(|error| Error::db(error.into()))?;
    let granted = if resource.is_empty() {
        !write
    } else {
        resources
            .get(resource)
            .ok_or_else(Error::missing)?
            .contains(if write { "push" } else { "pull" })
    };
    if !granted || (write && !grant.write) {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "DENIED",
            if write {
                "Registry token is read-only"
            } else {
                "Registry token does not allow pulls"
            },
        ));
    }
    let Some(id) = grant.token_id else {
        return crate::packages::authenticate(state, &HeaderMap::new(), write).await;
    };
    let configured = codeyard_db_cluster::node_config::get_node_configuration(&state.pool)
        .await
        .map_err(Error::db)?
        .is_some_and(|config| config.setup_completed);
    if !configured {
        return Err(Error::missing());
    }
    let user = codeyard_db_identity::users::tokens::token_user(&state.pool, id, write)
        .await
        .map_err(Error::db)?
        .ok_or(Error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Registry credentials revoked",
        ))?;
    let token = codeyard_db_repos::packages::registry_source(&state.pool, id)
        .await
        .map_err(Error::db)?
        .ok_or(Error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Registry credentials revoked",
        ))?;
    Ok(Some(Authentication { token, user }))
}

pub(super) async fn exchange(
    state: &AppState,
    headers: &HeaderMap,
    query: &str,
) -> Result<Response> {
    let auth = crate::packages::authenticate(state, headers, false).await?;
    let mut resources = BTreeMap::new();
    let mut count = 0;
    for (_, value) in
        url::form_urlencoded::parse(query.as_bytes()).filter(|(key, _)| key == "scope")
    {
        for scope in value.split_ascii_whitespace() {
            count += 1;
            if count > 64 {
                return Err(Error::invalid());
            }
            let mut parts = scope.split(':');
            if parts.next() != Some("repository") {
                return Err(Error::invalid());
            }
            let resource = parts
                .next()
                .filter(|name| name.len() <= 512)
                .ok_or_else(Error::invalid)?;
            let (owner, name) = resource.split_once('/').ok_or_else(Error::invalid)?;
            if !codeyard_model::package_input::valid_name(owner)
                || owner.contains('/')
                || !codeyard_model::package_input::valid_name(name)
            {
                return Err(Error::invalid());
            }
            let actions: Vec<_> = parts
                .next()
                .ok_or_else(Error::invalid)?
                .split(',')
                .collect();
            if actions.is_empty()
                || actions
                    .iter()
                    .any(|action| !matches!(*action, "pull" | "push"))
                || parts.next().is_some()
            {
                return Err(Error::invalid());
            }
            if auth.is_none() {
                let owner = crate::packages::container_owner(state, owner).await?;
                crate::packages::package(state, &owner, "container", name, None, false).await?;
            }
            // Anonymous tokens can only pull.
            let granted: &mut BTreeSet<String> = resources.entry(resource.to_owned()).or_default();
            for action in actions {
                if action == "pull" || auth.is_some() {
                    granted.insert(action.to_owned());
                }
            }
        }
    }
    if auth.is_none() {
        return Ok(Json(json!({"token":"cyp_registry_anonymous","access_token":"cyp_registry_anonymous","expires_in":300,"issued_at":chrono::Utc::now().to_rfc3339()})).into_response());
    }
    let write = resources.values().any(|actions| actions.contains("push"));
    let resources = serde_json::to_string(&resources).map_err(|error| Error::db(error.into()))?;
    let value = format!(
        "cyp_registry_{}",
        codeyard_api_http::session::generate_session_id()
    );
    codeyard_db_repos::packages::issue_registry_token(
        &state.pool,
        &token_hash(&value),
        auth.as_ref().map(|auth| auth.token.id),
        &resources,
        write,
    )
    .await
    .map_err(Error::db)?;
    Ok(Json(json!({"token":value,"access_token":value,"expires_in":300,"issued_at":chrono::Utc::now().to_rfc3339()})).into_response())
}
