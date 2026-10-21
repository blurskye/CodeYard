//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::NodeConfiguration;
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::node_config::get_node_configuration,
    crate::backend::db::sqlite::node_config::get_node_configuration,
    fn get_node_configuration(pool: &DbPool) -> Result<Option<NodeConfiguration>>);

native_operation!(crate::backend::db::postgres::node_config::upsert_node_configuration,
    crate::backend::db::sqlite::node_config::upsert_node_configuration,
    fn upsert_node_configuration(
    pool: &DbPool,
    node_mode: &str,
    setup_completed: bool,
) -> Result<NodeConfiguration>);

native_operation!(crate::backend::db::postgres::node_config::set_ai_crawlers,
    crate::backend::db::sqlite::node_config::set_ai_crawlers,
    fn set_ai_crawlers(pool: &DbPool, actor: i64, session: &str, value: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::node_config::set_node_mode,
    crate::backend::db::sqlite::node_config::set_node_mode,
    fn set_node_mode(pool: &DbPool, actor: i64, session: &str, value: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::node_config::get_cluster_ssh_host_key,
    crate::backend::db::sqlite::node_config::get_cluster_ssh_host_key,
    fn get_cluster_ssh_host_key(pool: &DbPool) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::node_config::insert_cluster_ssh_host_key,
    crate::backend::db::sqlite::node_config::insert_cluster_ssh_host_key,
    fn insert_cluster_ssh_host_key(pool: &DbPool, private_key_pem: &str) -> Result<()>);
