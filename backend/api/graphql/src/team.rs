//! Team identities use native active-membership and secret-team visibility, not repository grants.
use crate::missing;
use crate::token_denied;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use serde_json::Value;

pub(super) async fn by_id(id: i64, request: &RequestContext) -> Result<Value> {
    if request.bounded() {
        return Err(token_denied());
    }
    request.require_oauth_scope("read:org")?;
    let team = codeyard_db_orgs::organizations::teams::team_identity(
        &request.state.pool,
        request.auth.user.id,
        request.auth.token.id,
        codeyard_db_orgs::organizations::teams::TeamIdentity::Id(id),
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    request.authorize_organization(team.org_id).await?;
    crate::review_requests::team_object(&team, request).await
}
pub(super) async fn by_slug(org: i64, slug: &str, request: &RequestContext) -> Result<Value> {
    if request.bounded() {
        return Err(token_denied());
    }
    request.authorize_organization(org).await?;
    let team = codeyard_db_orgs::organizations::teams::team_identity(
        &request.state.pool,
        request.auth.user.id,
        request.auth.token.id,
        codeyard_db_orgs::organizations::teams::TeamIdentity::Slug(org, slug),
    )
    .await
    .map_err(unavailable)?;
    // GitHub answers `team: null` for a slug that names no team.
    let Some(team) = team else {
        return Ok(Value::Null);
    };
    request.authorize_organization(org).await?;
    crate::review_requests::team_object(&team, request).await
}
