//! Native registry protocols use personal and job tokens, never browser cookies.
mod archives;
mod maven;
mod npm;
mod nuget;
mod oci;
mod rubygems;
pub(crate) use maven::route as maven_requests;
mod pages;
mod tokens;

use super::github::auth::{self, Authentication};
use crate::backend::{
    app::AppState,
    db::{
        self,
        packages::{Owner, Package, Permission, Refusal},
    },
    packages,
};
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use std::sync::Arc;

#[derive(Debug)]
struct Error(StatusCode, &'static str, &'static str);
type Result<T> = std::result::Result<T, Error>;

/// One of the shared upload slots, held while a registry upload is buffered and stored, so
/// concurrent large uploads cannot exhaust memory.
fn upload_slot(state: &AppState) -> Result<tokio::sync::OwnedSemaphorePermit> {
    state
        .upload_reads
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::unavailable())
}

impl Error {
    fn required() -> Self {
        Self(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Authentication required",
        )
    }

    fn registry_response(self) -> Response {
        let mut response = (self.0, Json(json!({"message":self.2}))).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        if self.0 == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"Codeyard Packages\"".parse().unwrap(),
            );
        }
        response
    }

    fn invalid() -> Self {
        Self(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            "Invalid package name or request",
        )
    }
    fn missing() -> Self {
        Self(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "Package not found")
    }
    fn unavailable() -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "UNKNOWN",
            "Package storage unavailable",
        )
    }
    fn db(error: anyhow::Error) -> Self {
        match error.downcast_ref::<Refusal>() {
            Some(Refusal::Missing) => Self::missing(),
            Some(Refusal::Unauthorized) => Self(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Authentication required",
            ),
            Some(Refusal::Forbidden) => {
                Self(StatusCode::FORBIDDEN, "DENIED", "Package access denied")
            }
            Some(Refusal::Conflict) => Self(
                StatusCode::CONFLICT,
                "DENIED",
                "Package version already exists",
            ),
            Some(Refusal::Blob) => Self(
                StatusCode::BAD_REQUEST,
                "BLOB_UNKNOWN",
                "Blob is missing or has the wrong size",
            ),
            Some(Refusal::Offset) => Self(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "RANGE_INVALID",
                "Upload offset does not match",
            ),
            None => {
                tracing::warn!(error=%error,"package operation failed");
                Self::unavailable()
            }
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut response = (
            self.0,
            Json(json!({"errors":[{"code":self.1,"message":self.2}]})),
        )
            .into_response();
        response.headers_mut().insert(
            "docker-distribution-api-version",
            "registry/2.0".parse().unwrap(),
        );
        if self.0 == StatusCode::UNAUTHORIZED {
            if let Ok(challenge) = format!(
                "Bearer realm=\"{}/v2/token\",service=\"codeyard\"",
                crate::backend::api::http::config::link_origin()
            )
            .parse()
            {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, challenge);
            }
        }
        response
    }
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    required: bool,
) -> Result<Option<Authentication>> {
    let mut normalized = headers.clone();
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if value
            .get(..6)
            .is_some_and(|v| v.eq_ignore_ascii_case("basic "))
        {
            if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
                return Err(Error::invalid());
            }
            let bytes = STANDARD.decode(&value[6..]).map_err(|_| Error::invalid())?;
            let value = std::str::from_utf8(&bytes).map_err(|_| Error::invalid())?;
            let (_, token) = value.split_once(':').ok_or_else(Error::invalid)?;
            normalized.insert(
                header::AUTHORIZATION,
                format!("Bearer {token}")
                    .parse()
                    .map_err(|_| Error::invalid())?,
            );
        }
    }
    auth::authenticate(state, &normalized, required)
        .await
        .map_err(|error| {
            let status = error.into_response().status();
            Error(
                status,
                if status == StatusCode::UNAUTHORIZED {
                    "UNAUTHORIZED"
                } else {
                    "DENIED"
                },
                "Authentication failed",
            )
        })
}

fn writer(auth: Option<&Authentication>) -> Result<&Authentication> {
    let auth = auth.ok_or_else(Error::required)?;
    auth.require_write()
        .map_err(|_| Error(StatusCode::FORBIDDEN, "DENIED", "Package access denied"))?;
    Ok(auth)
}

async fn owner(state: &AppState, name: &str) -> Result<Owner> {
    db::packages::owner(&state.pool, name, None)
        .await
        .map_err(Error::db)?
        .ok_or_else(Error::missing)
}

async fn package(
    state: &AppState,
    owner: &Owner,
    kind: &str,
    name: &str,
    auth: Option<&Authentication>,
    create: bool,
) -> Result<Package> {
    if !packages::valid_package_name(kind, name) {
        return Err(Error::invalid());
    }
    if create {
        let auth = writer(auth)?;
        let repo = if auth.user.username == db::ci_tokens::BOT {
            Some(
                db::ci_tokens::get(&state.pool, auth.token.id)
                    .await
                    .map_err(Error::db)?
                    .ok_or_else(Error::missing)?
                    .repo_id,
            )
        } else {
            db::repositories::records::get_repository(&state.pool, &owner.name, name)
                .await
                .map_err(Error::db)?
                .map(|repo| repo.id)
        };
        db::packages::create(&state.pool, owner, kind, name, auth.token.id, repo)
            .await
            .map_err(Error::db)
    } else {
        db::packages::find(
            &state.pool,
            owner,
            kind,
            name,
            auth.map(|a| a.token.id.into()),
            Permission::Read,
            false,
        )
        .await
        .map_err(Error::db)
    }
}

async fn all_versions(
    state: &AppState,
    package: i64,
    auth: Option<&Authentication>,
) -> Result<Vec<db::packages::Version>> {
    let mut versions = Vec::new();
    loop {
        let page = db::packages::versions(
            &state.pool,
            package,
            auth.map(|a| a.token.id.into()),
            false,
            versions.len() as i64,
            100,
        )
        .await
        .map_err(Error::db)?;
        let done = page.len() < 100;
        versions.extend(page);
        if done {
            return Ok(versions);
        }
    }
}

async fn owner_packages(
    state: &AppState,
    owner: &Owner,
    kind: &str,
    auth: Option<&Authentication>,
) -> Result<Vec<Package>> {
    let mut packages = Vec::new();
    loop {
        let page = db::packages::list(
            &state.pool,
            auth.map(|a| a.token.id.into()),
            db::packages::Listing {
                owner: Some(owner),
                repo: None,
                kind: Some(kind),
                visibility: None,
                offset: packages.len() as i64,
                limit: 100,
            },
        )
        .await
        .map_err(Error::db)?;
        let done = page.len() < 100;
        packages.extend(page);
        if done {
            return Ok(packages);
        }
    }
}

fn finish(
    result: std::result::Result<Result<Response>, tokio::time::error::Elapsed>,
    anonymous: bool,
) -> Response {
    match result {
        Ok(Ok(mut response)) => {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
        }
        // Challenge private and unknown resources alike so Basic-auth clients can retry.
        // Authenticated outsiders still receive 404, with no resource-existence disclosure.
        Ok(Err(error)) if anonymous && error.0 == StatusCode::NOT_FOUND => {
            Error::required().registry_response()
        }
        Ok(Err(error)) => error.registry_response(),
        Err(_) => Error::unavailable().registry_response(),
    }
}

pub fn router() -> Router<Arc<AppState>> {
    oci::router()
        .merge(npm::router())
        .merge(nuget::router())
        .merge(rubygems::router())
        .merge(pages::router())
}
