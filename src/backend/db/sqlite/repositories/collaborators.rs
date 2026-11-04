use crate::backend::db::repositories::collaborators::Added;
use crate::backend::db::repositories::collaborators::Collaborator;
use crate::backend::db::repositories::collaborators::CollaboratorQuery;
use crate::backend::db::repositories::collaborators::Invitation;
use crate::backend::db::repositories::collaborators::InvitationSummary;
use crate::backend::db::repositories::collaborators::Refusal;
use anyhow::Result;
use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::User;
use sqlx::{Sqlite, SqlitePool, Transaction};

async fn admin(tx: &mut Transaction<'_, Sqlite>, actor: i64, repo: i64) -> Result<()> {
    anyhow::ensure!(
        super::super::discussions::lock_admin_authority(tx, actor, repo).await?,
        Refusal::Denied
    );
    Ok(())
}

pub async fn add(
    pool: &SqlitePool,
    actor: i64,
    repo_id: i64,
    username: &str,
    permission: AccessLevel,
) -> Result<Added> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    admin(&mut tx, actor, repo_id).await?;
    anyhow::ensure!((1..=5).contains(&(permission as i16)), Refusal::Denied);
    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE username=?1 AND NOT is_banned AND NOT must_change_password",
    )
    .bind(username)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Refusal::NotFound)?;
    let owner: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1 AND owner_kind='user' AND owner_user_id=?2)").bind(repo_id).bind(user.id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(!owner, Refusal::Owner);
    let existing: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repo_access WHERE repo_id=?1 AND user_id=?2) OR EXISTS(SELECT 1 FROM repositories r JOIN org_memberships m ON m.org_id=r.owner_org_id WHERE r.id=?1 AND m.user_id=?2 AND m.role IN ('owner','admin','member'))").bind(repo_id).bind(user.id).fetch_one(&mut *tx).await?;
    if existing {
        super::access::grant_repo_access_in_transaction(&mut tx, user.id, repo_id, permission)
            .await?;
        tx.commit().await?;
        return Ok(Added::Updated);
    }
    sqlx::query("UPDATE repository_invitations SET state='expired' WHERE repo_id=?1 AND state='pending' AND created_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now','-7 days')").bind(repo_id).execute(&mut *tx).await?;
    let existing: Option<i64>=sqlx::query_scalar("SELECT id FROM repository_invitations WHERE repo_id=?1 AND invitee_id=?2 AND state='pending'").bind(repo_id).bind(user.id).fetch_optional(&mut *tx).await?;
    let invitation = if let Some(id) = existing {
        sqlx::query_as::<_, Invitation>(
            "UPDATE repository_invitations SET permission=?2,inviter_id=?3 WHERE id=?1 RETURNING *",
        )
        .bind(id)
        .bind(permission as i16)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await?
    } else {
        let count: i64=sqlx::query_scalar("SELECT COUNT(*) FROM repository_invitations WHERE repo_id=?1 AND created_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 day')").bind(repo_id).fetch_one(&mut *tx).await?;
        anyhow::ensure!(count < 50, Refusal::Limit);
        sqlx::query_as::<_,Invitation>("INSERT INTO repository_invitations(repo_id,invitee_id,inviter_id,permission) VALUES(?1,?2,?3,?4) RETURNING *").bind(repo_id).bind(user.id).bind(actor).bind(permission as i16).fetch_one(&mut *tx).await?
    };
    tx.commit().await?;
    Ok(Added::Invited(invitation))
}

pub async fn remove(pool: &SqlitePool, actor: i64, repo_id: i64, username: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    admin(&mut tx, actor, repo_id).await?;
    let target: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username=?1")
        .bind(username)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::NotFound)?;
    let owner: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1 AND owner_kind='user' AND owner_user_id=?2)").bind(repo_id).bind(target).fetch_one(&mut *tx).await?;
    anyhow::ensure!(!owner, Refusal::Owner);
    super::access::revoke_repo_access_in_transaction(&mut tx, target, repo_id).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list(
    pool: &SqlitePool,
    repo_id: i64,
    query: &CollaboratorQuery<'_>,
) -> Result<Vec<Collaborator>> {
    let projection = super::access::user_repo_grant_sql();
    Ok(sqlx::query_as(&format!("WITH candidates AS (SELECT owner_user_id AS id FROM repositories WHERE id=?1 AND owner_kind='user' UNION SELECT user_id FROM repo_access WHERE repo_id=?1 UNION SELECT m.user_id FROM org_memberships m JOIN repositories r ON r.owner_org_id=m.org_id WHERE r.id=?1 AND m.role IN ('owner','admin','member'))
        SELECT * FROM (SELECT u.*, ({projection}) AS permission,
        EXISTS(SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=u.id) AS direct,
        EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=u.id AND m.role IN ('owner','admin','member')) AS organization_member,
        COALESCE(r.owner_user_id=u.id AND r.owner_kind='user',FALSE) AS personal_owner
        FROM repositories r CROSS JOIN candidates c JOIN users u ON u.id=c.id WHERE r.id=?1 AND (?2 IS NULL OR u.username=?2)) found
        WHERE permission>0 AND (?3 IS NULL OR permission=?3 OR (?3=5 AND permission=6))
        AND (?4='all' OR (?4='direct' AND direct) OR (?4='outside' AND direct AND NOT organization_member AND NOT personal_owner)) ORDER BY username LIMIT ?5 OFFSET ?6"))
        .bind(repo_id).bind(query.username).bind(query.permission).bind(query.affiliation).bind(query.limit).bind(query.offset).fetch_all(pool).await?)
}

fn administration(user: &str) -> String {
    let access = super::access::USER_REPO_ACCESS_SQL;
    format!("EXISTS(SELECT 1 FROM users u WHERE u.id={user} AND NOT u.is_banned AND NOT u.must_change_password
        AND NOT r.is_gist AND NOT r.is_archived
        AND ((r.owner_kind='user' AND u.is_admin) OR ({access})>=5)
        AND (u.is_admin OR NOT EXISTS(SELECT 1 FROM user_blocks b WHERE r.owner_kind='user'
            AND ((b.user_id=r.owner_user_id AND b.blocked_id=u.id) OR (b.blocked_id=r.owner_user_id AND b.user_id=u.id)))))")
}

fn invitation_query(projection: &str, joins: &str, session: bool) -> String {
    let inviter = administration("i.inviter_id");
    let viewer = administration("?1");
    let credential = if session {
        "AND EXISTS(SELECT 1 FROM sessions s JOIN users viewer ON viewer.id=s.user_id WHERE s.id=?5 AND s.user_id=?1 AND julianday(s.expires_at)>julianday('now') AND NOT viewer.is_banned AND NOT viewer.must_change_password)"
    } else {
        ""
    };
    format!("SELECT {projection} FROM repository_invitations i JOIN repositories r ON r.id=i.repo_id {joins}
        WHERE i.state='pending' AND julianday(i.created_at)>julianday('now','-7 days') AND {inviter}
        AND EXISTS(SELECT 1 FROM users recipient WHERE recipient.id=?1 AND NOT recipient.is_banned AND NOT recipient.must_change_password)
        AND ((?2 IS NULL AND i.invitee_id=?1) OR (i.repo_id=?2 AND {viewer})) {credential}
        ORDER BY i.id DESC LIMIT ?3 OFFSET ?4")
}

pub async fn recipient_invitations(
    pool: &SqlitePool,
    actor: i64,
    session: &str,
    offset: i64,
    limit: i64,
) -> Result<Vec<InvitationSummary>> {
    anyhow::ensure!(
        offset >= 0 && (1..=101).contains(&limit),
        "Invalid invitation page"
    );
    let query = invitation_query("i.*,CASE WHEN r.owner_kind='user' THEN owner.username ELSE org.name END AS owner,r.name AS repository,inviter.username AS inviter",
        "JOIN users inviter ON inviter.id=i.inviter_id LEFT JOIN users owner ON owner.id=r.owner_user_id LEFT JOIN organizations org ON org.id=r.owner_org_id", true);
    Ok(sqlx::query_as(&query)
        .bind(actor)
        .bind(None::<i64>)
        .bind(limit)
        .bind(offset)
        .bind(session)
        .fetch_all(pool)
        .await?)
}

pub async fn invitations(
    pool: &SqlitePool,
    actor: i64,
    repo_id: Option<i64>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Invitation>> {
    anyhow::ensure!(offset >= 0 && limit > 0, "Invalid invitation page");
    Ok(sqlx::query_as(&invitation_query("i.*", "", false))
        .bind(actor)
        .bind(repo_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?)
}

pub async fn update_invitation(
    pool: &SqlitePool,
    actor: i64,
    repo_id: i64,
    id: i64,
    permission: Option<AccessLevel>,
) -> Result<Invitation> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    admin(&mut tx, actor, repo_id).await?;
    let invitation=sqlx::query_as::<_,Invitation>("UPDATE repository_invitations SET permission=COALESCE(?3,permission),inviter_id=?4,state=CASE WHEN ?3 IS NULL THEN 'cancelled' ELSE 'pending' END WHERE id=?1 AND repo_id=?2 AND state='pending' AND created_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-7 days') RETURNING *").bind(id).bind(repo_id).bind(permission.map(|level|level as i16)).bind(actor).fetch_optional(&mut *tx).await?.ok_or(Refusal::NotFound)?;
    tx.commit().await?;
    Ok(invitation)
}

pub async fn respond(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    id: i64,
    accept: bool,
) -> Result<()> {
    let authority = actor.into();
    let actor = authority.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let invitation=sqlx::query_as::<_,Invitation>("SELECT * FROM repository_invitations WHERE id=?1 AND invitee_id=?2 AND state='pending' AND created_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-7 days')").bind(id).bind(actor).fetch_optional(&mut *tx).await?.ok_or(Refusal::NotFound)?;
    let live: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password)").bind(actor).fetch_one(&mut *tx).await?;
    anyhow::ensure!(live, Refusal::Denied);
    if let Some(credential) = authority.credential() {
        super::super::users::transitions::require_account(&mut tx, actor, &credential)
            .await
            .map_err(|error| {
                if error.is::<crate::backend::db::users::accounts::AccountRestricted>() {
                    Refusal::Denied.into()
                } else {
                    error
                }
            })?;
    }
    if accept {
        admin(&mut tx, invitation.inviter_id, invitation.repo_id).await?;
        let current: i16=sqlx::query_scalar("SELECT COALESCE((SELECT access_level FROM repo_access WHERE repo_id=?1 AND user_id=?2),0)").bind(invitation.repo_id).bind(actor).fetch_one(&mut *tx).await?;
        super::access::grant_repo_access_in_transaction(
            &mut tx,
            actor,
            invitation.repo_id,
            AccessLevel::from(current.max(invitation.permission)),
        )
        .await?;
    }
    sqlx::query("UPDATE repository_invitations SET state=?2 WHERE id=?1")
        .bind(id)
        .bind(if accept { "accepted" } else { "declined" })
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
