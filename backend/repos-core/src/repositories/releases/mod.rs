use crate::repositories::refs::RefError;
use codeyard_git as git;
use codeyard_git::error::RepoError;
use codeyard_identity::repository_access::AccessError;
use codeyard_storage::files::downloads::DownloadError;
use codeyard_storage::files::uploads::UploadError;
use codeyard_db_repos::releases::ReleaseFields;
pub mod immutable;
#[derive(Debug)]
pub enum ReleaseError {
    Access(AccessError),
    Invalid(&'static str),
    NotFound,
    /// The tag is already a release.
    TagTaken,
    /// The release already has an asset with this name.
    AssetNameTaken,
    Conflict,
    /// The tag is new and no branch or commit was given to create it on.
    TargetRequired,
    Ref(RefError),
    Git(RepoError),
    Upload(UploadError),
    Download(DownloadError),
    Unavailable,
}

impl From<AccessError> for ReleaseError {
    fn from(error: AccessError) -> Self {
        Self::Access(error)
    }
}
impl From<RefError> for ReleaseError {
    fn from(error: RefError) -> Self {
        match error {
            RefError::Protected(message)
                if message == crate::repositories::releases::immutable::TAG_PROTECTED =>
            {
                Self::Invalid(message)
            }
            error => Self::Ref(error),
        }
    }
}

pub fn unavailable<E>(_: E) -> ReleaseError {
    ReleaseError::Unavailable
}

pub fn validate_fields(fields: &ReleaseFields) -> Result<(), ReleaseError> {
    if fields
        .name
        .as_deref()
        .is_some_and(|text| text.contains('\0'))
        || fields
            .body
            .as_deref()
            .is_some_and(|text| text.contains('\0'))
    {
        return Err(ReleaseError::Invalid(
            "Release text cannot contain NUL characters",
        ));
    }
    if fields.name.as_deref().is_some_and(|name| name.len() > 255) {
        return Err(ReleaseError::Invalid(
            "Release titles are limited to 255 bytes",
        ));
    }
    if fields
        .body
        .as_deref()
        .is_some_and(|body| body.len() > 65536)
    {
        return Err(ReleaseError::Invalid(
            "Release notes are limited to 65536 bytes",
        ));
    }
    Ok(())
}

pub fn validate_tag(tag: &str) -> Result<(), ReleaseError> {
    if tag.is_empty() || tag.len() > 255 || git::tag_ref(tag).is_none() {
        return Err(ReleaseError::Invalid("Tag name is not valid"));
    }
    Ok(())
}

