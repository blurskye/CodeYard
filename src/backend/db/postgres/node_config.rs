use super::models::NodeConfiguration;
use anyhow::Result;
use sqlx::PgPool;

pub async fn get_node_configuration(pool: &PgPool) -> Result<Option<NodeConfiguration>> {
    let config =
        sqlx::query_as::<_, NodeConfiguration>("SELECT * FROM node_configuration WHERE id = 1")
            .fetch_optional(pool)
            .await?;

    Ok(config)
}

pub async fn upsert_node_configuration(
    pool: &PgPool,
    node_mode: &str,
    setup_completed: bool,
) -> Result<NodeConfiguration> {
    let config = sqlx::query_as::<_, NodeConfiguration>(
        r#"
        INSERT INTO node_configuration (id, node_mode, setup_completed)
        VALUES (1, $1, $2)
        ON CONFLICT (id) DO UPDATE SET
            node_mode = EXCLUDED.node_mode,
            setup_completed = EXCLUDED.setup_completed,
            updated_at = NOW()
        RETURNING *
        "#,
    )
    .bind(node_mode)
    .bind(setup_completed)
    .fetch_one(pool)
    .await?;

    Ok(config)
}

pub async fn get_cluster_ssh_host_key(pool: &PgPool) -> Result<Option<String>> {
    let key = sqlx::query_scalar::<_, String>(
        "SELECT private_key_pem FROM cluster_ssh_host_key WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(key)
}

pub async fn insert_cluster_ssh_host_key(pool: &PgPool, private_key_pem: &str) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO cluster_ssh_host_key (id, private_key_pem)
        VALUES (1, $1)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(private_key_pem)
    .execute(pool)
    .await?;

    Ok(())
}

/// Returns false when the node is not set up yet, so there is no row to change.
pub async fn set_ai_crawlers(
    pool: &PgPool,
    actor: i64,
    session: &str,
    value: &str,
) -> Result<bool> {
    set_setting(pool, actor, session, "ai_crawlers", value).await
}

/// `public` or `private`. Returns false when the node is not set up yet.
pub async fn set_node_mode(pool: &PgPool, actor: i64, session: &str, value: &str) -> Result<bool> {
    set_setting(pool, actor, session, "node_mode", value).await
}

/// Changes one column of the configuration row for a signed-in administrator, and logs it.
async fn set_setting(
    pool: &PgPool,
    actor: i64,
    session: &str,
    column: &'static str,
    value: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    if !super::users::transitions::active_admin(&mut tx, actor).await?
        || !super::users::transitions::live_session(&mut tx, actor, session).await?
    {
        return Ok(false);
    }
    let result = sqlx::query(&format!(
        "UPDATE node_configuration SET {column} = $1, updated_at = NOW() WHERE id = 1"
    ))
    .bind(value)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 1 {
        sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,$2,'node_configuration',1,$3)")
            .bind(actor).bind(format!("node.{column}")).bind(serde_json::json!({column:value})).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}
