//! Uploaded file records, upload quotas and download accounting.
use crate::backend::db::models::{
    File, FileReason, FileReasonLimit, FileVisibility, UserFileStats,
};
use crate::backend::db::DbPool;
use std::net::IpAddr;
use uuid::Uuid;

pub const MAX_UPLOAD_BYTES: i64 = 100 * 1024 * 1024;

/// How long a client has to send the bytes after reserving an upload.
pub const PUT_LIFETIME_SECONDS: i64 = 900;

/// A PUT accepted before its deadline can keep transferring after it, and a replayed
/// PUT can recreate bytes that were already removed. Deleted files' bytes therefore
/// stay queued this long past the deadline before cleanup removes them.
pub const IN_FLIGHT_PUT_GRACE_SECONDS: i64 = 3600;

#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    #[error("Invalid upload declaration")]
    Invalid,
    #[error("Upload quota exhausted")]
    Quota,
    #[error("Upload policy unavailable")]
    PolicyUnavailable,
    #[error("Upload database unavailable")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub enum DeleteFileOutcome {
    Deleted(Box<File>),
    NotFound,
    ReferencedOrganizationAvatar,
}

#[derive(Debug, Clone)]
pub enum DeleteUnreferencedOutcome {
    Deleted(Box<File>),
    NotFound,
    Referenced,
}

pub struct ReserveUpload<'a> {
    pub user_id: i64,
    pub filename: &'a str,
    pub content_type: &'a str,
    pub file_size: i64,
    pub reason: FileReason,
    pub visibility: FileVisibility,
    pub sha256_hex: &'a str,
}

impl ReserveUpload<'_> {
    /// Basic shape of the declaration, before any policy lookup.
    pub(crate) fn valid(&self) -> bool {
        (1..=MAX_UPLOAD_BYTES).contains(&self.file_size)
            && !self.filename.is_empty()
            && self.filename.len() <= 255
            && !self.filename.contains('\0')
            && !self.content_type.is_empty()
            && self.content_type.len() <= 100
            && self.content_type.contains('/')
            && self
                .content_type
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/-+.".contains(&c))
            && self.sha256_hex.len() == 64
            && self
                .sha256_hex
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            // Only avatars, of users and of organizations, may be public; other
            // uploads are visible to their owner.
            && (self.visibility != FileVisibility::Public
                || matches!(self.reason, FileReason::Avatar | FileReason::OrgAvatar))
    }

    /// Size and content type rules configured for this upload reason.
    pub(crate) fn allowed_by(&self, limits: &FileReasonLimit) -> bool {
        self.file_size <= limits.max_file_size_bytes
            && limits.max_uploads_per_hour > 0
            && limits.max_uploads_per_day > 0
            && limits.max_bytes_per_hour > 0
            && limits.max_bytes_per_day > 0
            && limits
                .allowed_content_types
                .as_ref()
                .is_none_or(|types| types.iter().any(|t| t == self.content_type))
            && (!matches!(self.reason, FileReason::Avatar | FileReason::OrgAvatar)
                || matches!(
                    self.content_type,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ))
    }
}

native_operation!(crate::backend::db::postgres::files::reserve_upload,
    crate::backend::db::sqlite::files::reserve_upload,
    fn reserve_upload(pool: &DbPool, req: ReserveUpload<'_>) -> Result<File, AdmissionError>);

native_operation!(crate::backend::db::postgres::files::verify_pending_upload,
    crate::backend::db::sqlite::files::verify_pending_upload,
    fn verify_pending_upload(pool: &DbPool, file_id: Uuid, user_id: i64) -> Result<bool, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::activate_upload,
    crate::backend::db::sqlite::files::activate_upload,
    fn activate_upload(pool: &DbPool, file_id: Uuid, user_id: i64) -> Result<bool, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::extend_direct_upload,
    crate::backend::db::sqlite::files::extend_direct_upload,
    fn extend_direct_upload(pool: &DbPool, file_id: Uuid, user_id: i64, seconds: i64) -> Result<bool, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::delete_file,
    crate::backend::db::sqlite::files::delete_file,
    fn delete_file(pool: &DbPool, file_id: Uuid, user_id: i64) -> Result<DeleteFileOutcome, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::delete_unreferenced_file,
    crate::backend::db::sqlite::files::delete_unreferenced_file,
    fn delete_unreferenced_file(pool: &DbPool, file_id: Uuid, user_id: i64) -> Result<DeleteUnreferencedOutcome, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::expired_uploads,
    crate::backend::db::sqlite::files::expired_uploads,
    fn expired_uploads(pool: &DbPool, limit: i64) -> Result<Vec<File>, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::delete_expired_upload,
    crate::backend::db::sqlite::files::delete_expired_upload,
    fn delete_expired_upload(pool: &DbPool, file_id: Uuid) -> Result<Option<File>, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::due_object_deletions,
    crate::backend::db::sqlite::files::due_object_deletions,
    fn due_object_deletions(pool: &DbPool, limit: i64) -> Result<Vec<String>, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::finish_object_deletion,
    crate::backend::db::sqlite::files::finish_object_deletion,
    fn finish_object_deletion(pool: &DbPool, object_key: &str) -> Result<bool, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::postpone_object_deletion,
    crate::backend::db::sqlite::files::postpone_object_deletion,
    fn postpone_object_deletion(pool: &DbPool, object_key: &str, seconds: i64) -> Result<(), sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::get_file_by_id,
    crate::backend::db::sqlite::files::get_file_by_id,
    fn get_file_by_id(pool: &DbPool, file_id: Uuid) -> Result<Option<File>, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::get_user_file,
    crate::backend::db::sqlite::files::get_user_file,
    fn get_user_file(pool: &DbPool, file_id: Uuid, user_id: i64) -> Result<Option<File>, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::get_downloadable_file,
    crate::backend::db::sqlite::files::get_downloadable_file,
    fn get_downloadable_file(pool: &DbPool, file_id: Uuid, actor: Option<i64>) -> Result<Option<File>, sqlx::Error>);

// Retain upload charges for the maximum 730-day admin report and downloads for
// 30 days. Each sweep removes at most 20 batches of 500 rows per table, so a backlog
// drains over several sweeps, preserving quota windows.
native_operation!(crate::backend::db::postgres::files::remove_expired_transfer_rows,
    crate::backend::db::sqlite::files::remove_expired_transfer_rows,
    fn remove_expired_transfer_rows(pool: &DbPool) -> Result<u64, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::record_download,
    crate::backend::db::sqlite::files::record_download,
    fn record_download(
    pool: &DbPool,
    user_id: i64,
    file_id: Uuid,
    reason: Option<&str>,
    ip: Option<IpAddr>,
) -> Result<(), sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::get_user_file_stats,
    crate::backend::db::sqlite::files::get_user_file_stats,
    fn get_user_file_stats(pool: &DbPool, user_id: i64) -> Result<Option<UserFileStats>, sqlx::Error>);

native_operation!(crate::backend::db::postgres::files::get_user_total_storage,
    crate::backend::db::sqlite::files::get_user_total_storage,
    fn get_user_total_storage(pool: &DbPool, user_id: i64) -> Result<i64, sqlx::Error>);
