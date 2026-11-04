//! Durable object placement. Copy readiness is published only after checked bytes are durable.
use anyhow::Result;
use codeyard_db_core::{native_operation, DbPool};
#[derive(sqlx::FromRow, Clone, Debug)]
pub struct Object {
    pub object_key: String,
    pub digest: String,
    pub bytes: i64,
    pub content_type: String,
    pub confirmed: i32,
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
