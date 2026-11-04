use crate::backend::db::events::EventEntry;
use crate::backend::db::events::EventScope;
use crate::backend::db::events::EventViewer;
use anyhow::Result;
use serde_json::Value;
use sqlx::PgPool;

pub async fn record_event(
    pool: &PgPool,
    repo: i64,
    actor: Option<i64>,
    kind: &str,
    payload: &Value,
    public: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO events(repo_id,actor_id,kind,payload,is_public) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(repo)
    .bind(actor)
    .bind(kind)
    .bind(payload)
    .bind(public)
    .execute(pool)
    .await?;
    Ok(())
}
pub async fn list_events(
    pool: &PgPool,
    viewer: Option<EventViewer<'_>>,
    scope: EventScope,
    public_only: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<EventEntry>> {
    let (kind, id) = scope.parts();
    let (viewer, token, feed) = match viewer {
        Some(viewer) => {
            let (user, token, feed) = viewer.parts();
            (Some(user), Some(token), feed)
        }
        None => (None, None, None),
    };
    let offset = offset.max(0);
    let readable = crate::backend::db::postgres::repositories::access::readable_condition();
    let issuer = crate::backend::db::oauth_apps::token_issuer_condition("t", "$9");
    Ok(sqlx::query_as(&format!("WITH RECURSIVE ancestors(id,parent) AS (
        SELECT id,forked_from_id FROM repositories WHERE $2='network' AND id=$3
        UNION SELECT r.id,r.forked_from_id FROM repositories r JOIN ancestors a ON r.id=a.parent
    ), network(id) AS (SELECT id FROM ancestors UNION SELECT r.id FROM repositories r JOIN network n ON r.forked_from_id=n.id)
    SELECT e.id AS event_id,e.created_at AS event_created_at,e.repo_id,e.actor_id,e.kind,e.payload,(e.is_public AND NOT r.is_private AND (e.kind<>'ForkEvent' OR COALESCE(NOT fk.is_private,FALSE))) AS is_public,r.*,
           CASE WHEN r.owner_kind='user' THEN owner_u.username ELSE owner_o.name END AS owner
    FROM events e JOIN repositories r ON r.id=e.repo_id LEFT JOIN users u ON u.id=$1
    LEFT JOIN users owner_u ON r.owner_user_id=owner_u.id LEFT JOIN organizations owner_o ON r.owner_org_id=owner_o.id
    LEFT JOIN repositories fk ON e.kind='ForkEvent' AND fk.id=CAST(e.payload->'forkee'->>'id' AS BIGINT)
    WHERE e.created_at>=NOW()-INTERVAL '90 days' AND {readable}
      AND ((r.owner_kind='user' AND owner_u.id IS NOT NULL AND r.owner_org_id IS NULL) OR (r.owner_kind='org' AND owner_o.id IS NOT NULL AND r.owner_user_id IS NULL))
      AND ($7::bigint IS NULL OR EXISTS(SELECT 1 FROM personal_access_tokens t
           WHERE t.id=$7 AND t.user_id=$1 AND t.scope IN ('read','write') AND (t.expires_at IS NULL OR t.expires_at>NOW())
           AND NOT u.is_banned AND NOT u.must_change_password AND {issuer}
           AND ($8::text IS NULL OR EXISTS(SELECT 1 FROM activity_feed_tokens f WHERE f.api_token_id=t.id AND f.value=$8))))
      AND (NOT $4 OR (e.is_public AND NOT r.is_private))
      AND (e.is_public OR (u.id IS NOT NULL AND NOT u.is_banned AND NOT u.must_change_password))
      AND (e.kind<>'ForkEvent' OR EXISTS(SELECT 1 FROM repositories r WHERE r.id=CAST(e.payload->'forkee'->>'id' AS BIGINT) AND (NOT $4 OR NOT r.is_private) AND {readable}))
      AND ($2='all' OR ($2='repo' AND r.id=$3) OR ($2='network' AND r.id IN (SELECT id FROM network))
           OR ($2='org' AND r.owner_org_id=$3) OR ($2='actor' AND e.actor_id=$3)
           OR ($2='received' AND (EXISTS(SELECT 1 FROM follows f WHERE f.follower_id=$3 AND f.following_id=e.actor_id)
                 OR EXISTS(SELECT 1 FROM watches w WHERE w.user_id=$3 AND w.repo_id=r.id AND w.level IN ('watching','custom')))))
    ORDER BY e.created_at DESC,e.id DESC LIMIT $5 OFFSET $6"))
        .bind(viewer).bind(kind).bind(id).bind(public_only).bind(limit.clamp(1,101).min((300-offset).max(0))).bind(offset.max(0)).bind(token).bind(feed).bind(codeyard_model::identity::CLI_CLIENT).fetch_all(pool).await?)
}
pub async fn activity_feed_token(
    pool: &PgPool,
    token_id: i64,
    value: &str,
) -> Result<Option<String>> {
    let issuer = crate::backend::db::oauth_apps::token_issuer_condition("t", "$3");
    Ok(sqlx::query_scalar(&format!("INSERT INTO activity_feed_tokens(api_token_id,value) SELECT t.id,$2 FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND (t.expires_at IS NULL OR t.expires_at>NOW()) AND {issuer} ON CONFLICT(api_token_id) DO UPDATE SET api_token_id=excluded.api_token_id RETURNING value")).bind(token_id).bind(value).bind(codeyard_model::identity::CLI_CLIENT).fetch_optional(pool).await?)
}
pub async fn activity_feed_token_id(pool: &PgPool, value: &str) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT api_token_id FROM activity_feed_tokens WHERE value=$1")
            .bind(value)
            .fetch_optional(pool)
            .await?,
    )
}
/// Who made the latest recorded push that moved `reference` to `sha`.
pub async fn push_actor(
    pool: &PgPool,
    repo: i64,
    reference: &str,
    sha: &str,
) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar::<_, Option<i64>>("SELECT actor_id FROM events WHERE repo_id=$1 AND kind='PushEvent' AND payload->>'ref'=$2 AND payload->>'after'=$3 ORDER BY id DESC LIMIT 1")
        .bind(repo)
        .bind(reference)
        .bind(sha)
        .fetch_optional(pool)
        .await?
        .flatten())
}
pub async fn prune_events(pool: &PgPool) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM events WHERE created_at<NOW()-INTERVAL '90 days'")
            .execute(pool)
            .await?
            .rows_affected(),
    )
}
