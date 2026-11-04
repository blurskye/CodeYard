//! File uploads (avatars, attachments, assets): reserve, upload, confirm,
//! download and delete. Queries live in `db::files`; bytes in `storage`.

pub mod deletion;
pub mod downloads;
pub mod local_uploads;
pub mod uploads;
