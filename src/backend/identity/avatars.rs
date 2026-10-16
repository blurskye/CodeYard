//! Profile pictures of users and organizations.
//!
//! A picture is an uploaded file that the account points at. It has to be the
//! account holder's own confirmed upload with the matching reason (`avatar` or
//! `org_avatar`), so nobody can borrow someone else's file or an unrelated one.
//! A picture arriving from a form is stored through the same upload checks and
//! quotas as one uploaded through the files API, then activated when set. The picture
//! an account stops using is removed in the same database transaction as the change.

use crate::backend::{
    db::{
        self,
        models::{FileReason, FileVisibility, User},
        organizations::{self, OrganizationOutcome},
        users::accounts::UserAvatarOutcome,
        DbPool,
    },
    files::{self, UploadDeclaration, UploadError},
    storage::{media_signature_matches, ObjectStore},
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The largest picture, the same limit the files API has for avatars.
pub const MAX_PICTURE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AvatarError {
    #[error("Choose a PNG, JPEG, GIF or WebP picture of at most 2 MB.")]
    BadPicture,
    #[error("Choose one of your own confirmed uploads made for this picture.")]
    NotUsable,
    #[error("Too many pictures were uploaded recently. Try again later.")]
    TooMany,
    #[error("You are not allowed to change this picture.")]
    Denied,
    #[error("This organization was not found.")]
    NotFound,
    #[error("Organization governance is suspended.")]
    Suspended,
    #[error("Pictures cannot be changed right now. Try again later.")]
    Unavailable,
}

/// The type of a picture, read from its first bytes, if it is one avatars may use.
/// A name or a type sent by the browser proves nothing.
pub fn picture_type(bytes: &[u8]) -> Option<&'static str> {
    ["image/png", "image/jpeg", "image/gif", "image/webp"]
        .into_iter()
        .find(|kind| media_signature_matches(kind, bytes))
}

fn unavailable<E>(_: E) -> AvatarError {
    AvatarError::Unavailable
}

fn upload_error(error: UploadError) -> AvatarError {
    match error {
        UploadError::Invalid => AvatarError::BadPicture,
        UploadError::Quota => AvatarError::TooMany,
        _ => AvatarError::Unavailable,
    }
}

/// Stores `bytes` as a verified pending picture; assignment makes it public.
async fn store_picture(
    pool: &DbPool,
    store: &ObjectStore,
    actor: i64,
    reason: FileReason,
    bytes: Vec<u8>,
) -> Result<Uuid, AvatarError> {
    if bytes.is_empty() || bytes.len() > MAX_PICTURE_BYTES {
        return Err(AvatarError::BadPicture);
    }
    let content_type = picture_type(&bytes).ok_or(AvatarError::BadPicture)?;
    let checksum = hex::encode(Sha256::digest(&bytes));
    files::store_upload(
        pool,
        store,
        actor,
        UploadDeclaration {
            filename: "avatar",
            content_type,
            file_size: bytes.len() as i64,
            reason,
            visibility: FileVisibility::Public,
            sha256_hex: &checksum,
        },
        bytes,
    )
    .await
    .map_err(upload_error)
}

/// Attempts early cleanup after assignment fails. Pending expiry covers failures.
async fn discard(pool: &DbPool, store: &ObjectStore, owner: i64, file: Uuid) {
    if files::delete_unreferenced_file(pool, store, owner, file)
        .await
        .is_err()
    {
        tracing::warn!(%file, "could not remove an unused picture");
    }
}

// ----- Users -----

/// Makes one of the user's confirmed `avatar` uploads their picture.
pub async fn set_user_avatar(pool: &DbPool, user: &User, file_id: Uuid) -> Result<(), AvatarError> {
    change_user_avatar(pool, user, Some(file_id)).await
}

/// Stores `bytes` as the user's new picture.
pub async fn upload_user_avatar(
    pool: &DbPool,
    store: &ObjectStore,
    user: &User,
    bytes: Vec<u8>,
) -> Result<(), AvatarError> {
    let file = store_picture(pool, store, user.id, FileReason::Avatar, bytes).await?;
    if let Err(error) = set_user_avatar(pool, user, file).await {
        discard(pool, store, user.id, file).await;
        return Err(error);
    }
    Ok(())
}

/// Takes the user's picture away.
pub async fn remove_user_avatar(pool: &DbPool, user: &User) -> Result<(), AvatarError> {
    change_user_avatar(pool, user, None).await
}

async fn change_user_avatar(
    pool: &DbPool,
    user: &User,
    file: Option<Uuid>,
) -> Result<(), AvatarError> {
    match db::users::accounts::set_user_avatar(pool, user.id, file)
        .await
        .map_err(unavailable)?
    {
        UserAvatarOutcome::Applied(_) => Ok(()),
        UserAvatarOutcome::NotUsable => Err(AvatarError::NotUsable),
        UserAvatarOutcome::Banned => Err(AvatarError::Denied),
    }
}

// ----- Organizations -----

/// Sets (or with `None` removes) an organization's picture. Only its owners and
/// admins may, and a file has to be their own confirmed `org_avatar` upload.
pub async fn set_org_avatar(
    pool: &DbPool,
    actor: &User,
    org: i64,
    file: Option<Uuid>,
) -> Result<(), AvatarError> {
    match organizations::set_organization_avatar(pool, actor.id, org, file)
        .await
        .map_err(unavailable)?
    {
        OrganizationOutcome::Applied(_) => Ok(()),
        OrganizationOutcome::Missing => Err(AvatarError::NotFound),
        OrganizationOutcome::Denied => Err(AvatarError::Denied),
        OrganizationOutcome::Conflict => Err(AvatarError::NotUsable),
        OrganizationOutcome::Suspended => Err(AvatarError::Suspended),
    }
}

/// Stores `bytes` as the organization's new picture.
pub async fn upload_org_avatar(
    pool: &DbPool,
    store: &ObjectStore,
    actor: &User,
    org: i64,
    bytes: Vec<u8>,
) -> Result<(), AvatarError> {
    let file = store_picture(pool, store, actor.id, FileReason::OrgAvatar, bytes).await?;
    if let Err(error) = set_org_avatar(pool, actor, org, Some(file)).await {
        discard(pool, store, actor.id, file).await;
        return Err(error);
    }
    Ok(())
}
