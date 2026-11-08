use codeyard_db_core::models::File;
use codeyard_db_core::models::FileReason;
use codeyard_db_core::models::FileReasonLimit;
use codeyard_db_core::models::FileVisibility;
use codeyard_db_core::models::UserFileStats;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use std::net::IpAddr;
use uuid::Uuid;

/// Request entry points retain their session; trusted internal file work carries an account.
#[derive(Clone)]
pub struct FileAuthority {
    pub user_id: Option<i64>,
    pub session: Option<String>,
}
impl From<i64> for FileAuthority {
    fn from(user_id: i64) -> Self {
        Self {
            user_id: Some(user_id),
            session: None,
        }
    }
}
impl From<Option<i64>> for FileAuthority {
    fn from(user_id: Option<i64>) -> Self {
        Self {
            user_id,
            session: None,
        }
    }
}
impl From<(i64, String)> for FileAuthority {
    fn from((user_id, session): (i64, String)) -> Self {
        Self {
            user_id: Some(user_id),
            session: Some(session),
        }
    }
}
impl From<(Option<i64>, Option<String>)> for FileAuthority {
    fn from((user_id, session): (Option<i64>, Option<String>)) -> Self {
        Self { user_id, session }
    }
}

pub const MAX_UPLOAD_BYTES: i64 = codeyard_types::object_limits::MAX_OBJECT_BYTES;

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
    Denied,
    ReferencedOrganizationAvatar,
    ReferencedReleaseAsset,
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
    pub fn valid(&self) -> bool {
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
                || codeyard_types::media::AVATAR_CONTENT_TYPES.contains(&self.content_type))
    }
}

native_operation!(crate::postgres::files::reserve_upload,
    crate::sqlite::files::reserve_upload,
    fn reserve_upload(pool: &DbPool, req: ReserveUpload<'_>) -> Result<File, AdmissionError>);

native_operation!(crate::postgres::files::charge_upload,
    crate::sqlite::files::charge_upload,
    fn charge_upload(pool: &DbPool, user: i64, reason: &str, size: i64) -> Result<Uuid, AdmissionError>);

native_operation!(crate::postgres::files::verify_pending_upload,
    crate::sqlite::files::verify_pending_upload,
    fn verify_pending_upload(pool: &DbPool, file_id: Uuid, user_id: i64) -> Result<bool, sqlx::Error>);

native_operation!(crate::postgres::files::can_publish_upload,
    crate::sqlite::files::can_publish_upload,
    fn can_publish_upload(pool: &DbPool, file_id: Uuid, actor: FileAuthority) -> Result<bool, sqlx::Error>);

native_operation!(crate::postgres::files::activate_upload,
    crate::sqlite::files::activate_upload,
    fn activate_upload(pool: &DbPool, file_id: Uuid, actor: impl Into<FileAuthority>) -> Result<bool, sqlx::Error>);

native_operation!(crate::postgres::files::extend_direct_upload,
    crate::sqlite::files::extend_direct_upload,
    fn extend_direct_upload(pool: &DbPool, file_id: Uuid, user_id: i64, seconds: i64) -> Result<bool, sqlx::Error>);

native_operation!(crate::postgres::files::delete_file,
    crate::sqlite::files::delete_file,
    fn delete_file(pool: &DbPool, file_id: Uuid, actor: impl Into<FileAuthority>) -> Result<DeleteFileOutcome, sqlx::Error>);

native_operation!(crate::postgres::files::delete_unreferenced_file,
    crate::sqlite::files::delete_unreferenced_file,
    fn delete_unreferenced_file(pool: &DbPool, file_id: Uuid, user_id: i64) -> Result<DeleteUnreferencedOutcome, sqlx::Error>);

native_operation!(crate::postgres::files::expired_uploads,
    crate::sqlite::files::expired_uploads,
    fn expired_uploads(pool: &DbPool, limit: i64) -> Result<Vec<File>, sqlx::Error>);

native_operation!(crate::postgres::files::delete_expired_upload,
    crate::sqlite::files::delete_expired_upload,
    fn delete_expired_upload(pool: &DbPool, file_id: Uuid) -> Result<Option<File>, sqlx::Error>);

native_operation!(crate::postgres::files::due_object_deletions,
    crate::sqlite::files::due_object_deletions,
    fn due_object_deletions(pool: &DbPool, limit: i64) -> Result<Vec<String>, sqlx::Error>);

native_operation!(crate::postgres::files::finish_object_deletion,
    crate::sqlite::files::finish_object_deletion,
    fn finish_object_deletion(pool: &DbPool, object_key: &str) -> Result<bool, sqlx::Error>);

native_operation!(crate::postgres::files::postpone_object_deletion,
    crate::sqlite::files::postpone_object_deletion,
    fn postpone_object_deletion(pool: &DbPool, object_key: &str, seconds: i64) -> Result<(), sqlx::Error>);

native_operation!(crate::postgres::files::get_file_by_id,
    crate::sqlite::files::get_file_by_id,
    fn get_file_by_id(pool: &DbPool, file_id: Uuid) -> Result<Option<File>, sqlx::Error>);

/// The file, only when `actor_id` uploaded it.
pub async fn get_owned_file(
    pool: &DbPool,
    file_id: Uuid,
    actor_id: i64,
) -> Result<Option<File>, sqlx::Error> {
    Ok(get_file_by_id(pool, file_id)
        .await?
        .filter(|file| file.user_id == Some(actor_id)))
}

native_operation!(crate::postgres::files::get_user_file,
    crate::sqlite::files::get_user_file,
    fn get_user_file(pool: &DbPool, file_id: Uuid, user_id: i64) -> Result<Option<File>, sqlx::Error>);

native_operation!(crate::postgres::files::get_downloadable_file,
    crate::sqlite::files::get_downloadable_file,
    fn get_downloadable_file(pool: &DbPool, file_id: Uuid, actor: impl Into<FileAuthority>) -> Result<Option<File>, sqlx::Error>);

/// Upload charges are kept for the longest admin report window and download
/// charges for 30 days. Each sweep removes at most `SWEEP_BATCHES` batches of
/// `SWEEP_BATCH` rows per table, so a backlog drains over several sweeps and
/// quota windows stay intact.
pub(crate) const UPLOAD_CHARGE_DAYS: i64 = 730;
pub(crate) const DOWNLOAD_CHARGE_DAYS: i64 = 30;
pub(crate) const SWEEP_BATCH: u64 = 500;
pub(crate) const SWEEP_BATCHES: usize = 20;

native_operation!(crate::postgres::files::remove_expired_transfer_rows,
    crate::sqlite::files::remove_expired_transfer_rows,
    fn remove_expired_transfer_rows(pool: &DbPool) -> Result<u64, sqlx::Error>);

native_operation!(crate::postgres::files::record_download,
    crate::sqlite::files::record_download,
    fn record_download(
    pool: &DbPool,
    user_id: i64,
    file_id: Uuid,
    reason: Option<&str>,
    ip: Option<IpAddr>,
) -> Result<(), sqlx::Error>);

native_operation!(crate::postgres::files::get_user_file_stats,
    crate::sqlite::files::get_user_file_stats,
    fn get_user_file_stats(pool: &DbPool, user_id: i64) -> Result<Option<UserFileStats>, sqlx::Error>);

native_operation!(crate::postgres::files::get_user_total_storage,
    crate::sqlite::files::get_user_total_storage,
    fn get_user_total_storage(pool: &DbPool, user_id: i64) -> Result<i64, sqlx::Error>);
