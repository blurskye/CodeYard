//! Cells share one search index. Hits in this cell's repositories are checked again against
//! live access; another cell's private hit is shown only to a credential that acts as its
//! whole account, as that cell named the account a reader.
use axum::http::StatusCode;
use codeyard_app::app::AppState;
use codeyard_db_core::cells::search::{Filter, Hit, Kind, Query, Refusal};
use codeyard_identity::github_auth::Authentication;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;

pub(super) struct Found {
    pub total: i64,
    pub hits: Vec<Hit>,
    pub incomplete: bool,
}

/// `None` when this server has no shared index; it then searches its own database.
#[allow(
    clippy::too_many_arguments,
    reason = "Each caller passes its own parsed query, order and page."
)]
pub(super) async fn search(
    state: &AppState,
    auth: Option<&Authentication>,
    kind: Kind,
    filter: Filter,
    sort: Option<String>,
    ascending: bool,
    window: (usize, usize),
    scope: &str,
) -> Result<Option<Found>> {
    let Some(index) = state.pool.search() else {
        return Ok(None);
    };
    let (offset, limit) = window;
    let query = Query {
        kind,
        viewer: auth.map(|auth| auth.user.id),
        filter: own_login(filter, auth.map(|auth| auth.user.username.as_str())),
        sort,
        ascending,
        offset: u32::try_from(offset).map_err(|_| Error::invalid("Invalid page"))?,
        limit: u32::try_from(limit).map_err(|_| Error::invalid("Invalid page"))?,
    };
    query.validate().map_err(Error::invalid)?;
    let results = index.search(&query).await.map_err(|error| {
        if matches!(error.downcast_ref::<Refusal>(), Some(Refusal::Invalid)) {
            Error::invalid("Search request is invalid")
        } else {
            tracing::warn!(error = %format!("{error:#}"), "Search index is unavailable");
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Search is unavailable. Try again shortly.",
            )
        }
    })?;
    let cell = i32::from(state.pool.cell_id().get());
    let mut local: Vec<i64> = results
        .items
        .iter()
        .filter(|hit| hit.cell == cell)
        .map(|hit| hit.repository_id)
        .collect();
    local.sort_unstable();
    local.dedup();
    // This also refuses a credential revoked since it authenticated.
    let readable =
        codeyard_identity::github_auth::readable_repositories(state, auth, &local, scope).await?;
    let account = match auth {
        Some(auth) => auth
            .acts_as_account(&state.pool)
            .await
            .map_err(|_| Error::unavailable(()))?,
        None => false,
    };
    let mut incomplete = false;
    let hits = results
        .items
        .into_iter()
        .filter(|hit| {
            let keep = if hit.cell == cell {
                readable.contains(&hit.repository_id)
            } else if hit.public {
                auth.is_none() || hit.classic_allowed || !account
            } else {
                account && hit.classic_allowed
            };
            incomplete |= !keep;
            keep
        })
        .collect();
    Ok(Some(Found {
        total: results.total,
        hits,
        incomplete,
    }))
}

/// The index knows logins, not sessions, so `@me` becomes the viewer's login.
fn own_login(filter: Filter, login: Option<&str>) -> Filter {
    match filter {
        Filter::All(items) => Filter::All(items.into_iter().map(|f| own_login(f, login)).collect()),
        Filter::Any(items) => Filter::Any(items.into_iter().map(|f| own_login(f, login)).collect()),
        Filter::Not(item) => Filter::Not(Box::new(own_login(*item, login))),
        Filter::Term(key, value) if value == "@me" => {
            Filter::Term(key, login.unwrap_or("@me").to_owned())
        }
        term => term,
    }
}
