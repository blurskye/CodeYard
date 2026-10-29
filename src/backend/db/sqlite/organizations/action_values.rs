use crate::backend::{
    crypto::{SecretEncryptor, SecretError},
    db::{
        organization_values::{
            self, Kind, OrganizationValue, Save, Selection, SettingsPage, SettingsValue, StoredValue,
            ValueError, SETTINGS_PAGE_SIZE,
        },
        organizations::{administers, OrganizationOutcome},
    },
};
use sqlx::{SqliteConnection as Connection, SqlitePool as Pool};

async fn authorize(connection: &mut Connection, actor: i64, org: i64) -> Result<(), ValueError> {
    match super::authority::authority(connection, actor, org)
        .await
        .map_err(ValueError::Authority)?
    {
        OrganizationOutcome::Applied(role) if administers(role) => Ok(()),
        OrganizationOutcome::Missing => Err(ValueError::NotFound),
        _ => Err(ValueError::Denied),
    }
}

pub async fn key(
    pool: &Pool,
    encryptor: &SecretEncryptor,
    actor: i64,
    org: i64,
) -> Result<(String, crypto_box::SecretKey), ValueError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, org).await?;
    let key = crypto_box::SecretKey::generate(&mut rand::thread_rng());
    let (encrypted, nonce) = encryptor.encrypt(&key.to_bytes())?;
    sqlx::query("INSERT INTO org_actions_keys(org_id,key_id,encrypted_key,nonce) VALUES($1,$2,$3,$4) ON CONFLICT(org_id) DO NOTHING")
        .bind(org).bind(uuid::Uuid::new_v4().simple().to_string()).bind(encrypted).bind(nonce.as_slice()).execute(&mut *tx).await?;
    let (id, encrypted, nonce): (String, Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT key_id,encrypted_key,nonce FROM org_actions_keys WHERE org_id=$1")
            .bind(org)
            .fetch_one(&mut *tx)
            .await?;
    let bytes: [u8; 32] = encryptor
        .decrypt(&encrypted, &nonce)?
        .try_into()
        .map_err(|_| SecretError::DecryptionFailed)?;
    tx.commit().await?;
    Ok((id, crypto_box::SecretKey::from(bytes)))
}

pub async fn list(
    pool: &Pool,
    actor: i64,
    org: i64,
    kind: Kind,
) -> Result<Vec<OrganizationValue>, ValueError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, org).await?;
    let rows = sqlx::query_as("SELECT name,kind,value,visibility,created_at,updated_at FROM org_actions_values WHERE org_id=$1 AND kind=$2 ORDER BY name").bind(org).bind(kind.as_str()).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

/// The page and selected-ID summaries share retained owner/session authority.
pub async fn settings_page(pool: &Pool, actor: i64, session: &str, org: i64, page: i64) -> Result<SettingsPage, ValueError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, org).await?;
    if !crate::backend::db::sqlite::users::transitions::live_session(&mut tx, actor, session).await.map_err(ValueError::Authority)? {
        return Err(ValueError::Denied);
    }
    let rows = sqlx::query_as::<_, SettingsValue>(
        "WITH page AS MATERIALIZED (
             SELECT kind,name FROM org_actions_values WHERE org_id=$1 ORDER BY kind,name LIMIT $2 OFFSET $3
         )
         SELECT v.name,v.kind,CASE WHEN v.kind='variable' THEN v.value ELSE '' END AS value,v.visibility,v.created_at,v.updated_at,
         CASE WHEN v.visibility='selected' THEN COALESCE((
             SELECT group_concat(repo_id,', ') FROM (
                 SELECT s.repo_id FROM org_actions_value_repositories s JOIN repositories r ON r.id=s.repo_id AND r.owner_org_id=s.org_id AND r.owner_kind='org'
                 WHERE s.org_id=v.org_id AND s.kind=v.kind AND s.name=v.name ORDER BY s.repo_id
             )
         ),'') ELSE '' END AS repository_ids
         FROM page p JOIN org_actions_values v ON v.org_id=$1 AND v.kind=p.kind AND v.name=p.name ORDER BY v.kind,v.name")
        .bind(org).bind(SETTINGS_PAGE_SIZE + 1).bind((page.clamp(1,1000) - 1) * SETTINGS_PAGE_SIZE)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(SettingsPage::from_rows(rows))
}

async fn valid_repositories(
    connection: &mut Connection,
    org: i64,
    ids: &[i64],
) -> Result<Vec<i64>, ValueError> {
    let ids: std::collections::BTreeSet<_> = ids.iter().copied().collect();
    for &id in &ids {
        if id <= 0
            || !super::authority::scoped_repo(connection, org, id)
                .await
                .map_err(ValueError::Authority)?
        {
            return Err(ValueError::Repository);
        }
    }
    Ok(ids.into_iter().collect())
}

async fn replace_repositories(
    connection: &mut Connection,
    org: i64,
    kind: Kind,
    name: &str,
    ids: &[i64],
) -> Result<(), ValueError> {
    let ids = valid_repositories(connection, org, ids).await?;
    sqlx::query(
        "DELETE FROM org_actions_value_repositories WHERE org_id=$1 AND kind=$2 AND name=$3",
    )
    .bind(org)
    .bind(kind.as_str())
    .bind(name)
    .execute(&mut *connection)
    .await?;
    for id in ids {
        sqlx::query("INSERT INTO org_actions_value_repositories(org_id,kind,name,repo_id) VALUES($1,$2,$3,$4)").bind(org).bind(kind.as_str()).bind(name).bind(id).execute(&mut *connection).await?;
    }
    Ok(())
}

pub async fn save(
    pool: &Pool,
    encryptor: Option<&SecretEncryptor>,
    actor: i64,
    org: i64,
    input: &Save<'_>,
) -> Result<bool, ValueError> {
    organization_values::validate(input.name, input.value.unwrap_or_default())?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, org).await?;
    let kind = input.kind.as_str();
    let previous = input.previous.unwrap_or(input.name);
    let old: Option<StoredValue> = sqlx::query_as("SELECT name,value,visibility,encrypted_value,nonce FROM org_actions_values WHERE org_id=$1 AND kind=$2 AND name=$3").bind(org).bind(kind).bind(previous).fetch_optional(&mut *tx).await?;
    if input.kind == Kind::Variable {
        if input.previous.is_some() && old.is_none() {
            return Err(ValueError::NotFound);
        }
        if input.previous.is_none() && old.is_some() {
            return Err(ValueError::Exists);
        }
    }
    if input.name != previous {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_actions_values WHERE org_id=$1 AND kind=$2 AND name=$3)").bind(org).bind(kind).bind(input.name).fetch_one(&mut *tx).await?;
        if exists {
            return Err(ValueError::Exists);
        }
    }
    if old.is_none() {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM org_actions_values WHERE org_id=$1 AND kind=$2",
        )
        .bind(org)
        .bind(kind)
        .fetch_one(&mut *tx)
        .await?;
        if count >= 1000 {
            return Err(ValueError::Limit);
        }
    }
    let visibility = input
        .visibility
        .or_else(|| old.as_ref().map(|row| row.visibility.as_str()))
        .unwrap_or("private");
    if !matches!(visibility, "all" | "private" | "selected") {
        return Err(ValueError::Visibility);
    }
    if input.repositories.is_some() && visibility != "selected" {
        return Err(ValueError::Visibility);
    }
    let value = input
        .value
        .or_else(|| old.as_ref().map(|row| row.value.as_str()))
        .ok_or(ValueError::InvalidValue)?;
    let (plain, encrypted, nonce) = if input.kind == Kind::Secret {
        if input.value.is_none() {
            let row = old.as_ref().ok_or(ValueError::NotFound)?;
            ("", row.encrypted_value.clone(), row.nonce.clone())
        } else {
            let (encrypted, nonce) = encryptor
                .ok_or(SecretError::InvalidMasterKey)?
                .encrypt(value.as_bytes())?;
            ("", Some(encrypted), Some(nonce.to_vec()))
        }
    } else {
        (value, None, None)
    };
    if input.name != previous {
        sqlx::query(
            "UPDATE org_actions_values SET name=$4 WHERE org_id=$1 AND kind=$2 AND name=$3",
        )
        .bind(org)
        .bind(kind)
        .bind(previous)
        .bind(input.name)
        .execute(&mut *tx)
        .await?;
    }
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO org_actions_values(org_id,kind,name,value,encrypted_value,nonce,visibility,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$8) ON CONFLICT(org_id,kind,name) DO UPDATE SET value=excluded.value,encrypted_value=excluded.encrypted_value,nonce=excluded.nonce,visibility=excluded.visibility,updated_at=excluded.updated_at")
        .bind(org).bind(kind).bind(input.name).bind(plain).bind(encrypted).bind(nonce).bind(visibility).bind(now).execute(&mut *tx).await?;
    if visibility != "selected" {
        replace_repositories(&mut tx, org, input.kind, input.name, &[]).await?;
    } else if let Some(ids) = input.repositories {
        replace_repositories(&mut tx, org, input.kind, input.name, ids).await?;
    }
    tx.commit().await?;
    Ok(old.is_none())
}

pub async fn delete(
    pool: &Pool,
    actor: i64,
    org: i64,
    kind: Kind,
    name: &str,
) -> Result<(), ValueError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, org).await?;
    let result =
        sqlx::query("DELETE FROM org_actions_values WHERE org_id=$1 AND kind=$2 AND name=$3")
            .bind(org)
            .bind(kind.as_str())
            .bind(name)
            .execute(&mut *tx)
            .await?;
    if result.rows_affected() == 0 {
        return Err(ValueError::NotFound);
    }
    tx.commit().await?;
    Ok(())
}

async fn selected(
    connection: &mut Connection,
    org: i64,
    kind: Kind,
    name: &str,
) -> Result<(), ValueError> {
    let visibility: Option<String> = sqlx::query_scalar(
        "SELECT visibility FROM org_actions_values WHERE org_id=$1 AND kind=$2 AND name=$3",
    )
    .bind(org)
    .bind(kind.as_str())
    .bind(name)
    .fetch_optional(connection)
    .await?;
    match visibility.as_deref() {
        Some("selected") => Ok(()),
        Some(_) => Err(ValueError::NotSelected),
        None => Err(ValueError::NotFound),
    }
}

pub async fn repositories(
    pool: &Pool,
    actor: i64,
    org: i64,
    kind: Kind,
    name: &str,
) -> Result<Vec<crate::backend::db::models::Repository>, ValueError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, org).await?;
    selected(&mut tx, org, kind, name).await?;
    let rows = sqlx::query_as("SELECT r.* FROM repositories r JOIN org_actions_value_repositories s ON s.repo_id=r.id AND s.org_id=r.owner_org_id WHERE s.org_id=$1 AND s.kind=$2 AND s.name=$3 AND r.owner_kind='org' ORDER BY r.id").bind(org).bind(kind.as_str()).bind(name).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn select(
    pool: &Pool,
    actor: i64,
    org: i64,
    kind: Kind,
    name: &str,
    change: Selection<'_>,
) -> Result<(), ValueError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, org).await?;
    selected(&mut tx, org, kind, name).await?;
    match change {
        Selection::Set(ids) => replace_repositories(&mut tx, org, kind, name, ids).await?,
        Selection::Add(id) => {
            valid_repositories(&mut tx, org, &[id]).await?;
            sqlx::query("INSERT INTO org_actions_value_repositories(org_id,kind,name,repo_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(org).bind(kind.as_str()).bind(name).bind(id).execute(&mut *tx).await?;
        }
        Selection::Remove(id) => {
            valid_repositories(&mut tx, org, &[id]).await?;
            sqlx::query("DELETE FROM org_actions_value_repositories WHERE org_id=$1 AND kind=$2 AND name=$3 AND repo_id=$4").bind(org).bind(kind.as_str()).bind(name).bind(id).execute(&mut *tx).await?;
        }
    }
    sqlx::query(
        "UPDATE org_actions_values SET updated_at=$4 WHERE org_id=$1 AND kind=$2 AND name=$3",
    )
    .bind(org)
    .bind(kind.as_str())
    .bind(name)
    .bind(chrono::Utc::now())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn inherited(
    pool: &Pool,
    actor: i64,
    repo: i64,
    kind: Kind,
) -> Result<Vec<OrganizationValue>, ValueError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let allowed = if kind == Kind::Secret {
        super::super::discussions::lock_admin_authority(&mut tx, actor, repo).await
    } else {
        super::super::discussions::lock_contribution_authority(&mut tx, actor, repo).await
    }
    .map_err(ValueError::Authority)?;
    if !allowed {
        return Err(ValueError::Denied);
    }
    let rows = sqlx::query_as("SELECT v.name,v.kind,v.value,v.visibility,v.created_at,v.updated_at FROM org_actions_values v JOIN repositories r ON r.owner_org_id=v.org_id AND r.owner_kind='org' WHERE r.id=$1 AND v.kind=$2 AND (v.visibility='all' OR (v.visibility='private' AND r.is_private) OR (v.visibility='selected' AND EXISTS(SELECT 1 FROM org_actions_value_repositories s WHERE s.org_id=v.org_id AND s.kind=v.kind AND s.name=v.name AND s.repo_id=r.id))) ORDER BY v.name")
        .bind(repo).bind(kind.as_str()).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn workflow_values(
    pool: &Pool,
    repo: i64,
    kind: Kind,
) -> Result<Vec<StoredValue>, ValueError> {
    Ok(sqlx::query_as("SELECT v.name,v.value,v.visibility,v.encrypted_value,v.nonce FROM org_actions_values v JOIN repositories r ON r.owner_org_id=v.org_id AND r.owner_kind='org' WHERE r.id=$1 AND v.kind=$2 AND (v.visibility='all' OR (v.visibility='private' AND r.is_private) OR (v.visibility='selected' AND EXISTS(SELECT 1 FROM org_actions_value_repositories s WHERE s.org_id=v.org_id AND s.kind=v.kind AND s.name=v.name AND s.repo_id=r.id))) ORDER BY v.name LIMIT $3")
        .bind(repo).bind(kind.as_str()).bind(if kind==Kind::Secret {100i64} else {1000i64}).fetch_all(pool).await?)
}
