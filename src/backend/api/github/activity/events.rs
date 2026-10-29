use super::{auth, repository, Error, Pagination, Result};
use crate::backend::{
    app::AppState,
    db::{self, events::EventScope},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Extension,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Copy)]
enum Listing {
    Public,
    Repository,
    Network,
    Organization,
    User { received: bool, public: bool },
    UserOrganization,
}

pub(super) async fn documents(
    state: &AppState,
    viewer: Option<i64>,
    scope: EventScope,
    public: bool,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Value>, bool)> {
    let entries = db::events::list_events(
        &state.pool,
        viewer,
        scope,
        public,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let more = entries.len() > limit;
    let mut actors: HashMap<i64, Value> = HashMap::new();
    let mut values = Vec::new();
    for entry in entries.into_iter().take(limit) {
        let actor = if let Some(id) = entry.event.actor_id {
            if let Some(value) = actors.get(&id) {
                value.clone()
            } else {
                let value = db::users::accounts::get_user_by_id(&state.pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .as_ref()
                    .map(payloads::user)
                    .unwrap_or_else(|| entry.event.payload["sender"].clone());
                actors.insert(id, value.clone());
                value
            }
        } else {
            entry.event.payload["sender"].clone()
        };
        if !actor.is_object() {
            return Err(Error::unavailable(()));
        }
        values.push(payloads::event_json(
            &entry.event,
            &entry.repository,
            &entry.owner,
            actor,
        ));
    }
    Ok((values, more))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    params: Option<Path<HashMap<String, String>>>,
    Query(page): Query<Pagination>,
    Extension(listing): Extension<Listing>,
) -> Response {
    let result: Result<Response> = async {
        let mut params = params.map(|Path(p)| p).unwrap_or_default();
        // Shared routes normalize the username segment to user_id.
        if let Some(username) = params.remove("user_id") {
            params.insert("username".into(), username);
        }
        let param = |key: &str| {
            params
                .get(key)
                .map(String::as_str)
                .ok_or_else(Error::missing)
        };
        let identity = auth::authenticate(
            &state,
            &headers,
            matches!(listing, Listing::UserOrganization),
        )
        .await?;
        let viewer = identity.as_ref().map(|a| a.user.id);
        let (scope, public) = match listing {
            Listing::Public => (EventScope::All, true),
            Listing::Repository | Listing::Network => {
                let (repo, _) =
                    repository(&state, &headers, param("owner")?, param("repo")?, false).await?;
                if matches!(listing, Listing::Network) {
                    (EventScope::Network(repo.id), true)
                } else {
                    (EventScope::Repository(repo.id), false)
                }
            }
            Listing::Organization | Listing::UserOrganization => {
                let org = db::organizations::profiles::get_public_organization(
                    &state.pool,
                    param("org")?,
                )
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
                let public = if matches!(listing, Listing::UserOrganization) {
                    let user =
                        db::users::accounts::get_user_by_username(&state.pool, param("username")?)
                            .await
                            .map_err(Error::unavailable)?
                            .ok_or_else(Error::missing)?;
                    if viewer != Some(user.id) {
                        return Err(Error::new(
                            StatusCode::FORBIDDEN,
                            "Authenticate as the requested user",
                        ));
                    }
                    if !matches!(
                        db::organizations::profiles::get_organization_metadata(
                            &state.pool,
                            user.id,
                            org.id
                        )
                        .await
                        .map_err(Error::unavailable)?,
                        db::organizations::OrganizationOutcome::Applied(_)
                    ) {
                        return Err(Error::missing());
                    }
                    false
                } else {
                    true
                };
                (EventScope::Organization(org.id), public)
            }
            Listing::User { received, public } => {
                let user =
                    db::users::accounts::get_user_by_username(&state.pool, param("username")?)
                        .await
                        .map_err(Error::unavailable)?
                        .filter(|u| !u.is_banned)
                        .ok_or_else(Error::missing)?;
                let scope = if received {
                    EventScope::Received(user.id)
                } else {
                    EventScope::Actor(user.id)
                };
                (scope, public || viewer != Some(user.id))
            }
        };
        let (offset, limit) = page.window()?;
        let (values, more) = documents(&state, viewer, scope, public, offset, limit).await?;
        let etag = format!(
            "\"{:x}\"",
            Sha256::digest(serde_json::to_vec(&values).map_err(Error::unavailable)?)
        );
        let unchanged = crate::backend::api::http::static_files::not_modified(&headers, &etag);
        let mut response = if unchanged {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            page.response(&uri, values, more)?
        };
        response
            .headers_mut()
            .insert(header::ETAG, etag.parse().map_err(Error::unavailable)?);
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            "private, max-age=60".parse().unwrap(),
        );
        response
            .headers_mut()
            .insert(header::VARY, "Authorization".parse().unwrap());
        response
            .headers_mut()
            .insert("x-poll-interval", "60".parse().unwrap());
        Ok(response)
    }
    .await;
    match result {
        Ok(response) => response,
        Err(error) => {
            let json = match listing {
                Listing::Public => matches!(
                    error.0,
                    StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE
                ),
                Listing::Network => {
                    matches!(error.0, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND)
                }
                _ => false,
            };
            if json {
                error.into_response()
            } else {
                let mut response = error.0.into_response();
                response
                    .headers_mut()
                    .insert(header::CONTENT_LENGTH, "0".parse().unwrap());
                response
            }
        }
    }
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "activity/list-public-events",
            get(list).layer(Extension(Listing::Public)),
        ),
        (
            "activity/list-repo-events",
            get(list).layer(Extension(Listing::Repository)),
        ),
        (
            "activity/list-public-events-for-repo-network",
            get(list).layer(Extension(Listing::Network)),
        ),
        (
            "activity/list-public-org-events",
            get(list).layer(Extension(Listing::Organization)),
        ),
        (
            "activity/list-org-events-for-authenticated-user",
            get(list).layer(Extension(Listing::UserOrganization)),
        ),
        (
            "activity/list-events-for-authenticated-user",
            get(list).layer(Extension(Listing::User {
                received: false,
                public: false,
            })),
        ),
        (
            "activity/list-public-events-for-user",
            get(list).layer(Extension(Listing::User {
                received: false,
                public: true,
            })),
        ),
        (
            "activity/list-received-events-for-user",
            get(list).layer(Extension(Listing::User {
                received: true,
                public: false,
            })),
        ),
        (
            "activity/list-received-public-events-for-user",
            get(list).layer(Extension(Listing::User {
                received: true,
                public: true,
            })),
        ),
    ]
}
