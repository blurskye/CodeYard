/// A missing setting means block: the default is to block AI crawlers.
pub const ALLOW: &str = "allow";

/// Whether AI training crawlers are blocked. No setting row means block; an unreadable
/// setting is an error, not allow.
pub async fn ai_crawlers_blocked(pool: &codeyard_db_core::DbPool) -> anyhow::Result<bool> {
    Ok(
        codeyard_db_cluster::node_config::get_node_configuration(pool)
            .await?
            .is_none_or(|config| config.ai_crawlers != ALLOW),
    )
}
