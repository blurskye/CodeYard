//! Team identities use native active-membership and secret-team visibility, not repository grants.
use super::{db, missing, token_denied, unavailable, RequestContext, Result, Value};
pub(super) async fn by_id(id: i64, request: &RequestContext) -> Result<Value> {
    if request.bounded() {
        return Err(token_denied());
    }
    request.require_oauth_scope("read:org")?;
    let team = db::organizations::teams::get_team_by_id(
        &request.state.pool, request.auth.user.id, id,
    ).await.map_err(unavailable)?.ok_or_else(missing)?;
    request.authorize_organization(team.org_id).await?;
    super::review_requests::team_object(&team, request).await
}
pub(super) async fn by_slug(org: i64, slug: &str, request: &RequestContext) -> Result<Value> {
    if request.bounded() {
        return Err(token_denied());
    }
    request.authorize_organization(org).await?;
    let visible = db::organizations::teams::get_public_team(
        &request.state.pool,
        request.auth.user.id,
        org,
        slug,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    request.authorize_organization(org).await?;
    super::review_requests::team_object(&visible.team, request).await
}
