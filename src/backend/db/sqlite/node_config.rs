use super::models::NodeConfiguration;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn get_node_configuration(pool: &SqlitePool) -> Result<Option<NodeConfiguration>> {
    let config =
        sqlx::query_as::<_, NodeConfiguration>("SELECT * FROM node_configuration WHERE id = 1")
            .fetch_optional(pool)
            .await?;

    Ok(config)
}

pub async fn upsert_node_configuration(
    pool: &SqlitePool,
    node_mode: &str,
    setup_completed: bool,
) -> Result<NodeConfiguration> {
    let config = sqlx::query_as::<_, NodeConfiguration>(
        r#"
        INSERT INTO node_configuration (id, node_mode, setup_completed)
        VALUES (1, ?1, ?2)
        ON CONFLICT (id) DO UPDATE SET
            node_mode = EXCLUDED.node_mode,
            setup_completed = EXCLUDED.setup_completed,
            updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
        RETURNING *
        "#,
    )
    .bind(node_mode)
    .bind(setup_completed)
    .fetch_one(pool)
    .await?;

    Ok(config)
}

/// Returns false when the node is not set up yet, so there is no row to change.
pub async fn set_ai_crawlers(pool: &SqlitePool, value: &str) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE node_configuration SET ai_crawlers = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = 1",
    )
    .bind(value)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn get_cluster_ssh_host_key(pool: &SqlitePool) -> Result<Option<String>> {
    let key = sqlx::query_scalar::<_, String>(
        "SELECT private_key_pem FROM cluster_ssh_host_key WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(key)
}

pub async fn insert_cluster_ssh_host_key(pool: &SqlitePool, private_key_pem: &str) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO cluster_ssh_host_key (id, private_key_pem)
        VALUES (1, ?1)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(private_key_pem)
    .execute(pool)
    .await?;

    Ok(())
}
