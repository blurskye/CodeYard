//! Team identities use native active-membership and secret-team visibility, not repository grants.
use super::{db, missing, token_denied, unavailable, RequestContext, Result, Value};
pub(super) async fn by_id(id: i64, request: &RequestContext) -> Result<Value> {
    if request.bounded() {
        return Err(token_denied());
    }
    request.require_oauth_scope("read:org")?;
    let team = db::organizations::teams::team_identity(
        &request.state.pool, request.auth.user.id, request.auth.token.id,
        db::organizations::teams::TeamIdentity::Id(id),
    ).await.map_err(unavailable)?.ok_or_else(missing)?;
    request.authorize_organization(team.org_id).await?;
    super::review_requests::team_object(&team, request).await
}
pub(super) async fn by_slug(org: i64, slug: &str, request: &RequestContext) -> Result<Value> {
    if request.bounded() {
        return Err(token_denied());
    }
    request.authorize_organization(org).await?;
    let team = db::organizations::teams::team_identity(
        &request.state.pool, request.auth.user.id, request.auth.token.id,
        db::organizations::teams::TeamIdentity::Slug(org, slug),
    ).await.map_err(unavailable)?.ok_or_else(missing)?;
    request.authorize_organization(org).await?;
    super::review_requests::team_object(&team, request).await
}
