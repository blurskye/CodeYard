//! Durable object placement. Copy readiness is published only after checked bytes are durable.
use anyhow::Result;
use codeyard_database::{native_operation, DbPool};
#[derive(sqlx::FromRow, Clone, Debug)]
pub struct Object {
    pub object_key: String,
    pub digest: String,
    pub bytes: i64,
    pub content_type: String,
    pub confirmed: i32,
}
/// Choose both durable destinations using the same reserve as Git placement and drain.
pub(crate) fn targets(
    nodes: &[codeyard_database::models::ClusterNode],
    copies: &[(i64, i32)],
    pending: &[(i64, i64)],
    bytes: i64,
) -> Result<Vec<i64>> {
    let mut choices = Vec::new();
    for node in nodes
        .iter()
        .filter(|node| node.state == "active" && node.is_serving_storage())
    {
        let current = copies.iter().find(|copy| copy.0 == node.id);
        if current.is_some_and(|copy| copy.1 < 0) || node.disk_total.is_none() {
            continue;
        }
        let Some(available) = node.available_storage_bytes() else {
            continue;
        };
        let reserved = pending
            .iter()
            .find(|row| row.0 == node.id)
            .map_or(0, |row| row.1);
        let needed = if current.is_some() {
            0
        } else {
            bytes.max(1).saturating_mul(2)
        };
        if available.saturating_sub(reserved) >= needed {
            choices.push((
                current.is_some_and(|copy| copy.1 == 1),
                node.disk_free.unwrap_or_default(),
                node.id,
            ));
        }
    }
    choices.sort_by_key(|(ready, free, id)| {
        (std::cmp::Reverse(*ready), std::cmp::Reverse(*free), *id)
    });
    anyhow::ensure!(
        choices.len() >= 2,
        "two storage nodes with spare capacity are required"
    );
    Ok(choices.into_iter().take(2).map(|(_, _, id)| id).collect())
}

native_operation!(crate::postgres::objects::get, crate::sqlite::objects::get,
    fn get(pool: &DbPool, key: &str) -> Result<Option<Object>>);
native_operation!(crate::postgres::objects::copies, crate::sqlite::objects::copies,
    fn copies(pool: &DbPool, key: &str) -> Result<Vec<(i64,i32)>>);
native_operation!(crate::postgres::objects::reserve, crate::sqlite::objects::reserve,
    fn reserve(pool: &DbPool, owner: i64, public_key: &str, object: &Object) -> Result<Vec<i64>>);
native_operation!(crate::postgres::objects::authorize, crate::sqlite::objects::authorize,
    fn authorize(pool: &DbPool, key: &str, node: i64, server_key: &str, caller: &str, write: bool, delete: bool) -> Result<Object>);
native_operation!(crate::postgres::objects::ready, crate::sqlite::objects::ready,
    fn ready(pool: &DbPool, key: &str, node: i64, server_key: &str, caller: &str) -> Result<()>);
native_operation!(crate::postgres::objects::confirm, crate::sqlite::objects::confirm,
    fn confirm(pool: &DbPool, owner: i64, public_key: &str, key: &str) -> Result<()>);
native_operation!(crate::postgres::objects::begin_delete, crate::sqlite::objects::begin_delete,
    fn begin_delete(pool: &DbPool, owner: i64, public_key: &str, key: &str) -> Result<bool>);
native_operation!(crate::postgres::objects::forget_copy, crate::sqlite::objects::forget_copy,
    fn forget_copy(pool: &DbPool, key: &str, node: i64, server_key: &str, caller: &str) -> Result<()>);
native_operation!(crate::postgres::objects::finish_delete, crate::sqlite::objects::finish_delete,
    fn finish_delete(pool: &DbPool, key: &str) -> Result<()>);

native_operation!(crate::postgres::objects::work, crate::sqlite::objects::work,
    fn work(pool:&DbPool,node:i64)->Result<Vec<(String,i32)>>);
native_operation!(crate::postgres::objects::retire, crate::sqlite::objects::retire,
    fn retire(pool:&DbPool,node:i64,public_key:&str,key:&str)->Result<bool>);

native_operation!(crate::postgres::objects::purge, crate::sqlite::objects::purge,
    fn purge(pool:&DbPool)->Result<()>);
