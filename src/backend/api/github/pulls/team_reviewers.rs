use super::super::{Error, Result};
use crate::backend::{app::AppState, db};
use serde_json::Value;

pub(super) async fn documents(
    state: &AppState,
    repo: i64,
    pull: i64,
    viewer: Option<i64>,
) -> Result<Vec<Value>> {
    let teams = db::pull_requests::team_reviewers::list(&state.pool, repo, Some(pull), viewer)
        .await
        .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for row in teams {
        values.push(
            super::super::teams::summary(state, &row.team, &row.org_slug, row.permission).await?,
        );
    }
    Ok(values)
}
