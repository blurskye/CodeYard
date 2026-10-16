//! File uploads (avatars, attachments, assets): reserve, upload, confirm,
//! download and delete. Queries live in `db::files`; bytes in `storage`.
mod deletion;
mod downloads;
mod local_uploads;
mod uploads;

pub use deletion::{
    delete_owned_file, delete_unreferenced_file, remove_expired_uploads, DeleteError, ExpiryError,
};
pub use downloads::{
    download_asset, download_for_user, open_asset_download, AssetDownload, DownloadCapability,
    DownloadError, DownloadTransport, SignedDownload,
};
pub use local_uploads::{put_local_upload, LocalUploadError};
pub use uploads::{
    confirm_upload, reserve_and_sign_upload, store_upload, ConfirmationError, UploadCapability,
    UploadDeclaration, UploadError, UploadReservation,
};
