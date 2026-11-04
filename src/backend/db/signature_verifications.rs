//! Successful verification belongs to the signed bytes, not the lifetime of an enrolled key.
use anyhow::Result;
use chrono::{DateTime, Utc};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

native_operation!(crate::backend::db::postgres::signature_verifications::verified_at,
    crate::backend::db::sqlite::signature_verifications::verified_at,
    fn verified_at(pool: &DbPool, input_hash: &str) -> Result<Option<DateTime<Utc>>>);

native_operation!(crate::backend::db::postgres::signature_verifications::record_verified,
    crate::backend::db::sqlite::signature_verifications::record_verified,
    fn record_verified(pool: &DbPool, input_hash: &str) -> Result<DateTime<Utc>>);
