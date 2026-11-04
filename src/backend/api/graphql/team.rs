//! Team identities use native active-membership and secret-team visibility, not repository grants.
use crate::backend::api::graphql::db;
use crate::backend::api::graphql::missing;
use crate::backend::api::graphql::token_denied;
use crate::backend::api::graphql::unavailable;
use crate::backend::api::graphql::RequestContext;
use crate::backend::api::graphql::Result;
use crate::backend::api::graphql::Value;
pub(super) async fn by_id(id: i64, request: &RequestContext) -> Result<Value> {
    if request.bounded() {
        return Err(token_denied());
    }
    request.require_oauth_scope("read:org")?;
    let team = db::organizations::teams::team_identity(
        &request.state.pool,
        request.auth.user.id,
        request.auth.token.id,
        db::organizations::teams::TeamIdentity::Id(id),
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    request.authorize_organization(team.org_id).await?;
    crate::backend::api::graphql::review_requests::team_object(&team, request).await
}
pub(super) async fn by_slug(org: i64, slug: &str, request: &RequestContext) -> Result<Value> {
    if request.bounded() {
        return Err(token_denied());
    }
    request.authorize_organization(org).await?;
    let team = db::organizations::teams::team_identity(
        &request.state.pool,
        request.auth.user.id,
        request.auth.token.id,
        db::organizations::teams::TeamIdentity::Slug(org, slug),
    )
    .await
    .map_err(unavailable)?;
    // GitHub answers `team: null` for a slug that names no team.
    let Some(team) = team else {
        return Ok(Value::Null);
    };
    request.authorize_organization(org).await?;
    crate::backend::api::graphql::review_requests::team_object(&team, request).await
}
